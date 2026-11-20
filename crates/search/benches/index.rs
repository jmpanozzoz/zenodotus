//! Indexing throughput: applying a version of 1,000 documents of 10 sections each to a fresh store.
#![forbid(unsafe_code)]
#![allow(missing_docs, reason = "criterion's macros generate undocumented items")]

mod support;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use zenodotus_core::config::IndexSettings;
use zenodotus_search::store::StoreWriter;

const DOCUMENTS: usize = 1_000;
const SECTIONS: usize = 10;

fn bench(c: &mut Criterion) {
    let changes = support::changes(DOCUMENTS, SECTIONS);
    let state = support::version_state();
    let settings = IndexSettings::default();
    let mut group = c.benchmark_group("index");
    group.sample_size(10);
    group.throughput(Throughput::Elements((DOCUMENTS * SECTIONS) as u64));
    group.bench_function("apply_10k_sections", |b| {
        b.iter_batched(
            || tempfile::TempDir::new().unwrap(),
            |dir| {
                let mut writer = StoreWriter::open(dir.path(), &settings).unwrap();
                writer.apply(&state, &changes).unwrap();
                writer.finish().unwrap();
                dir
            },
            BatchSize::PerIteration,
        );
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
