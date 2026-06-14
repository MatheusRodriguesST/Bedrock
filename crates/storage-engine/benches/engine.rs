//! Benchmarks for the storage engine. Run with `cargo bench`.
//!
//! Criterion stores a baseline under target/criterion and reports the delta on
//! the next run, so regressions/improvements show up as each milestone lands.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use storage_engine::Db;

const PRELOAD: u64 = 10_000;

fn temp_path(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("bedrock-bench-{}-{}.db", std::process::id(), tag));
    let _ = std::fs::remove_file(&p);
    p
}

fn preload(db: &mut Db, n: u64) {
    for i in 0..n {
        db.set(format!("key-{i}"), format!("val-{i}")).unwrap();
    }
}

// Write path: append a record + fsync. Dominated by the fsync.
fn bench_set(c: &mut Criterion) {
    let path = temp_path("set");
    let mut db = Db::open(path.to_str().unwrap()).unwrap();

    let mut g = c.benchmark_group("set");
    g.sample_size(50);
    g.bench_function("append+fsync", |b| {
        b.iter(|| {
            db.set(
                black_box("k".to_string()),
                black_box("value-payload".to_string()),
            )
            .unwrap();
        });
    });
    g.finish();
    let _ = std::fs::remove_file(&path);
}

// Read path: index lookup + seek + read from disk.
fn bench_get(c: &mut Criterion) {
    let path = temp_path("get");
    let mut db = Db::open(path.to_str().unwrap()).unwrap();
    preload(&mut db, PRELOAD);

    c.bench_function("get (seek+read)", |b| {
        b.iter(|| {
            db.get(black_box("key-5000")).unwrap();
        });
    });
    let _ = std::fs::remove_file(&path);
}

// Startup: rebuild the in-memory index by replaying the whole log.
fn bench_open(c: &mut Criterion) {
    let path = temp_path("open");
    {
        let mut db = Db::open(path.to_str().unwrap()).unwrap();
        preload(&mut db, PRELOAD);
    }

    let mut g = c.benchmark_group("open");
    g.sample_size(30);
    g.bench_function("replay 10k records", |b| {
        b.iter(|| {
            let db = Db::open(black_box(path.to_str().unwrap())).unwrap();
            black_box(db.get("key-0").unwrap());
        });
    });
    g.finish();
    let _ = std::fs::remove_file(&path);
}

criterion_group!(benches, bench_set, bench_get, bench_open);
criterion_main!(benches);
