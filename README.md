# Bedrock

[![CI](https://github.com/MatheusRodriguesST/Bedrock/actions/workflows/ci.yml/badge.svg)](https://github.com/MatheusRodriguesST/Bedrock/actions/workflows/ci.yml)

A durable key/value **storage engine** written in Rust. Bedrock persists data in a
log format it fully controls, rebuilds its state after a restart, and is designed to
survive a process or machine crash without losing acknowledged writes.

It is a learning-by-building project focused on database internals: on-disk storage,
durability, crash recovery, and indexing. Design decisions and their trade-offs are
documented below — that is the point of the project as much as the code.

## Status & guarantees

| Property | Guarantee |
|----------|-----------|
| **Durability** | An acknowledged write survives a process **and** machine crash (power loss / kernel panic), assuming the disk honors the flush. Every write is `fsync`'d before `set`/`delete` returns. |
| **Crash recovery** | A write torn in half by a crash is detected via a per-record CRC32 and discarded on replay; all complete records before it are preserved. Proven by an automated SIGKILL test. |
| **Atomicity** | A single `set`/`delete` is the unit of atomicity (one record). Multi-key transactions are not supported yet. |
| **Isolation** | Single-writer, single-process. Concurrent access control (locking / MVCC) is on the roadmap, not implemented. |

## Architecture

Bedrock is a **Bitcask-style** engine (see *Designing Data-Intensive Applications*,
ch. 3, "Hash Indexes"):

- **Append-only log.** Every `set`/`delete` appends one record to the end of a single
  data file. Nothing is ever updated in place.
- **In-memory index.** A hash map maps each key to the **on-disk location** of its
  value (`offset + length`) — not the value itself. RAM usage scales with the *number
  of keys*, not the total size of the values, so the dataset can exceed RAM. Reads do
  one `seek + read`; the OS page cache absorbs the cost of hot keys.
- **Recovery by replay.** On `open`, the log is replayed front to back to rebuild the
  index. A `DEL` tombstone removes the key, so deletes survive restarts.

### On-disk record format

Binary, length-prefixed, all integers little-endian:

```
┌──────────┬────────┬───────────┬───────────┬──────────┬──────────┐
│ checksum │   op   │  key_len  │  val_len  │   key    │   val    │
│  u32 (4) │ u8 (1) │  u32 (4)  │  u32 (4)  │ key_len  │ val_len  │
└──────────┴────────┴───────────┴───────────┴──────────┴──────────┘
             └──── CRC32 covers everything from here on ──────────┘
```

- `checksum` — CRC32 of the record body; detects torn writes and corruption.
- `op` — `0` = SET, `1` = DEL. A full byte, leaving room for future operations.
- `key_len` / `val_len` — field sizes in bytes; `val_len` is `0` for a DEL.

Length-prefixing is what makes torn-write detection possible: the reader reads exactly
N bytes per field instead of scanning for a delimiter, so arbitrary bytes in keys and
values are safe and a half-written record at the tail is unambiguous.

## Benchmarks

Measured with [Criterion](https://github.com/bheisler/criterion.rs) on a development
laptop (macOS); reproduce with `cargo bench`. Absolute numbers are hardware-dependent —
the **ratios** are the point.

| Operation | Latency | Throughput |
|-----------|---------|------------|
| `set` (append + fsync) | ~2.7 ms | ~370 ops/s |
| `get` (index + seek + read, warm cache) | ~740 ns | ~1.35M ops/s |
| `open` (replay 10k records) | ~1.5 ms | ~150 ns/record |

**Reads are ~3,600× faster than writes** — because each write pays a real durability
barrier. On macOS, Rust's `File::sync_all` issues `F_FULLFSYNC`, which forces data all
the way to the physical medium rather than just to the drive's write cache. That ~2.7 ms
is the honest cost of *true* crash durability, not a weaker `fsync`. It also motivates a
future **group-commit** optimization (batch many writes behind one flush) to trade
latency for throughput.

## Design decisions & trade-offs

- **Append-only log, not in-place B-Tree.** Appends are sequential writes (fast, simple
  to make crash-safe) at the cost of space: superseded and deleted records linger until
  **compaction** (roadmap). A B-Tree updates in place — less space amplification, but
  in-place writes are harder to make atomic and durable.
- **Index stores offsets, not values.** Enables datasets larger than RAM, at the cost of
  one disk read per `get` (read amplification), softened by the page cache.
- **Custom binary format, no serialization framework.** The point is to control the
  on-disk layout end to end. CRC32 (`crc32fast`) is used only for integrity, not security.
- **Durability is verified, not assumed.** `tests/crash_recovery.rs` spawns a writer,
  `SIGKILL`s it mid-write, reopens the log, and asserts that surviving keys form a
  contiguous, uncorrupted prefix — no lost committed data, no replay crash.

### What it does **not** do (yet)

Multi-key transactions, concurrent writers, compaction/segment merging, and a network
API. See the roadmap.

## Roadmap

- [x] Append-only log, in-memory index, `set` / `get`
- [x] Crash recovery on restart via log replay
- [x] `delete` via tombstones
- [x] Binary length-prefixed format, per-record CRC32, `fsync` → crash durability
- [x] Offset index (values on disk, not in RAM) — Bitcask-complete
- [ ] Segments + compaction (reclaim space; foundation for an LSM-tree)
- [ ] Concurrency: `RwLock`, then MVCC / snapshot isolation
- [ ] Network API (HTTP or a minimal query language)

## Usage

```bash
cargo test --workspace      # unit + crash-recovery tests
cargo bench --bench engine  # benchmarks (Criterion)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

```rust
use storage_engine::Db;

let mut db = Db::open("data.db")?;
db.set("key".to_string(), "value".to_string())?;
assert_eq!(db.get("key")?.as_deref(), Some("value"));
db.delete("key")?;
```

## Project layout

- `crates/storage-engine` — the engine (library + a small demo binary)
- `crates/server` — future network API (placeholder)
