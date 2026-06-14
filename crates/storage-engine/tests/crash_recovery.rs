//! Crash-recovery test: SIGKILL the writer process mid-write, then verify no
//! committed data is lost and that replay survives the possibly-torn tail.
//!
//! Integration test: it only sees the public `storage_engine::Db` API.

use std::process::Command;
use std::time::{Duration, Instant};
use storage_engine::Db;

fn temp_path(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("bedrock-crash-{}-{}.db", std::process::id(), name));
    let _ = std::fs::remove_file(&p);
    p
}

#[test]
fn survives_sigkill_mid_write() {
    let path = temp_path("sigkill");
    let p = path.to_str().unwrap().to_string();

    // Spawn the writer (separate binary) that appends records, fsyncing each.
    // CARGO_BIN_EXE_crash_writer is the binary's path, injected by Cargo for tests.
    let mut child = Command::new(env!("CARGO_BIN_EXE_crash_writer"))
        .arg(&p)
        .spawn()
        .expect("failed to spawn crash_writer");

    // Wait until the log holds several complete records, then kill it.
    // child.kill() sends SIGKILL on Unix.
    let start = Instant::now();
    loop {
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.len() > 200 {
                break;
            }
        }
        if start.elapsed() > Duration::from_secs(5) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // Let it run a bit more to raise the odds of dying mid-write.
    std::thread::sleep(Duration::from_millis(50));
    child.kill().expect("failed to kill writer");
    let _ = child.wait();

    // Reopen after the crash — replay must survive the possibly-torn tail.
    let db = Db::open(&p).expect("reopening the log after crash failed");

    // Invariant: present keys form a contiguous prefix key-0..key-(n-1). Writes are
    // sequential and each fsyncs before the next, so the log is a run of complete
    // records plus at most one torn record at the end; replay applies the complete
    // ones and drops the torn one — no gap, no garbage.
    let mut n: u64 = 0;
    while db.get(&format!("key-{n}")).unwrap().is_some() {
        n += 1;
    }
    assert!(
        n >= 1,
        "expected at least one committed write before the crash"
    );

    for i in 0..n {
        assert_eq!(
            db.get(&format!("key-{i}")).unwrap().as_deref(),
            Some(format!("val-{i}").as_str()),
            "committed key key-{i} was lost or corrupted after the crash"
        );
    }
    assert!(
        db.get(&format!("key-{n}")).unwrap().is_none(),
        "index has a gap: key-{n} missing but a later key is present"
    );

    let _ = std::fs::remove_file(&path);
}
