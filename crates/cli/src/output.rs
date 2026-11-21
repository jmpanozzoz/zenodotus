//! Everything the commands print, as text or JSON.

use std::fmt::Write as _;

use serde_json::{Value, json};
use zenodotus_core::config::Config;
use zenodotus_core::model::VersionState;
use zenodotus_core::time::Timestamp;

use crate::sync::{SyncReport, VersionReport};

/// A timestamp, or `never`.
fn when(at: Option<Timestamp>) -> String {
    at.map_or_else(|| "never".to_owned(), |at| at.to_string())
}

/// The record of `scope` among `states`.
fn state_of<'a>(states: &'a [VersionState], library: &str, version: &str) -> Option<&'a VersionState> {
    states
        .iter()
        .find(|state| state.scope.library.as_str() == library && state.scope.version.as_str() == version)
}

/// One line per synced version, then one per removed scope.
#[must_use]
pub fn sync_text(report: &SyncReport) -> String {
    let mut out = String::new();
    for version in &report.versions {
        out.push_str(&sync_line(version));
    }
    for scope in &report.pruned {
        let _ = writeln!(out, "{scope}: removed, no longer configured");
    }
    if report.versions.is_empty() && report.pruned.is_empty() {
        out.push_str("Nothing to sync: the config has no libraries.\n");
    }
    out
}

/// The report of one synced version.
fn sync_line(report: &VersionReport) -> String {
    let state = &report.state;
    let mut line = format!(
        "{}: {}, revision {}, {} documents, {} sections",
        state.scope, state.outcome, state.revision, state.document_count, state.section_count
    );
    if report.upserted + report.deleted + report.unchanged + report.failed > 0 {
        let _ = write!(
            line,
            " ({} changed, {} deleted, {} unchanged, {} failed)",
            report.upserted, report.deleted, report.unchanged, report.failed
        );
    }
    line.push('\n');
    if let Some(error) = &state.last_error {
        let _ = writeln!(line, "  last error: {error}");
    }
    line
}

/// The configured libraries and versions with their indexed status.
#[must_use]
pub fn list_text(config: &Config, states: &[VersionState]) -> String {
    if config.libraries.is_empty() {
        return "No libraries configured; add one with `zenodotus add`.\n".to_owned();
    }
    let mut out = String::new();
    for library in &config.libraries {
        let _ = writeln!(out, "{} ({})", library.id, library.display_name());
        for spec in &library.versions {
            let default = if spec.default { " (default)" } else { "" };
            let indexed = state_of(states, library.id.as_str(), spec.version.as_str()).map_or_else(
                || "not synced".to_owned(),
                |state| {
                    format!(
                        "revision {}, {}, last success {}",
                        state.revision,
                        state.outcome,
                        when(state.last_success)
                    )
                },
            );
            let _ = writeln!(
                out,
                "  {}{default}: {}; {indexed}",
                spec.version,
                spec.source.describe()
            );
        }
    }
    out
}

/// [`list_text`] as JSON.
#[must_use]
pub fn list_json(config: &Config, states: &[VersionState]) -> Value {
    let libraries: Vec<Value> = config
        .libraries
        .iter()
        .map(|library| {
            let versions: Vec<Value> = library
                .versions
                .iter()
                .map(|spec| {
                    json!({
                        "version": spec.version.as_str(),
                        "default": spec.default,
                        "source": spec.source.describe(),
                        "synced": state_of(states, library.id.as_str(), spec.version.as_str()),
                    })
                })
                .collect();
            json!({
                "library_id": library.id.as_str(),
                "name": library.display_name(),
                "description": library.description.as_deref().unwrap_or_default(),
                "versions": versions,
            })
        })
        .collect();
    json!({ "libraries": libraries })
}

/// The sync record of every indexed version, then the index size.
#[must_use]
pub fn status_text(states: &[VersionState], index_bytes: u64) -> String {
    let mut out = String::new();
    if states.is_empty() {
        out.push_str("The index is empty; run `zenodotus sync`.\n");
    }
    for state in states {
        let _ = writeln!(
            out,
            "{}: {}, revision {}, {} documents, {} sections",
            state.scope, state.outcome, state.revision, state.document_count, state.section_count
        );
        let _ = writeln!(
            out,
            "  last attempt {}, last success {}, last verified {}",
            state.last_attempt,
            when(state.last_success),
            when(state.last_verified)
        );
        if let Some(commit) = &state.resolved_commit {
            let _ = writeln!(out, "  commit {commit}");
        }
        if let Some(error) = &state.last_error {
            let _ = writeln!(out, "  last error: {error}");
        }
    }
    let _ = writeln!(out, "Index size: {index_bytes} bytes");
    out
}

