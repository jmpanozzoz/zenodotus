//! Peak heap while a job streams 5,000 documents into the store (plan §3.11, design §12.8). dhat allows one profiler
//! per process, so this binary holds one test.

use dhat::{HeapStats, Profiler};
use tempfile::TempDir;
use zenodotus_core::config::IndexSettings;
use zenodotus_core::ids::{ChunkId, DocKey, LibraryId, Scope, Version};
use zenodotus_core::model::{DocState, Document, Origin, Outcome, Section, VersionState, content_hash};
use zenodotus_core::time::Timestamp;
use zenodotus_ingest::sink::{DocumentSink, Flow};
use zenodotus_jobs::writer::StoreSink;
use zenodotus_search::store::StoreWriter;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

const DOCUMENTS: u32 = 5_000;
const SECTIONS_PER_DOCUMENT: u32 = 4;
const MIB: usize = 1 << 20;
/// The §12.8 budget a streaming job may use beside the readers: the 32 MiB writer heap and the 24 MiB ingest working
/// set.
const BUDGET_BYTES: usize = (32 + 24) * MIB;

/// A synthetic page of about 2 KiB, built on demand so that only one exists at a time.
fn document(scope: &Scope, number: u32) -> Document {
    let doc_path = format!("guide/page-{number}.md");
    let sections: Vec<Section> = (0..SECTIONS_PER_DOCUMENT)
        .map(|ordinal| {
            let text =
                format!("Widget {number} option {ordinal} sets the frobnication depth of the sprocket. ").repeat(8);
            Section {
                chunk_id: ChunkId::derive(scope, &doc_path, ordinal),
                doc_key: DocKey::new(scope, &doc_path).unwrap(),
                ordinal,
                title: format!("Page {number}"),
                heading_path: vec![format!("Page {number}"), format!("Option {ordinal}")],
                content_hash: content_hash(&text),
                text,
                code_languages: Vec::new(),
                symbols: vec![format!("widget_{number}_{ordinal}")],
                source_ref: format!("local:{doc_path}"),
            }
        })
        .collect();
    Document {
        state: DocState {
            doc_key: DocKey::new(scope, &doc_path).unwrap(),
            etag: None,
            last_modified: None,
            content_hash: content_hash(&doc_path),
            fetched_at: Timestamp::from_unix(1),
            source: None,
        },
        sections,
    }
}

fn snapshot(scope: &Scope) -> VersionState {
    VersionState {
        scope: scope.clone(),
        name: "widgets".to_owned(),
        description: String::new(),
        aliases: Vec::new(),
        homepage: None,
        license: None,
        source: "local:docs".to_owned(),
        default: true,
        list_order: 0,
        revision: 0,
        last_success: None,
        last_attempt: Timestamp::from_unix(2),
        last_verified: None,
        outcome: Outcome::Ok,
        last_error: None,
        document_count: 0,
        section_count: 0,
        resolved_commit: None,
        origin: Origin::Config,
        plan: None,
        bytes: 0,
    }
}

#[test]
fn a_streaming_job_stays_under_the_ingest_budget() {
    let dir = TempDir::new().unwrap();
    let _profiler = Profiler::builder()
        .testing()
        .file_name(dir.path().join("dhat-heap.json"))
        .build();
    let scope = Scope::new(LibraryId::parse("widgets").unwrap(), Version::parse("1").unwrap());
    let mut writer = StoreWriter::open(&dir.path().join("data"), &IndexSettings::default()).unwrap();
    let mut sink = StoreSink::begin(&mut writer, &scope).unwrap();
    for number in 0..DOCUMENTS {
        assert_eq!(sink.upsert(document(&scope, number)), Flow::Continue);
    }
    let stored = sink.commit(&snapshot(&scope)).unwrap();
    writer.finish().unwrap();
    assert_eq!(
        (stored.document_count, stored.section_count),
        (u64::from(DOCUMENTS), u64::from(DOCUMENTS * SECTIONS_PER_DOCUMENT))
    );
    let peak = HeapStats::get().max_bytes;
    dhat::assert!(peak < BUDGET_BYTES, "peak heap {peak} bytes over {BUDGET_BYTES}");
}
