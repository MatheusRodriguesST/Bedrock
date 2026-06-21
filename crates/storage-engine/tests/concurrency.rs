//! Concurrency test for step 8a (RwLock).
//!
//! The engine is wrapped in `Arc<RwLock<Db>>` and hammered from many threads: one
//! writer overwrites a fixed key set round after round, while several readers read
//! those keys concurrently. The invariant under test: every successful `get` returns
//! a value that is *valid* for its key — never another key's bytes, a torn string,
//! or a `from_utf8` panic.
//!
//! This is the regression test for the positioned-read (`read_at`) fix. With the old
//! `seek` + `read_exact`, concurrent readers shared the file cursor of each segment
//! handle: one reader's `seek` would move the cursor out from under another's `read`,
//! so a `get` could return the wrong value or panic decoding garbage. The `RwLock`
//! does NOT prevent this — it lets readers run together on purpose. Only positioned
//! reads (which touch no cursor) make concurrent reads safe, and this test passes
//! only because of them. Revert `get` to `seek` + `read_exact` and it should start
//! failing.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;

use storage_engine::Db;

const KEYS: u64 = 10;
const WRITE_ROUNDS: u64 = 100; // every round rewrites all KEYS; fsync dominates runtime
const READERS: usize = 8;
const READ_CAP: u64 = 1_000_000; // hard ceiling so readers can never hang the test

fn temp_dir(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("bedrock-test-{}-{}", std::process::id(), name));
    let _ = std::fs::remove_dir_all(&p);
    p
}

// Every value we ever write for key `kN` ends in "-kN" and starts with either the
// seed marker or a `v<round>` overwrite. A misaligned read lands on another record's
// bytes, which fails this shape check (and usually fails to decode as UTF-8 first).
fn value_is_valid(key: &str, value: &str) -> bool {
    let suffix = format!("-{key}");
    value.ends_with(&suffix) && (value.starts_with("seed") || value.starts_with('v'))
}

#[test]
fn concurrent_readers_with_a_writer_never_corrupt() {
    let path = temp_dir("concurrency");
    let p = path.to_str().unwrap().to_string();

    // Seed every key so readers always find a value from the very first read.
    let mut db = Db::open(&p).unwrap();
    for i in 0..KEYS {
        db.set(format!("k{i}"), format!("seed-k{i}")).unwrap();
    }

    let db = Arc::new(RwLock::new(db));
    let writer_done = Arc::new(AtomicBool::new(false));

    // Writer: rewrite the whole key set, round after round. The write lock is taken
    // PER `set` (not held across the loop) so readers get windows between writes —
    // holding it across the whole loop would serialize everything into a single
    // writer and defeat the point of the test.
    let writer = {
        let db = Arc::clone(&db);
        let writer_done = Arc::clone(&writer_done);
        thread::spawn(move || {
            for round in 0..WRITE_ROUNDS {
                for i in 0..KEYS {
                    db.write()
                        .unwrap()
                        .set(format!("k{i}"), format!("v{round}-k{i}"))
                        .unwrap();
                }
            }
            writer_done.store(true, Ordering::Release);
        })
    };

    // Readers: keep reading every key until the writer finishes (or the safety cap is
    // hit). Each value seen must be valid for its key. `yield_now` keeps a reader from
    // monopolizing the lock and starving the writer on reader-preferring platforms.
    let readers: Vec<_> = (0..READERS)
        .map(|t| {
            let db = Arc::clone(&db);
            let writer_done = Arc::clone(&writer_done);
            thread::spawn(move || {
                let mut reads = 0u64;
                while !writer_done.load(Ordering::Acquire) && reads < READ_CAP {
                    for i in 0..KEYS {
                        let key = format!("k{i}");
                        if let Some(value) = db.read().unwrap().get(&key).unwrap() {
                            assert!(
                                value_is_valid(&key, &value),
                                "reader {t} saw a corrupt value for {key}: {value:?}"
                            );
                        }
                        reads += 1;
                    }
                    thread::yield_now();
                }
                reads
            })
        })
        .collect();

    writer.join().unwrap();
    let total_reads: u64 = readers.into_iter().map(|r| r.join().unwrap()).sum();
    assert!(
        total_reads > 0,
        "readers never ran — no concurrency exercised"
    );

    // Final state: every key holds the last value the writer wrote, and it survives
    // a reopen (the concurrent rewrites + any compaction left a consistent log).
    {
        let guard = db.read().unwrap();
        for i in 0..KEYS {
            assert_eq!(
                guard.get(&format!("k{i}")).unwrap(),
                Some(format!("v{}-k{i}", WRITE_ROUNDS - 1))
            );
        }
    }
    drop(db);

    let reopened = Db::open(&p).unwrap();
    for i in 0..KEYS {
        assert_eq!(
            reopened.get(&format!("k{i}")).unwrap(),
            Some(format!("v{}-k{i}", WRITE_ROUNDS - 1)),
            "k{i} wrong after a concurrent run + reopen"
        );
    }

    let _ = std::fs::remove_dir_all(&p);
}
