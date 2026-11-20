//! `sync`: ingest every selected version and apply it to the index, one commit per version.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use zenodotus_core::config::{Config, LibrarySpec, VersionSpec};
use zenodotus_core::ids::{DocKey, LibraryId, Scope};
use zenodotus_core::model::{DocState, Outcome, VersionChanges, VersionState};
use zenodotus_core::time::{Clock, Timestamp};
use zenodotus_ingest::fetch::Fetcher;
use zenodotus_ingest::github::GitHub;
use zenodotus_ingest::guard::SystemResolve;
use zenodotus_ingest::sectioner::SectionRules;
use zenodotus_ingest::source::{self, Context};
use zenodotus_search::store::{StoreWriter, discard_rebuilt, swap_rebuilt};

use crate::error::AppError;
use crate::lock::SyncLock;

/// Which versions to sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection<'a> {
    /// Every configured version; scopes no longer configured are removed.
    All,
    /// Every version of one library.
    Library(&'a str),
    /// One version of one library.
    Version(&'a str, &'a str),
    /// Every configured version, into a new index that replaces the current one only if no version fails.
    Rebuild,
}

/// One configured version to sync.
struct Target<'a> {
    library: &'a LibrarySpec,
    spec: &'a VersionSpec,
    list_order: u32,
}

impl Target<'_> {
    fn scope(&self) -> Scope {
        Scope::new(self.library.id.clone(), self.spec.version.clone())
    }

    /// The version record as the config describes it, attempted at `now`; the store carries the sync status over.
    fn snapshot(&self, now: Timestamp) -> VersionState {
        let library = self.library;
        VersionState {
            scope: self.scope(),
            name: library.display_name().to_owned(),
            description: library.description.clone().unwrap_or_default(),
            aliases: library.aliases.clone(),
            homepage: library.homepage.as_ref().map(ToString::to_string),
            license: library.license.clone(),
            source: self.spec.source.describe(),
            default: self.spec.default,
            list_order: self.list_order,
            revision: 0,
            last_success: None,
            last_attempt: now,
            last_verified: None,
            outcome: Outcome::Ok,
            last_error: None,
            document_count: 0,
            section_count: 0,
            resolved_commit: None,
        }
    }
}

/// What one sync of one version did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionReport {
    /// The stored record after the sync.
    pub state: VersionState,
    /// Documents added or changed.
    pub upserted: usize,
    /// Documents removed.
    pub deleted: usize,
    /// Documents confirmed current.
    pub unchanged: usize,
    /// Documents that failed and kept their previous content.
    pub failed: usize,
}

/// What a whole sync did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    /// One report per synced version, in config order.
    pub versions: Vec<VersionReport>,
    /// Scopes removed because they are no longer configured.
    pub pruned: Vec<Scope>,
    /// Whether a rebuild was discarded because a version failed; the previous index is then kept.
    pub abandoned: bool,
}

impl SyncReport {
    /// How many versions ended `partial` or `failed`.
    #[must_use]
    pub fn incomplete(&self) -> usize {
        self.count(|outcome| outcome != Outcome::Ok)
    }

    /// How many versions ended `failed`.
    #[must_use]
    pub fn failed(&self) -> usize {
        self.count(|outcome| outcome == Outcome::Failed)
    }

    fn count(&self, matches: impl Fn(Outcome) -> bool) -> usize {
        self.versions
            .iter()
            .filter(|report| matches(report.state.outcome))
            .count()
    }
}

/// Where a sync reads its config and writes its index.
#[derive(Debug, Clone, Copy)]
pub struct Paths<'a> {
    /// The config file; relative local source paths are taken from its directory.
    pub config: &'a Path,
    /// The data directory.
    pub data_dir: &'a Path,
}

/// What every version of a sync reads its documents with.
#[derive(Clone, Copy)]
pub struct Sources<'a> {
    /// The time source.
    pub clock: &'a dyn Clock,
    /// The GitHub endpoints and token.
    pub github: &'a GitHub,
}

/// What [`sync_version`] needs besides the target.
struct Run<'a> {
    config: &'a Config,
    config_path: &'a Path,
    clock: &'a dyn Clock,
    fetcher: &'a Fetcher,
    github: &'a GitHub,
}

