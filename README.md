# Bedrock

[![CI](https://github.com/MatheusRodriguesST/Bedrock/actions/workflows/ci.yml/badge.svg)](https://github.com/MatheusRodriguesST/Bedrock/actions/workflows/ci.yml)

A durable key/value **storage engine** written in Rust. Bedrock persists data in a
log format it fully controls, rebuilds its state after a restart, and is designed to
survive a process or machine crash without losing acknowledged writes.

It is a learning-by-building project focused on database internals: on-disk storage,
durability, crash recovery, and indexing. Design decisions and their trade-offs are
documented below — that is the point of the project as much as the code.

**Highlights**

- Custom binary, length-prefixed on-disk format with a per-record **CRC32**.
- **`fsync` on every write** (`F_FULLFSYNC` on macOS) → crash durability, proven by a SIGKILL test.
- In-memory **offset index** (Bitcask-style): values stay on disk, so the dataset can exceed RAM.
- Log-structured **segments + crash-safe compaction** (atomic manifest swap).
- **Benchmarked vs SQLite** under matched durability; CI runs fmt + clippy + tests on every push.

## Status & guarantees

| Property | Guarantee |
|----------|-----------|
| **Durability** | An acknowledged write survives a process **and** machine crash (power loss / kernel panic), assuming the disk honors the flush. Every write is `fsync`'d before `set`/`delete` returns. |
| **Crash recovery** | A write torn in half by a crash is detected via a per-record CRC32 and discarded on replay; all complete records before it are preserved. Proven by an automated SIGKILL test. |
| **Atomicity** | A single `set`/`delete` is the unit of atomicity (one record). Multi-key transactions are not supported yet. |
| **Isolation** | Reads touch no shared file cursor (positioned `pread`), so the engine is `Send + Sync` and safe to share as `Arc<RwLock<Db>>`: **many concurrent readers xor one exclusive writer**. The lock is coarse — a writer (including compaction) blocks all readers while it runs. No snapshot isolation across operations yet; MVCC is the next step. |

## Architecture

Bedrock is a **Bitcask-style** engine (see *Designing Data-Intensive Applications*,
ch. 3, "Hash Indexes"):

- **Append-only log.** Every `set`/`delete` appends one record to the active segment.
  Nothing is ever updated in place.
- **In-memory index.** A hash map maps each key to the **on-disk location** of its
  value (`offset + length`) — not the value itself. RAM usage scales with the *number
  of keys*, not the total size of the values, so the dataset can exceed RAM. Reads do
  one positioned read (`pread` / `read_at`) — no shared file cursor, so concurrent
  readers don't interfere; the OS page cache absorbs the cost of hot keys.
- **Segments + compaction.** The log is split into fixed-size segment files; the active
  one rolls over when it fills. A size-triggered **compaction** merges the immutable
  segments into one, keeping only the latest value per key, which reclaims the space held
  by overwritten and deleted records.
- **Recovery by replay.** On `open`, the live segments are replayed in order to rebuild
  the index. A `DEL` tombstone removes the key, so deletes survive restarts.

### Manifest (crash-safe compaction)

A `manifest` file lists the live segments in replay order and is the source of truth for
which segments are valid. Compaction writes a new segment, fsyncs it, then **atomically
swaps the manifest** (write-temp + `rename`) before deleting the old segments. A crash
before the swap leaves the old set live (the new segment is an ignored orphan); a crash
after leaves the new set live (the old segments are orphans, cleaned up on the next open).
The swap is the single atomic step that makes compaction safe — the same idea LevelDB and
RocksDB use.

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
the ratios and the comparison below are the point.

| Operation | Latency | Throughput |
|-----------|---------|------------|
| `set` (append + fsync) | ~2.6 ms | ~385 ops/s |
| `get` (index + seek + read, warm cache) | ~770 ns | ~1.3M ops/s |
| `open` (replay 10k records) | ~1.5 ms | ~150 ns/record |

Reads pay no durability barrier; writes do — which is why writes are ~3,000× slower. On
macOS, `File::sync_all` issues `F_FULLFSYNC`, forcing data to the physical medium (not just
the drive cache), so that ~2.6 ms is the honest cost of *true* crash durability. It also
motivates a future **group-commit** optimization (batch writes behind one flush).

### vs SQLite (matched durability)

Same workload against SQLite in WAL mode with `synchronous=FULL` + `fullfsync=ON`, so both
engines fsync every write to survive a machine crash. They are different designs — Bedrock
is an append-only KV log, SQLite a B-tree SQL engine — so this is positioning, not a contest
(`cargo bench --bench compare`):

| Operation | Bedrock | SQLite |
|-----------|---------|--------|
| write (fsync per write) | ~2.6 ms | ~2.8 ms |
| read (point lookup) | ~0.77 µs | ~2.15 µs |

Writes land in the same ballpark: both are dominated by the fsync barrier, so the cost is
durability, not the engine. On point reads Bedrock is ~3× faster — a hash-index lookup plus
one `seek`+`read` beats SQL parsing and a B-tree descent, which is the upside of a
specialized key/value store over a general SQL engine.

## Design decisions & trade-offs

- **Append-only log, not in-place B-Tree.** Appends are sequential writes (fast, simple
  to make crash-safe) at the cost of space: superseded and deleted records linger until
  **compaction** reclaims them. A B-Tree updates in place — less space amplification, but
  in-place writes are harder to make atomic and durable.
- **Index stores offsets, not values.** Enables datasets larger than RAM, at the cost of
  one disk read per `get` (read amplification), softened by the page cache.
- **Custom binary format, no serialization framework.** The point is to control the
  on-disk layout end to end. CRC32 (`crc32fast`) is used only for integrity, not security.
- **Durability is verified, not assumed.** `tests/crash_recovery.rs` spawns a writer,
  `SIGKILL`s it mid-write, reopens the log, and asserts that surviving keys form a
  contiguous, uncorrupted prefix — no lost committed data, no replay crash.

### What it does **not** do (yet)

Multi-key transactions, concurrent writers, and a network API. See the roadmap.

## Roadmap

- [x] Append-only log, in-memory index, `set` / `get`
- [x] Crash recovery on restart via log replay
- [x] `delete` via tombstones
- [x] Binary length-prefixed format, per-record CRC32, `fsync` → crash durability
- [x] Offset index (values on disk, not in RAM) — Bitcask-complete
- [x] Segments + size-triggered, crash-safe compaction (manifest swap)
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
