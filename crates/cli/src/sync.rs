//! `sync`: ingest every selected version and apply it to the index, one commit per version.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use zenodotus_core::config::{Config, LibrarySpec, VersionSpec};
use zenodotus_core::ids::{DocKey, LibraryId, Scope};
use zenodotus_core::model::{DocState, Outcome, SourceSpec, VersionChanges, VersionState};
use zenodotus_core::time::{Clock, Timestamp};
use zenodotus_ingest::local::{Limits, LocalSource, collect_local};
use zenodotus_ingest::sectioner::SectionRules;
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

/// Syncs the `selection` of `config`, holding the sync lock throughout.
///
/// # Errors
///
/// [`AppError::Busy`] when another sync runs, [`AppError::Usage`] or [`AppError::Core`] for a selection that is not
/// configured, and [`AppError::Search`] for index failures. Versions that end `partial` or `failed`, and an abandoned
/// rebuild, are only reported.
pub fn sync(
    config: &Config,
    paths: Paths<'_>,
    selection: Selection<'_>,
    clock: &dyn Clock,
) -> Result<SyncReport, AppError> {
    let targets = select(config, selection)?;
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
        report
            .versions
            .push(sync_version(&mut writer, target, config, paths.config, clock)?);
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
fn sync_version(
    writer: &mut StoreWriter,
    target: &Target<'_>,
    config: &Config,
    config_path: &Path,
    clock: &dyn Clock,
) -> Result<VersionReport, AppError> {
    let snapshot = target.snapshot(clock.now());
    let previous = writer.store().doc_states(&snapshot.scope)?;
    match collect(target, config, config_path, &previous, clock) {
        Ok(changes) => {
            let state = writer.apply(&snapshot, &changes)?;
            Ok(VersionReport {
                state,
                upserted: changes.upserts.len(),
                deleted: changes.deletions.len(),
                unchanged: changes.unchanged.len(),
                failed: changes.errors.len(),
            })
        }
        Err(error) => Ok(VersionReport {
            state: writer.record_failure(&snapshot, &error)?,
            upserted: 0,
            deleted: 0,
            unchanged: 0,
            failed: 0,
        }),
    }
}

/// Lists and reads the documents of one version.
fn collect(
    target: &Target<'_>,
    config: &Config,
    config_path: &Path,
    previous: &BTreeMap<DocKey, DocState>,
    clock: &dyn Clock,
) -> Result<VersionChanges, String> {
    let SourceSpec::Local { path, include, exclude } = &target.spec.source else {
        return Err(format!(
            "{} sources are not supported by this build",
            target.spec.source.describe()
        ));
    };
    let root = local_root(config_path, path);
    let source = LocalSource {
        root: &root,
        include,
        exclude,
    };
    let limits = Limits {
        max_bytes: config.settings.fetch.page_max_bytes,
        rules: SectionRules::default(),
    };
    collect_local(&target.scope(), &source, previous, clock, &limits).map_err(|error| error.to_string())
}

/// A local source root: `path` itself when absolute, otherwise taken from the config file's directory.
fn local_root(config_path: &Path, path: &Path) -> PathBuf {
    config_path.parent().unwrap_or(Path::new("")).join(path)
}
