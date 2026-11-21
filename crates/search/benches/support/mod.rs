//! A deterministic synthetic corpus for the benches: documents of sections drawn from a fixed vocabulary.

use zenodotus_core::ids::{ChunkId, DocKey, LibraryId, Scope, Version};
use zenodotus_core::model::{DocState, Document, Origin, Outcome, Section, VersionChanges, VersionState, content_hash};
use zenodotus_core::time::Timestamp;

/// Words the sections are made of.
const WORDS: [&str; 48] = [
    "async", "await", "buffer", "channel", "client", "config", "cursor", "decode", "encode", "error", "event", "fetch",
    "file", "future", "handle", "header", "index", "iterator", "json", "lock", "loop", "message", "module", "mutex",
    "option", "parse", "path", "promise", "query", "reader", "request", "response", "result", "route", "runtime",
    "schema", "select", "server", "signal", "socket", "spawn", "stream", "string", "task", "timeout", "token", "value",
    "writer",
];

/// The scope every synthetic document belongs to.
#[must_use]
pub fn scope() -> Scope {
    Scope::new(LibraryId::parse("bench").unwrap(), Version::parse("1.0").unwrap())
}

/// A 64-bit linear congruential step, enough to spread words deterministically.
fn next(state: &mut u64) -> usize {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    usize::try_from(*state >> 33).unwrap()
}

/// The text of section `ordinal` of document `doc`: prose, a symbol and a code block.
fn text(doc: usize, ordinal: usize) -> String {
    let mut state = (doc * 1_000 + ordinal) as u64;
    let prose: Vec<&str> = (0..60).map(|_| WORDS[next(&mut state) % WORDS.len()]).collect();
    let symbol = format!("{}_{}", WORDS[doc % WORDS.len()], WORDS[ordinal % WORDS.len()]);
    format!(
        "{} `{symbol}()` {}.\n\n```rust\nlet value = {symbol}(input)?;\n```\n",
        prose[..30].join(" "),
        prose[30..].join(" ")
    )
}

/// `documents` documents of `sections` sections each.
#[must_use]
pub fn changes(documents: usize, sections: usize) -> VersionChanges {
    let scope = scope();
    let upserts = (0..documents)
        .map(|doc| {
            let path = format!("guide/page{doc}.md");
            let doc_key = DocKey::new(&scope, &path).unwrap();
            let sections: Vec<Section> = (0..sections)
                .map(|ordinal| {
                    let text = text(doc, ordinal);
                    Section {
                        chunk_id: ChunkId::derive(&scope, &path, u32::try_from(ordinal).unwrap()),
                        doc_key: doc_key.clone(),
                        ordinal: u32::try_from(ordinal).unwrap(),
                        title: format!("Page {doc}"),
                        heading_path: vec![format!("Section {ordinal}")],
                        content_hash: content_hash(&text),
                        text,
                        code_languages: vec!["rust".to_owned()],
                        symbols: vec![format!("{}_{}", WORDS[doc % WORDS.len()], WORDS[ordinal % WORDS.len()])],
                        source_ref: format!("local:{path}"),
                    }
                })
                .collect();
            Document {
                state: DocState {
                    doc_key,
                    etag: None,
                    last_modified: None,
                    content_hash: content_hash(&path),
                    fetched_at: Timestamp::from_unix(1),
                    source: None,
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

/// The version state the synthetic documents are applied under.
#[must_use]
pub fn version_state() -> VersionState {
    let scope = scope();
    VersionState {
        name: "Bench".to_owned(),
        scope,
        description: String::new(),
        aliases: Vec::new(),
        homepage: None,
        license: None,
        source: "local:bench".to_owned(),
        default: true,
        list_order: 0,
        revision: 0,
        last_success: None,
        last_attempt: Timestamp::from_unix(0),
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
