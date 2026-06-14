//! Helper for the crash-recovery test (`tests/crash_recovery.rs`).
//!
//! Opens the log at the given path and writes sequential keys (`key-0`, `key-1`, …)
//! in an infinite loop; each `set` fsyncs, so every write is durable before the next
//! begins. The test SIGKILLs this process mid-write — there is no clean shutdown.

use storage_engine::Db;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: crash_writer <log_path>");
    let mut db = Db::open(&path).expect("failed to open log");

    let mut i: u64 = 0;
    loop {
        db.set(format!("key-{i}"), format!("val-{i}"))
            .expect("failed to write");
        i += 1;
    }
}