/// [`status_text`] as JSON.
#[must_use]
pub fn status_json(states: &[VersionState], index_bytes: u64) -> Value {
    json!({ "index_bytes": index_bytes, "versions": states })
}

#[cfg(test)]
mod tests {
    use zenodotus_core::ids::{LibraryId, Scope, Version};
    use zenodotus_core::model::Outcome;

    use super::*;

    fn state(library: &str, version: &str) -> VersionState {
        VersionState {
            scope: Scope::new(LibraryId::parse(library).unwrap(), Version::parse(version).unwrap()),
            name: library.to_owned(),
            description: String::new(),
            aliases: Vec::new(),
            homepage: None,
            license: None,
            source: "local:/docs".to_owned(),
            default: false,
            list_order: 0,
            revision: 2,
            last_success: Some(Timestamp::from_unix(60)),
            last_attempt: Timestamp::from_unix(60),
            last_verified: None,
            outcome: Outcome::Partial,
            last_error: Some("1 document(s) failed; first: a.md: too_large".to_owned()),
            document_count: 3,
            section_count: 7,
            resolved_commit: Some("abc123".to_owned()),
        }
    }

    fn report(upserted: usize) -> VersionReport {
        VersionReport {
            state: state("tokio", "1"),
            upserted,
            deleted: 0,
            unchanged: 0,
            failed: 0,
        }
    }

    #[test]
    fn sync_lines_report_counts_errors_and_removed_scopes() {
        let report = SyncReport {
            versions: vec![report(2), report(0)],
            pruned: vec![state("old", "1").scope],
            abandoned: false,
        };
        assert_eq!(
            sync_text(&report),
            "tokio@1: partial, revision 2, 3 documents, 7 sections (2 changed, 0 deleted, 0 unchanged, 0 failed)\n  \
             last error: 1 document(s) failed; first: a.md: too_large\ntokio@1: partial, revision 2, 3 documents, 7 \
             sections\n  last error: 1 document(s) failed; first: a.md: too_large\nold@1: removed, no longer \
             configured\n"
        );
        assert_eq!(
            sync_text(&SyncReport::default()),
            "Nothing to sync: the config has no libraries.\n"
        );
    }

    fn config() -> Config {
        Config::parse(
            std::path::Path::new("z.toml"),
            r#"
[[library]]
id = "tokio"
name = "Tokio"
description = "Async runtime"
[[library.version]]
version = "1"
default = true
source = { kind = "local", path = "/docs" }
[[library.version]]
version = "2"
source = { kind = "local", path = "/docs2" }
"#,
        )
        .unwrap()
    }

    #[test]
    fn list_shows_each_version_with_its_index_status() {
        let mut synced = state("tokio", "1");
        synced.last_success = None;
        let states = vec![synced, state("other", "2")];
        assert_eq!(
            list_text(&config(), &states),
            "tokio (Tokio)\n  1 (default): local:/docs; revision 2, partial, last success never\n  2: local:/docs2; \
             not synced\n"
        );
        assert_eq!(
            list_text(&Config::default(), &[]),
            "No libraries configured; add one with `zenodotus add`.\n"
        );
        let json = list_json(&config(), &states);
        assert_eq!(json["libraries"][0]["description"], "Async runtime");
        assert_eq!(json["libraries"][0]["versions"][0]["synced"]["revision"], 2);
        assert_eq!(json["libraries"][0]["versions"][1]["synced"], Value::Null);
        assert_eq!(json["libraries"][0]["versions"][1]["default"], false);
    }

    #[test]
    fn status_shows_every_record_and_the_index_size() {
        let mut clean = state("axum", "0.8");
        clean.last_error = None;
        clean.resolved_commit = None;
        clean.last_verified = Some(Timestamp::from_unix(0));
        assert_eq!(
            status_text(&[state("tokio", "1"), clean.clone()], 42),
            "tokio@1: partial, revision 2, 3 documents, 7 sections\n  last attempt 1970-01-01T00:01:00Z, last success \
             1970-01-01T00:01:00Z, last verified never\n  commit abc123\n  last error: 1 document(s) failed; first: \
             a.md: too_large\naxum@0.8: partial, revision 2, 3 documents, 7 sections\n  last attempt \
             1970-01-01T00:01:00Z, last success 1970-01-01T00:01:00Z, last verified 1970-01-01T00:00:00Z\nIndex size: \
             42 bytes\n"
        );
        assert_eq!(
            status_text(&[], 0),
            "The index is empty; run `zenodotus sync`.\nIndex size: 0 bytes\n"
        );
        let json = status_json(&[clean], 42);
        assert_eq!(json["index_bytes"], 42);
        assert_eq!(json["versions"][0]["scope"]["library"], "axum");
    }
}
