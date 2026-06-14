//! Baseline comparison: Bedrock vs SQLite, under *matched* durability.
//!
//! Both engines fsync every write to survive a machine crash:
//!   - Bedrock: `sync_all` per write (F_FULLFSYNC on macOS).
//!   - SQLite:  WAL + `synchronous=FULL` + `fullfsync=ON`, one autocommit txn per write.
//!
//! They are different designs (Bedrock: append-only KV log; SQLite: B-tree SQL engine).
//! The point is positioning, not a winner — run with `cargo bench --bench compare`.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use rusqlite::Connection;
use storage_engine::Db;

const PRELOAD: u64 = 5_000;

fn tmp(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("bedrock-cmp-{}-{}", std::process::id(), tag));
    let _ = std::fs::remove_dir_all(&p);
    let _ = std::fs::remove_file(&p);
    p
}

fn open_sqlite(path: &std::path::Path) -> Connection {
    let conn = Connection::open(path).unwrap();
    // Match Bedrock's durability: fsync (full barrier) on every committed write.
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=FULL;
         PRAGMA fullfsync=ON;",
    )
    .unwrap();
    conn.execute(
        "CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT)",
        [],
    )
    .unwrap();
    conn
}

fn bench_writes(c: &mut Criterion) {
    let mut g = c.benchmark_group("write (append + fsync)");
    g.sample_size(50);

    let bpath = tmp("bedrock-w");
    let mut db = Db::open(bpath.to_str().unwrap()).unwrap();
    g.bench_function("bedrock", |b| {
        b.iter(|| {
            db.set(
                black_box("k".to_string()),
                black_box("value-payload".to_string()),
            )
            .unwrap()
        })
    });

    let spath = tmp("sqlite-w.db");
    let conn = open_sqlite(&spath);
    g.bench_function("sqlite", |b| {
        b.iter(|| {
            conn.execute(
                "INSERT OR REPLACE INTO kv (k, v) VALUES (?1, ?2)",
                black_box(("k", "value-payload")),
            )
            .unwrap()
        })
    });
    g.finish();
    let _ = std::fs::remove_dir_all(&bpath);
}

fn bench_reads(c: &mut Criterion) {
    let mut g = c.benchmark_group("read (point lookup)");

    let bpath = tmp("bedrock-r");
    let mut db = Db::open(bpath.to_str().unwrap()).unwrap();
    for i in 0..PRELOAD {
        db.set(format!("key-{i}"), format!("val-{i}")).unwrap();
    }
    g.bench_function("bedrock", |b| {
        b.iter(|| db.get(black_box("key-2500")).unwrap())
    });

    let spath = tmp("sqlite-r.db");
    let conn = open_sqlite(&spath);
    for i in 0..PRELOAD {
        conn.execute(
            "INSERT OR REPLACE INTO kv (k, v) VALUES (?1, ?2)",
            (format!("key-{i}"), format!("val-{i}")),
        )
        .unwrap();
    }
    g.bench_function("sqlite", |b| {
        b.iter(|| {
            let _: String = conn
                .query_row(
                    "SELECT v FROM kv WHERE k = ?1",
                    black_box(["key-2500"]),
                    |r| r.get(0),
                )
                .unwrap();
        })
    });
    g.finish();
    let _ = std::fs::remove_dir_all(&bpath);
}

criterion_group!(benches, bench_writes, bench_reads);
criterion_main!(benches);