/// Syncs the `selection` of `config`, holding the sync lock throughout.
///
/// # Errors
///
/// [`AppError::Busy`] when another sync runs, [`AppError::Usage`] or [`AppError::Core`] for a selection that is not
/// configured, [`AppError::Ingest`] when the HTTP client cannot be built, and [`AppError::Search`] for index failures.
/// Versions that end `partial` or `failed`, and an abandoned rebuild, are only reported.
pub async fn sync(
    config: &Config,
    paths: Paths<'_>,
    selection: Selection<'_>,
    sources: Sources<'_>,
) -> Result<SyncReport, AppError> {
    let targets = select(config, selection)?;
    let settings = &config.settings;
    let fetcher = Fetcher::new(&settings.fetch, &settings.user_agent, Arc::new(SystemResolve))?;
    let run = Run {
        config,
        config_path: paths.config,
        clock: sources.clock,
        fetcher: &fetcher,
        github: sources.github,
    };
    let _lock = SyncLock::acquire(paths.data_dir)?;
    let rebuild = selection == Selection::Rebuild;
    let mut writer = if rebuild {
        StoreWriter::rebuild(paths.data_dir, &config.settings.index)?
    } else {
        StoreWriter::open(paths.data_dir, &config.settings.index)?
    };
    let mut report = SyncReport::default();
    if matches!(selection, Selection::All | Selection::Rebuild) {
        let keep: Vec<Scope> = targets.iter().map(Target::scope).collect();
        report.pruned = writer.prune(&keep)?;
        if targets.is_empty() && !report.pruned.is_empty() {
            writer.commit()?;
        }
    }
    for target in &targets {
        report.versions.push(sync_version(&mut writer, target, &run).await?);
    }
    writer.finish()?;
    if rebuild {
        report.abandoned = report.failed() > 0;
        if report.abandoned {
            discard_rebuilt(paths.data_dir)?;
        } else {
            swap_rebuilt(paths.data_dir)?;
        }
    }
    Ok(report)
}

/// The configured versions `selection` names, in config order.
fn select<'a>(config: &'a Config, selection: Selection<'_>) -> Result<Vec<Target<'a>>, AppError> {
    let all = config.libraries.iter().flat_map(|library| {
        library.versions.iter().zip(0..).map(move |(spec, list_order)| Target {
            library,
            spec,
            list_order,
        })
    });
    let (id, version) = match selection {
        Selection::All | Selection::Rebuild => return Ok(all.collect()),
        Selection::Library(id) => (id, None),
        Selection::Version(id, version) => (id, Some(version)),
    };
    let id = LibraryId::parse(id)?;
    config.library(&id)?;
    let chosen: Vec<Target<'a>> = all
        .filter(|target| target.library.id == id)
        .filter(|target| version.is_none_or(|version| target.spec.version.as_str() == version))
        .collect();
    if chosen.is_empty() {
        return Err(AppError::Usage(format!(
            "library {id} has no version {} in the config",
            version.unwrap_or_default()
        )));
    }
    Ok(chosen)
}

/// Ingests one version and applies it in one commit; a version whose documents cannot be listed is recorded as
/// `failed` without touching its sections.
async fn sync_version(writer: &mut StoreWriter, target: &Target<'_>, run: &Run<'_>) -> Result<VersionReport, AppError> {
    let snapshot = target.snapshot(run.clock.now());
    let previous = writer.store().doc_states(&snapshot.scope)?;
    let previous_commit = reusable_commit(writer.store().version_state(&snapshot.scope)?, &snapshot.source);
    let collected = collect(target, run, &previous, previous_commit.as_deref()).await;
    settle(writer, &snapshot, collected)
}

/// Stores the outcome of one version: its changes in one commit, or a `failed` record when its documents could not
/// be listed or the commit could not be made. Only a failure to store that record stops the sync.
fn settle(
    writer: &mut StoreWriter,
    snapshot: &VersionState,
    collected: Result<VersionChanges, String>,
) -> Result<VersionReport, AppError> {
    let error = match collected {
        Ok(changes) => match writer.apply(snapshot, &changes) {
            Ok(state) => {
                return Ok(VersionReport {
                    state,
                    upserted: changes.upserts.len(),
                    deleted: changes.deletions.len(),
                    unchanged: changes.unchanged.len(),
                    failed: changes.errors.len(),
                });
            }
            Err(error) => format!("cannot apply the changes: {error}"),
        },
        Err(error) => error,
    };
    Ok(VersionReport {
        state: writer.record_failure(snapshot, &error)?,
        upserted: 0,
        deleted: 0,
        unchanged: 0,
        failed: 0,
    })
}

