//! Search latency over a generated 10,000-note vault (acceptance target: < 100 ms per query).

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

#[path = "../tests/support/vaultgen.rs"]
mod generator;

use std::hint::black_box;
use std::time::Instant;

use criterion::{Criterion, criterion_group, criterion_main};
use selenite_core::VaultPath;
use selenite_index::Engine;

const NOTES: usize = 10_000;

fn bench_search(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    generator::generate_vault(dir.path(), NOTES).unwrap();
    let t = Instant::now();
    let engine = Engine::open_local(dir.path()).unwrap();
    eprintln!(
        "indexed {} pages in {:?}",
        engine.index().page_count().unwrap(),
        t.elapsed()
    );

    let queries = [
        ("single_term", "lantern"),
        ("prefix", "obsi"),
        ("two_terms", "harbor quartz"),
        ("phrase", "\"lunar orbit\""),
        ("term_and_tag", "garden tag:river"),
        ("tag_only", "tag:zenith"),
        ("excluded", "meadow -valley"),
    ];
    let mut group = c.benchmark_group("search_10k");
    for (name, q) in queries {
        group.bench_function(name, |b| {
            b.iter(|| black_box(engine.search(black_box(q), 50).unwrap()))
        });
    }
    let target = VaultPath::new("notes/b07/Note 00007.md").unwrap();
    group.bench_function("backlinks", |b| {
        b.iter(|| black_box(engine.backlinks(black_box(&target)).unwrap()))
    });
    group.finish();
}

criterion_group!(benches, bench_search);
criterion_main!(benches);
