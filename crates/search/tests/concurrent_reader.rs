//! A reader in this process queries in a loop while a child process (this same test binary) syncs and merges
//! repeatedly. Every response must come from one consistent snapshot: all its sections carry the revision its sync
//! status reports.

use std::path::Path;
use std::process::{Command, Stdio};

use zenodotus_core::config::{IndexSettings, RankingSettings};
use zenodotus_core::ids::{ChunkId, DocKey, LibraryId, Scope, Version};
use zenodotus_core::model::{DocState, Document, Outcome, Section, VersionChanges, VersionState, content_hash};
use zenodotus_core::time::Timestamp;
use zenodotus_search::query::{QueryRequest, query_docs};
use zenodotus_search::store::{Store, StoreWriter};

/// Set in the child process to the data directory it writes to.
const CHILD_DIR: &str = "ZENODOTUS_STRESS_DIR";
/// Commits the child makes.
const REVISIONS: u64 = 120;
/// Documents per revision, and sections per document.
const DOCUMENTS: u32 = 3;
const SECTIONS: u32 = 3;

fn scope() -> Scope {
    Scope::new(LibraryId::parse("stress").unwrap(), Version::parse("1").unwrap())
}

fn snapshot(at: i64) -> VersionState {
    VersionState {
        scope: scope(),
        name: "stress".to_owned(),
        description: String::new(),
        aliases: Vec::new(),
        homepage: None,
        license: None,
        source: "local:stress".to_owned(),
        default: true,
        list_order: 0,
        revision: 0,
        last_success: None,
        last_attempt: Timestamp::from_unix(at),
        last_verified: None,
        outcome: Outcome::Ok,
        last_error: None,
        document_count: 0,
        section_count: 0,
        resolved_commit: None,
    }
}

/// Every document rewritten so that each section says which revision wrote it.
fn revision_changes(revision: u64) -> VersionChanges {
    let scope = scope();
    let upserts = (0..DOCUMENTS)
        .map(|doc| {
            let path = format!("doc{doc}.md");
            let doc_key = DocKey::new(&scope, &path).unwrap();
            let sections = (0..SECTIONS)
                .map(|ordinal| {
                    let text = format!("common words in section {ordinal} of {path}, marker r{revision}");
                    Section {
                        chunk_id: ChunkId::derive(&scope, &path, ordinal),
                        doc_key: doc_key.clone(),
                        ordinal,
                        title: path.clone(),
                        heading_path: vec![path.clone()],
                        content_hash: content_hash(&text),
                        text,
                        code_languages: Vec::new(),
                        symbols: Vec::new(),
                        source_ref: format!("local:{path}"),
                    }
                })
                .collect();
            Document {
                state: DocState {
                    doc_key,
                    etag: None,
                    last_modified: None,
                    content_hash: content_hash(&format!("{path} r{revision}")),
                    fetched_at: Timestamp::from_unix(0),
                },
                sections,
            }
        })
        .collect();
    VersionChanges {
        upserts,
        ..VersionChanges::default()
    }
}

fn write_revisions(data_dir: &Path, revisions: std::ops::RangeInclusive<u64>) {
    let mut writer = StoreWriter::open(data_dir, &IndexSettings::default()).unwrap();
    for revision in revisions {
        let at = i64::try_from(revision).unwrap();
        let state = writer.apply(&snapshot(at), &revision_changes(revision)).unwrap();
        assert_eq!(state.revision, revision);
    }
    writer.finish().unwrap();
}

/// The child half of [`a_reader_always_sees_one_consistent_snapshot`]; does nothing in a normal test run.
#[test]
fn child_writer() {
    if let Ok(dir) = std::env::var(CHILD_DIR) {
        write_revisions(Path::new(&dir), 2..=REVISIONS);
    }
}

#[test]
fn a_reader_always_sees_one_consistent_snapshot() {
    let dir = tempfile::TempDir::new().unwrap();
    write_revisions(dir.path(), 1..=1);
    let store = Store::open(dir.path(), &IndexSettings::default()).unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_writer", "--quiet"])
        .env(CHILD_DIR, dir.path())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let request = QueryRequest {
        library_id: "stress",
        query: "common words",
        version: None,
        language: None,
        max_results: 20,
        max_chars: 100_000,
    };
    let mut queries = 0;
    let mut reload_failures = 0;
    let mut revisions_seen = std::collections::BTreeSet::new();
    let status = loop {
        let finished = child.try_wait().unwrap();
        // A reload can race with a merge that removes a segment; the previous snapshot then keeps serving.
        reload_failures += u32::from(store.reload().is_err());
        let result = query_docs(&store, &request, &RankingSettings::default()).unwrap();
        let revision = result.sync_status.revision;
        let marker = format!("marker r{revision}");
        assert_eq!(
            result.results.len(),
            (DOCUMENTS * SECTIONS) as usize,
            "revision {revision}"
        );
        for item in &result.results {
            assert!(item.text.ends_with(&marker), "{} in revision {revision}", item.text);
        }
        revisions_seen.insert(revision);
        queries += 1;
        if let Some(status) = finished {
            break status;
        }
    };
    assert!(status.success(), "the writer process failed: {status}");
    assert!(
        queries > reload_failures,
        "{queries} queries, {reload_failures} failed reloads"
    );
    assert_eq!(revisions_seen.first(), Some(&1));
    assert_eq!(revisions_seen.last(), Some(&REVISIONS));
}
