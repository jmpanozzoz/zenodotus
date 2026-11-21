//! `query_docs` on a synthetic index of 50,000 sections: the cost of one query, as an agent's call pays it.
#![forbid(unsafe_code)]
#![allow(missing_docs, reason = "criterion's macros generate undocumented items")]

mod support;

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use zenodotus_core::config::{IndexSettings, RankingSettings};
use zenodotus_search::query::{QueryRequest, query_docs};
use zenodotus_search::store::{Store, StoreWriter};

/// Queries of every shape the ranking handles: keywords, a symbol, a phrase and a question.
const QUERIES: [&str; 4] = [
    "spawn task runtime",
    "channel_select",
    "\"stream timeout\"",
    "how do I parse a json response from the server",
];

fn bench(c: &mut Criterion) {
    let dir = tempfile::TempDir::new().unwrap();
    let settings = IndexSettings::default();
    let mut writer = StoreWriter::open(dir.path(), &settings).unwrap();
    writer
        .apply(&support::version_state(), &support::changes(5_000, 10))
        .unwrap();
    writer.finish().unwrap();
    let store = Store::open(dir.path(), &settings).unwrap();
    let ranking = RankingSettings::default();
    let mut group = c.benchmark_group("query_docs_50k_sections");
    for query in QUERIES {
        let request = QueryRequest {
            library_id: "bench",
            query,
            version: None,
            language: None,
            max_results: 10,
            max_chars: 12_000,
        };
        group.bench_function(query, |b| {
            b.iter(|| query_docs(&store, black_box(&request), &ranking).unwrap());
        });
    }
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