/// The commit of the last sync, when it succeeded on the same source: an unchanged commit then needs no download.
/// A different source, such as other `paths`, must be read again even at the same commit.
fn reusable_commit(state: Option<VersionState>, source: &str) -> Option<String> {
    state
        .filter(|state| state.outcome == Outcome::Ok && state.source == source)
        .and_then(|state| state.resolved_commit)
}

/// Lists and reads the documents of one version.
async fn collect(
    target: &Target<'_>,
    run: &Run<'_>,
    previous: &BTreeMap<DocKey, DocState>,
    previous_commit: Option<&str>,
) -> Result<VersionChanges, String> {
    let scope = target.scope();
    let settings = &run.config.settings;
    let ctx = Context {
        scope: &scope,
        previous,
        previous_commit,
        clock: run.clock,
        fetcher: run.fetcher,
        settings: &settings.fetch,
        user_agent: &settings.user_agent,
        rules: SectionRules::default(),
        extra_hosts: &target.spec.extra_hosts,
        base_dir: run.config_path.parent().unwrap_or(Path::new("")),
        github: run.github,
    };
    source::collect(&target.spec.source, &ctx)
        .await
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use zenodotus_core::config::IndexSettings;
    use zenodotus_core::ids::{LibraryId, Scope, Version};
    use zenodotus_core::model::Document;
    use zenodotus_core::time::Timestamp;

    use super::*;

    fn state(source: &str, outcome: Outcome, commit: Option<&str>) -> VersionState {
        let scope = Scope::new(LibraryId::parse("lib").unwrap(), Version::parse("1").unwrap());
        VersionState {
            scope,
            name: "lib".to_owned(),
            description: String::new(),
            aliases: Vec::new(),
            homepage: None,
            license: None,
            source: source.to_owned(),
            default: true,
            list_order: 0,
            revision: 1,
            last_success: None,
            last_attempt: Timestamp::from_unix(0),
            last_verified: None,
            outcome,
            last_error: None,
            document_count: 0,
            section_count: 0,
            resolved_commit: commit.map(str::to_owned),
        }
    }

    #[test]
    fn a_change_set_that_cannot_be_applied_is_recorded_as_failed() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut writer = StoreWriter::open(dir.path(), &IndexSettings::default()).unwrap();
        let snapshot = state("local:docs", Outcome::Ok, None);
        let other = Scope::new(LibraryId::parse("other").unwrap(), Version::parse("1").unwrap());
        let foreign = Document {
            state: DocState {
                doc_key: DocKey::new(&other, "a.md").unwrap(),
                etag: None,
                last_modified: None,
                content_hash: "0".repeat(64),
                fetched_at: Timestamp::from_unix(0),
            },
            sections: Vec::new(),
        };
        let changes = VersionChanges {
            upserts: vec![foreign],
            listing_complete: true,
            ..VersionChanges::default()
        };
        let report = settle(&mut writer, &snapshot, Ok(changes)).unwrap();
        assert_eq!(report.state.outcome, Outcome::Failed);
        let error = report.state.last_error.unwrap();
        assert!(error.starts_with("cannot apply the changes: "), "{error}");
        assert_eq!(
            (report.upserted, report.deleted, report.unchanged, report.failed),
            (0, 0, 0, 0)
        );
        let listing = settle(&mut writer, &snapshot, Err("cannot list x: gone".to_owned())).unwrap();
        assert_eq!(listing.state.last_error.as_deref(), Some("cannot list x: gone"));
        let applied = settle(&mut writer, &snapshot, Ok(VersionChanges::default())).unwrap();
        assert_eq!(applied.state.outcome, Outcome::Ok);
        writer.finish().unwrap();
    }

    #[test]
    fn only_a_successful_sync_of_the_same_source_lends_its_commit() {
        let source = "github:a/b (docs)";
        let reuse = |state| reusable_commit(state, source);
        assert_eq!(
            reuse(Some(state(source, Outcome::Ok, Some("c1")))),
            Some("c1".to_owned())
        );
        assert_eq!(
            reuse(Some(state("github:a/b (docs, guide)", Outcome::Ok, Some("c1")))),
            None
        );
        assert_eq!(reuse(Some(state(source, Outcome::Partial, Some("c1")))), None);
        assert_eq!(reuse(Some(state(source, Outcome::Ok, None))), None);
        assert_eq!(reuse(None), None);
    }
}
