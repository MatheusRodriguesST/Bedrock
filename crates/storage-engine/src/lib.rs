//! Bedrock — an append-only key/value storage engine with an in-memory index
//! (Bitcask-style). Writes append a length-prefixed, CRC32-checked record to the
//! active segment and fsync it; data lives in a directory of segment files, and the
//! index maps each key to its value's location (segment, offset, length). Reads are
//! positioned (`pread`/`read_at`), so they never move a shared file cursor and are
//! safe to run from many threads at once. See README.md for the on-disk format and
//! durability guarantees.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::fs::OpenOptions;
use std::io::{self, BufReader, Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct Db {
    dir: std::path::PathBuf,
    index: HashMap<String, Vec<Version>>,
    active_id: u64,
    active: fs::File,
    active_size: u64,
    readers: HashMap<u64, fs::File>,
    live: Vec<u64>, // segment ids currently live, in replay order (from the manifest)
    next_id: u64,   // next free segment id (monotonic, never reused)
    next_seq: u64,
    snapshots: Arc<Mutex<BTreeMap<u64, u32>>>,
}

// Copy: small and pointer-like, so compaction can move it without cloning ceremony.
#[derive(Clone, Copy)]
struct ValueLoc {
    seg: u64,
    offset: u64,
    len: u32,
}

/// A consistent point-in-time view of the database, identified by the `seq` that was
/// current when it was taken. Reads through `get_as_of(key, snap.seq)` only ever see
/// versions with `seq <= snap.seq`, so later writes are invisible to the holder no
/// matter how long it lives (snapshot isolation, DDIA ch. 7).
///
/// The token is intentionally lightweight: it owns its `seq` and a clone of the live-
/// snapshot registry, but holds *no* reference to the `Db` and *no* lock. That lets it
/// outlive the short `db.read()` guard used to create it. While it is alive it keeps a
/// refcount on its `seq` in the registry, which the GC (compaction) consults so it never
/// reclaims a version this snapshot can still reach.
pub struct Snapshot {
    pub seq: u64,
    registry: Arc<Mutex<BTreeMap<u64, u32>>>,
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        // Locks only this small mutex, never the big Db lock — that's why the registry
        // lives outside the RwLock: dropping a snapshot mid-write can't deadlock.
        // `if let Ok` over `unwrap`: panicking inside Drop during another panic aborts.
        if let Ok(mut reg) = self.registry.lock() {
            if let Some(count) = reg.get_mut(&self.seq) {
                *count -= 1;
                // Remove at zero so `min_live` reflects only genuinely live snapshots.
                if *count == 0 {
                    reg.remove(&self.seq);
                }
            }
        }
    }
}

struct Version {
    seq: u64,
    kind: VersionKind,
}

enum VersionKind {
    Set(ValueLoc),
    // Tombstone. The u64 is the segment its DEL record lives in, so compaction can tell
    // whether a kept tombstone must be rewritten (its segment is being dropped) or not.
    Delete(u64),
}

// Header bytes before the value: crc(4) + op(1) + seq(8) + key_len(4) + val_len(4).
const HEADER_LEN: u64 = 21;
const OP_SET: u8 = 0;
const OP_DEL: u8 = 1;
const SEGMENT_MAX: u64 = 64 * 1024;
const COMPACT_AFTER: usize = 3; // compact once this many immutable segments pile up

// Serialize one record: [crc u32][op u8][seq u64][key_len u32][val_len u32][key][val], LE.
fn encode_record(op: u8, seq: u64, key: &[u8], val: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(op);
    body.extend_from_slice(&seq.to_le_bytes());
    body.extend_from_slice(&(key.len() as u32).to_le_bytes());
    body.extend_from_slice(&(val.len() as u32).to_le_bytes());
    body.extend_from_slice(key);
    body.extend_from_slice(val);

    let checksum = crc32fast::hash(&body);
    let mut record = Vec::new();
    record.extend_from_slice(&checksum.to_le_bytes());
    record.extend_from_slice(&body);
    record
}

fn seg_path(dir: &Path, id: u64) -> PathBuf {
    dir.join(format!("{id:06}.seg"))
}

// Replay one segment into the index, applying SET/DEL in file order. Returns the
// offset just past the last valid record (a torn/corrupt tail ends replay early).
fn replay_segment(
    file: &fs::File,
    seg: u64,
    index: &mut HashMap<String, Vec<Version>>,
    max_seq: &mut u64,
) -> io::Result<u64> {
    let mut reader = BufReader::new(file);
    let mut offset: u64 = 0;

    loop {
        let mut cksum_bytes = [0u8; 4];
        match reader.read_exact(&mut cksum_bytes) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break, // clean end
            Err(e) => return Err(e),
        }
        let stored_checksum = u32::from_le_bytes(cksum_bytes);

        let mut header = [0u8; 17];
        if reader.read_exact(&mut header).is_err() {
            break;
        }
        let op = header[0];
        let seq = u64::from_le_bytes(header[1..9].try_into().unwrap());
        let key_len = u32::from_le_bytes(header[9..13].try_into().unwrap()) as usize;
        let val_len = u32::from_le_bytes(header[13..17].try_into().unwrap()) as usize;

        let mut payload = vec![0u8; key_len + val_len];
        if reader.read_exact(&mut payload).is_err() {
            break;
        }

        let mut body = Vec::new();
        body.extend_from_slice(&header);
        body.extend_from_slice(&payload);
        if crc32fast::hash(&body) != stored_checksum {
            break; // torn / corrupted tail
        }

        // Both SET and DEL consume a seq, so track the max over every valid record
        // (not just surviving SETs) — a trailing DEL still advances next_seq.
        *max_seq = (*max_seq).max(seq);

        let key = String::from_utf8(payload[..key_len].to_vec()).unwrap();
        let kind = if op == OP_SET {
            let value_offset = offset + HEADER_LEN + key_len as u64;
            VersionKind::Set(ValueLoc {
                seg,
                offset: value_offset,
                len: val_len as u32,
            })
        } else {
            VersionKind::Delete(seg)
        };
        // Append in file order: within a segment records are seq-ascending, and segments
        // replay oldest-first, so each key's version list ends up sorted by seq.
        index.entry(key).or_default().push(Version { seq, kind });
        offset += HEADER_LEN + key_len as u64 + val_len as u64;
    }

    Ok(offset)
}

fn manifest_path(dir: &Path) -> PathBuf {
    dir.join("manifest")
}

// The manifest is the source of truth for which segments are live and in what replay
// order (oldest first), one id per line. Returns None if it doesn't exist yet.
fn read_manifest(dir: &Path) -> io::Result<Option<Vec<u64>>> {
    match fs::read_to_string(manifest_path(dir)) {
        Ok(s) => Ok(Some(
            s.lines()
                .filter_map(|l| l.trim().parse::<u64>().ok())
                .collect(),
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

// Write the live list atomically: write a temp file, fsync it, then rename over the
// manifest (atomic on POSIX). This is what makes compaction crash-safe — the live set
// flips in one step. (Dir fsync is best-effort; the rename itself is already atomic.)
fn write_manifest(dir: &Path, live: &[u64]) -> io::Result<()> {
    let body: String = live.iter().map(|id| format!("{id}\n")).collect();
    let tmp = dir.join("manifest.tmp");
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, manifest_path(dir))?;
    let _ = fs::File::open(dir).and_then(|f| f.sync_all());
    Ok(())
}

// The versions a GC pass must keep for one key, given the oldest live snapshot
// (`min_live`): the "floor" (greatest version with seq <= min_live, which the oldest
// reader resolves to) plus every version above it (any live snapshot can stop on one).
// Everything below the floor is unreachable by any live snapshot, so it can be dropped.
// `versions` is ascending by seq, so the floor is the last index with seq <= min_live and
// the kept slice is [floor..]. No version <= min_live means the key was first written
// after the oldest snapshot, so all versions are above the floor and all are kept.
fn versions_to_keep(versions: &[Version], min_live: u64) -> &[Version] {
    match versions.iter().rposition(|v| v.seq <= min_live) {
        Some(floor) => &versions[floor..],
        None => versions,
    }
}

impl Db {
    /// Open (or create) the segment directory and rebuild the index by replaying the
    /// live segments listed in the manifest, in order (so newer writes win).
    pub fn open(path: &str) -> io::Result<Db> {
        let dir = PathBuf::from(path);
        fs::create_dir_all(&dir)?;

        // The manifest lists the live segments in replay order; a fresh directory
        // starts with one empty segment.
        let live = match read_manifest(&dir)? {
            Some(ids) if !ids.is_empty() => ids,
            _ => {
                write_manifest(&dir, &[1])?;
                vec![1]
            }
        };

        // Ensure the active segment file exists (fresh directory), then replay every
        // live segment in order, keeping a read handle per segment.
        let active_id = *live.last().unwrap();
        let active = OpenOptions::new()
            .create(true)
            .append(true)
            .open(seg_path(&dir, active_id))?;

        let mut index = HashMap::new();
        let mut readers = HashMap::new();
        let mut active_end = 0u64;
        let mut max_seq = 0u64;
        for &id in &live {
            let r = OpenOptions::new().read(true).open(seg_path(&dir, id))?;
            active_end = replay_segment(&r, id, &mut index, &mut max_seq)?;
            readers.insert(id, r);
        }

        // Drop a torn tail on the active segment so future appends stay contiguous.
        active.set_len(active_end)?;

        // Any .seg on disk not in the manifest is an orphan from a crashed compaction:
        // delete it, and make sure new ids never collide with one left behind.
        let mut max_seen = active_id;
        for entry in fs::read_dir(&dir)? {
            let p = entry?.path();
            if p.extension().and_then(|e| e.to_str()) == Some("seg") {
                if let Some(id) = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    max_seen = max_seen.max(id);
                    if !live.contains(&id) {
                        let _ = fs::remove_file(&p);
                    }
                }
            }
        }

        Ok(Db {
            dir,
            index,
            active_id,
            active,
            active_size: active_end,
            readers,
            live,
            next_id: max_seen + 1,
            next_seq: max_seq + 1,
            snapshots: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    pub fn set(&mut self, key: String, value: String) -> io::Result<()> {
        let seq = self.next_seq;
        self.next_seq += 1;
        let rec = encode_record(OP_SET, seq, key.as_bytes(), value.as_bytes());
        let (seg, record_start) = self.append(&rec)?;

        let value_offset = record_start + HEADER_LEN + key.len() as u64;
        let loc = ValueLoc {
            seg,
            offset: value_offset,
            len: value.len() as u32,
        };
        // Append a new version instead of overwriting: older versions stay reachable for
        // snapshots, and the newest (last()) is what get returns.
        self.index.entry(key).or_default().push(Version {
            seq,
            kind: VersionKind::Set(loc),
        });
        self.maybe_compact()
    }

    /// Take a snapshot: freeze the current logical clock so later reads can ask for the
    /// database "as of now" even after more writes land. Returns a lightweight token.
    ///
    /// `&self` is enough even though we mutate the registry: the registry is a `Mutex`
    /// (interior mutability), and the caller invokes this under `db.read()`, so no writer
    /// is bumping `next_seq` concurrently — reading `next_seq - 1` is a consistent point.
    /// The seq is the last *committed* write (next_seq points at the next, unused one).
    pub fn snapshot(&self) -> Snapshot {
        let seq = self.next_seq - 1;
        // u32 refcount, not a bool: several readers may snapshot the same seq at once.
        *self.snapshots.lock().unwrap().entry(seq).or_insert(0) += 1;
        Snapshot {
            seq,
            registry: Arc::clone(&self.snapshots),
        }
    }

    pub fn get(&self, key: &str) -> io::Result<Option<String>> {
        // Reading "now" is just reading as of the latest committed seq.
        self.get_as_of(key, self.next_seq - 1)
    }

    /// Read a key as it existed at logical time `snap`: the value of the version with the
    /// greatest `seq <= snap`. This is the heart of MVCC (DDIA ch. 7, "Snapshot
    /// Isolation"). Versions with `seq > snap` are from the reader's future and ignored.
    pub fn get_as_of(&self, key: &str, snap: u64) -> io::Result<Option<String>> {
        // The version list is ascending by seq, so scanning from the back and taking the
        // first one within the snapshot gives the greatest seq <= snap in one pass.
        let visible = self
            .index
            .get(key)
            .and_then(|versions| versions.iter().rev().find(|v| v.seq <= snap));

        match visible {
            Some(Version {
                kind: VersionKind::Set(loc),
                ..
            }) => Ok(Some(self.read_value(loc)?)),
            // Tombstone at/before snap, or nothing visible (key first written after snap).
            _ => Ok(None),
        }
    }

    // Read a value's bytes straight from its on-disk location. Positioned I/O (`pread`)
    // moves no shared file cursor, so concurrent reads on the same segment handle don't
    // corrupt each other — no lock needed for reads.
    fn read_value(&self, loc: &ValueLoc) -> io::Result<String> {
        let r = &self.readers[&loc.seg];
        let mut buf = vec![0u8; loc.len as usize];
        r.read_exact_at(&mut buf, loc.offset)?;
        Ok(String::from_utf8(buf).unwrap())
    }

    pub fn delete(&mut self, key: &str) -> io::Result<()> {
        let seq = self.next_seq;
        self.next_seq += 1;
        let rec = encode_record(OP_DEL, seq, key.as_bytes(), &[]);
        let (seg, _) = self.append(&rec)?;
        // A delete is a versioned tombstone, not a removal: a snapshot taken before it
        // must still see the prior value, so the key's history is preserved.
        self.index
            .entry(key.to_string())
            .or_default()
            .push(Version {
                seq,
                kind: VersionKind::Delete(seg),
            });
        self.maybe_compact()
    }

    // Append a record to the active segment and fsync it; returns the (segment id,
    // start offset) where it landed. Rolls to a new segment afterwards if the active
    // one crossed SEGMENT_MAX, so the returned location still points at this record.
    fn append(&mut self, rec: &[u8]) -> io::Result<(u64, u64)> {
        let seg = self.active_id;
        let record_start = self.active_size;

        self.active.write_all(rec)?;
        self.active.sync_all()?; // fsync before the write counts as committed
        self.active_size += rec.len() as u64;

        if self.active_size >= SEGMENT_MAX {
            self.roll()?;
        }
        Ok((seg, record_start))
    }

    fn roll(&mut self) -> io::Result<()> {
        let new_id = self.next_id;
        let seg = seg_path(&self.dir, new_id);
        let active = OpenOptions::new().create(true).append(true).open(&seg)?;
        let reader = OpenOptions::new().read(true).open(&seg)?;

        // Record the new segment in the manifest before switching to it, so a crash
        // can never leave a written-to segment the manifest doesn't know about.
        let mut live = self.live.clone();
        live.push(new_id);
        write_manifest(&self.dir, &live)?;

        self.live = live;
        self.active = active;
        self.readers.insert(new_id, reader);
        self.active_id = new_id;
        self.active_size = 0;
        self.next_id = new_id + 1;
        Ok(())
    }

    // Compact once enough immutable segments have piled up. Must be called AFTER the
    // index reflects the latest write — otherwise a compaction running in the same
    // call would delete the segment holding a just-written, not-yet-indexed record.
    fn maybe_compact(&mut self) -> io::Result<()> {
        if self.live.len() > COMPACT_AFTER {
            self.compact()?;
        }
        Ok(())
    }

    /// Merge every immutable segment (all but the active) into one fresh segment, then
    /// atomically swap the manifest and drop the old segments.
    ///
    /// This is also the GC: per key it keeps only the versions a live snapshot can still
    /// reach (`versions_to_keep` against the oldest live snapshot) and discards the rest.
    /// With no snapshots live, that collapses to the single latest version per key.
    ///
    /// Crash-safe via the manifest: a crash before the swap leaves the old set live
    /// (the new segment is an ignored orphan); a crash after leaves the new set live
    /// (the old segments are ignored orphans, cleaned up on the next open).
    pub fn compact(&mut self) -> io::Result<()> {
        let immutable: Vec<u64> = self
            .live
            .iter()
            .copied()
            .filter(|&id| id != self.active_id)
            .collect();
        if immutable.is_empty() {
            return Ok(());
        }

        let comp_id = self.next_id;
        let comp_path = seg_path(&self.dir, comp_id);
        let mut comp = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&comp_path)?;

        // The oldest live snapshot decides what survives. No live snapshots -> the latest
        // committed seq, which makes versions_to_keep collapse to one version per key.
        let min_live = match self.snapshots.lock().unwrap().keys().next() {
            Some(&oldest) => oldest,
            None => self.next_seq - 1,
        };

        // Rebuild a fresh index from the kept versions. A version in an immutable segment
        // (about to be deleted) is rewritten into the compacted segment preserving its
        // ORIGINAL seq — the seq is the version's identity in time, so a snapshot must keep
        // resolving to it. A version already in the surviving active segment is kept as-is.
        let mut new_index: HashMap<String, Vec<Version>> = HashMap::new();
        let mut offset = 0u64;
        for (key, versions) in &self.index {
            let mut keep = versions_to_keep(versions, min_live);
            // A floor tombstone is reclaimable: nothing older survives, so its absence
            // reads as None just like the tombstone would. (Tombstones ABOVE the floor are
            // not in `keep`'s first slot and are preserved below.)
            if matches!(keep.first().map(|v| &v.kind), Some(VersionKind::Delete(_))) {
                keep = &keep[1..];
            }
            if keep.is_empty() {
                continue;
            }

            let mut kept_versions = Vec::with_capacity(keep.len());
            for v in keep {
                let new_kind = match &v.kind {
                    VersionKind::Set(loc) if immutable.contains(&loc.seg) => {
                        let value = self.read_value(loc)?;
                        let rec = encode_record(OP_SET, v.seq, key.as_bytes(), value.as_bytes());
                        comp.write_all(&rec)?;
                        let value_offset = offset + HEADER_LEN + key.len() as u64;
                        offset += rec.len() as u64;
                        VersionKind::Set(ValueLoc {
                            seg: comp_id,
                            offset: value_offset,
                            len: value.len() as u32,
                        })
                    }
                    VersionKind::Delete(seg) if immutable.contains(seg) => {
                        let rec = encode_record(OP_DEL, v.seq, key.as_bytes(), &[]);
                        comp.write_all(&rec)?;
                        offset += rec.len() as u64;
                        VersionKind::Delete(comp_id)
                    }
                    // Already in the surviving active segment -> keep the location as-is.
                    VersionKind::Set(loc) => VersionKind::Set(*loc),
                    VersionKind::Delete(seg) => VersionKind::Delete(*seg),
                };
                kept_versions.push(Version {
                    seq: v.seq,
                    kind: new_kind,
                });
            }
            new_index.insert(key.clone(), kept_versions);
        }
        comp.sync_all()?;

        // Atomic swap: compacted segment first (older data), then the active segment.
        let new_live = vec![comp_id, self.active_id];
        write_manifest(&self.dir, &new_live)?;

        // Commit in memory and drop the old segments.
        self.readers
            .insert(comp_id, OpenOptions::new().read(true).open(&comp_path)?);
        self.index = new_index;
        for id in &immutable {
            self.readers.remove(id);
            let _ = fs::remove_file(seg_path(&self.dir, *id));
        }
        self.live = new_live;
        self.next_id = comp_id + 1;
        Ok(())
    }

    /// Introspection snapshot for the observer tool (see `playground/observer`).
    /// Read-only; not a stability guarantee.
    #[doc(hidden)]
    pub fn debug_status(&self) -> io::Result<DebugStatus> {
        let mut segments = Vec::with_capacity(self.live.len());
        for &id in &self.live {
            let size = fs::metadata(seg_path(&self.dir, id))?.len();
            segments.push(SegInfo {
                id,
                size,
                is_active: id == self.active_id,
            });
        }
        segments.sort_by_key(|s| s.id);

        let mut index: Vec<IndexEntry> = self
            .index
            .iter()
            .filter_map(|(k, versions)| match versions.last() {
                Some(Version {
                    kind: VersionKind::Set(loc),
                    ..
                }) => Some(IndexEntry {
                    key: k.clone(),
                    seg: loc.seg,
                    offset: loc.offset,
                    len: loc.len,
                }),
                _ => None,
            })
            .collect();
        index.sort_by(|a, b| a.key.cmp(&b.key));

        Ok(DebugStatus {
            active_id: self.active_id,
            segments,
            index,
        })
    }
}

/// Read-only snapshot of internal state for the observer tool. Introspection only.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct DebugStatus {
    pub active_id: u64,
    pub segments: Vec<SegInfo>,
    pub index: Vec<IndexEntry>,
}

#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct SegInfo {
    pub id: u64,
    pub size: u64,
    pub is_active: bool,
}

#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub key: String,
    pub seg: u64,
    pub offset: u64,
    pub len: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // Each test gets its own segment directory; tests run in parallel, so a shared
    // path would let one test corrupt another's state.
    fn temp_dir(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("bedrock-test-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&p);
        p
    }

    fn count_segs(dir: &str) -> usize {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("seg"))
            .count()
    }

    #[test]
    fn set_then_get() {
        let path = temp_dir("set-get");
        let p = path.to_str().unwrap();
        let mut db = Db::open(p).unwrap();

        db.set("name".to_string(), "matheus".to_string()).unwrap();

        assert_eq!(db.get("name").unwrap().as_deref(), Some("matheus"));
        assert_eq!(db.get("missing").unwrap(), None);

        let _ = fs::remove_dir_all(p);
    }

    #[test]
    fn delete_removes_key() {
        let path = temp_dir("del");
        let p = path.to_str().unwrap();
        let mut db = Db::open(p).unwrap();

        db.set("name".to_string(), "matheus".to_string()).unwrap();
        db.delete("name").unwrap();

        assert_eq!(db.get("name").unwrap(), None);

        let _ = fs::remove_dir_all(p);
    }

    // Deletes must survive a restart: the tombstone is replayed on open, so a
    // previously SET key stays gone instead of resurrecting.
    #[test]
    fn delete_survives_reopen() {
        let path = temp_dir("del-reopen");
        let p = path.to_str().unwrap();

        {
            let mut db = Db::open(p).unwrap();
            db.set("name".to_string(), "matheus".to_string()).unwrap();
            db.set("city".to_string(), "sao-paulo".to_string()).unwrap();
            db.delete("name").unwrap();
        }

        let db = Db::open(p).unwrap();
        assert_eq!(db.get("name").unwrap(), None);
        assert_eq!(db.get("city").unwrap().as_deref(), Some("sao-paulo"));

        let _ = fs::remove_dir_all(p);
    }

    // Writing past SEGMENT_MAX must roll into new segments, and reopening must
    // discover and replay all of them — not just the first.
    #[test]
    fn rollover_and_reopen_across_segments() {
        let path = temp_dir("rollover");
        let p = path.to_str().unwrap();

        let n = 200u64;
        let big = "x".repeat(2000); // ~2 KiB values force a rollover past 64 KiB
        {
            let mut db = Db::open(p).unwrap();
            for i in 0..n {
                db.set(format!("key-{i}"), format!("{big}-{i}")).unwrap();
            }
            assert!(
                count_segs(p) > 1,
                "expected multiple segments after rollover, got {}",
                count_segs(p)
            );
        }

        // Reopen: discovery must replay every segment, so all keys survive.
        let db = Db::open(p).unwrap();
        for i in 0..n {
            assert_eq!(
                db.get(&format!("key-{i}")).unwrap(),
                Some(format!("{big}-{i}")),
                "key-{i} lost across a multi-segment reopen"
            );
        }

        let _ = fs::remove_dir_all(p);
    }

    // A crash can leave a half-written record at the tail. After such an unclean
    // recovery, a newly written key must still survive a later reopen — i.e. open()
    // must drop the torn tail so appends land contiguously.
    #[test]
    fn write_after_unclean_recovery_survives() {
        use std::io::Write as _;
        let path = temp_dir("torn-tail");
        let p = path.to_str().unwrap();

        {
            let mut db = Db::open(p).unwrap();
            db.set("a".to_string(), "1".to_string()).unwrap();
            db.set("b".to_string(), "2".to_string()).unwrap();
        }

        // Simulate a crash mid-write: append garbage to the active segment's tail.
        {
            let seg = path.join("000001.seg");
            let mut f = fs::OpenOptions::new().append(true).open(&seg).unwrap();
            f.write_all(&[0xAB; 8]).unwrap(); // not a valid record
        }

        // Reopen after the torn tail, write a new key, close.
        {
            let mut db = Db::open(p).unwrap();
            db.set("c".to_string(), "3".to_string()).unwrap();
        }

        // Reopen again: old and new keys must all be present (the torn tail was dropped).
        let db = Db::open(p).unwrap();
        assert_eq!(db.get("a").unwrap().as_deref(), Some("1"));
        assert_eq!(db.get("b").unwrap().as_deref(), Some("2"));
        assert_eq!(db.get("c").unwrap().as_deref(), Some("3"));

        let _ = fs::remove_dir_all(p);
    }

    // Many overwrites create lots of dead records and roll many segments. Compaction
    // must collapse them while preserving the latest value of every key — and the
    // result must survive a reopen (the manifest points at the compacted set).
    #[test]
    fn compaction_reclaims_dead_data_and_preserves_values() {
        let path = temp_dir("compaction");
        let p = path.to_str().unwrap();

        let n = 40u64;
        let big = "y".repeat(2000); // ~2 KiB values -> frequent rollovers
        {
            let mut db = Db::open(p).unwrap();
            for round in 0..20 {
                for i in 0..n {
                    db.set(format!("key-{i}"), format!("{big}-r{round}-{i}"))
                        .unwrap();
                }
            }
            // Without compaction this would be ~25 segments; with it, only a few.
            assert!(
                count_segs(p) <= COMPACT_AFTER + 2,
                "compaction did not collapse segments: {} live",
                count_segs(p)
            );
            for i in 0..n {
                assert_eq!(
                    db.get(&format!("key-{i}")).unwrap(),
                    Some(format!("{big}-r19-{i}"))
                );
            }
        }

        // The compacted set must replay correctly on reopen.
        let db = Db::open(p).unwrap();
        for i in 0..n {
            assert_eq!(
                db.get(&format!("key-{i}")).unwrap(),
                Some(format!("{big}-r19-{i}")),
                "key-{i} wrong after compaction + reopen"
            );
        }

        let _ = fs::remove_dir_all(p);
    }

    // A crash mid-compaction can leave a .seg on disk that the manifest never adopted.
    // Open must ignore (and clean up) that orphan and keep the real data intact.
    #[test]
    fn orphan_segment_is_ignored_on_open() {
        use std::io::Write as _;
        let path = temp_dir("orphan");
        let p = path.to_str().unwrap();

        {
            let mut db = Db::open(p).unwrap();
            db.set("a".to_string(), "1".to_string()).unwrap();
        }

        // A .seg the manifest doesn't list (as a crashed compaction would leave).
        {
            let orphan = path.join("009999.seg");
            let mut f = fs::File::create(&orphan).unwrap();
            f.write_all(b"garbage not in the manifest").unwrap();
        }

        let db = Db::open(p).unwrap();
        assert_eq!(db.get("a").unwrap().as_deref(), Some("1"));
        assert!(
            !path.join("009999.seg").exists(),
            "orphan segment should be deleted on open"
        );

        let _ = fs::remove_dir_all(p);
    }

    // The MVCC deliverable: a snapshot is frozen at the seq it was taken. Later writes —
    // even a delete — are invisible to it, while the present sees them. This is the
    // analogue of "revert to seek and watch it break": it proves old versions stay
    // reachable through a snapshot taken before them.
    #[test]
    fn snapshot_sees_a_frozen_point_in_time() {
        let path = temp_dir("snapshot-mvcc");
        let p = path.to_str().unwrap();
        let mut db = Db::open(p).unwrap();

        db.set("k".to_string(), "v0".to_string()).unwrap();
        let s = db.snapshot(); // frozen at the seq of k=v0

        db.set("k".to_string(), "v1".to_string()).unwrap();
        db.delete("k").unwrap();

        // The snapshot never moves: it still resolves k to the value it saw.
        assert_eq!(db.get_as_of("k", s.seq).unwrap().as_deref(), Some("v0"));
        // The present sees the latest write (the delete) -> key is gone.
        assert_eq!(db.get("k").unwrap(), None);

        let _ = fs::remove_dir_all(p);
    }

    // A snapshot taken before a key's first write must not see it: no version satisfies
    // seq <= snap, so the lookup yields None rather than leaking a future value.
    #[test]
    fn snapshot_predating_a_key_does_not_see_it() {
        let path = temp_dir("snapshot-predate");
        let p = path.to_str().unwrap();
        let mut db = Db::open(p).unwrap();

        let s = db.snapshot(); // empty db -> seq 0
        db.set("late".to_string(), "value".to_string()).unwrap();

        assert_eq!(db.get_as_of("late", s.seq).unwrap(), None);
        assert_eq!(db.get("late").unwrap().as_deref(), Some("value"));

        let _ = fs::remove_dir_all(p);
    }

    // 8b.4 GC: a version a live snapshot can still reach must survive compaction; once the
    // snapshot is dropped, that version becomes collectable and the pile collapses. The two
    // directions together prove the rule protects what's live and frees what's dead.
    #[test]
    fn gc_preserves_versions_a_live_snapshot_needs() {
        let path = temp_dir("gc-snapshot");
        let p = path.to_str().unwrap();
        let mut db = Db::open(p).unwrap();

        db.set("k".to_string(), "v0".to_string()).unwrap();
        let snap = db.snapshot(); // frozen at v0
        let snap_seq = snap.seq;

        // Overwrite k many times with big values to roll segments and trigger compaction
        // while the snapshot is alive (min_live = snap_seq, so v0's floor is protected).
        let big = "z".repeat(2000);
        for i in 1..=60 {
            db.set("k".to_string(), format!("{big}-v{i}")).unwrap();
        }

        // v0 survived the GC: the live snapshot still resolves k to it.
        assert_eq!(db.get_as_of("k", snap_seq).unwrap().as_deref(), Some("v0"));
        // The present sees the newest write.
        assert_eq!(db.get("k").unwrap(), Some(format!("{big}-v60")));

        // Release the snapshot, then compact again: v0 is now unreachable -> collected.
        drop(snap);
        db.compact().unwrap();

        assert_eq!(db.get_as_of("k", snap_seq).unwrap(), None); // v0 is gone
        assert_eq!(db.index.get("k").unwrap().len(), 1); // pile collapsed to the latest

        let _ = fs::remove_dir_all(p);
    }

    // Dropping a snapshot must release its refcount; the registry entry disappears once
    // the last holder is gone. This is what keeps min_live correct for the GC (8b.4).
    #[test]
    fn dropping_a_snapshot_releases_its_refcount() {
        let path = temp_dir("snapshot-refcount");
        let p = path.to_str().unwrap();
        let mut db = Db::open(p).unwrap();
        db.set("k".to_string(), "v".to_string()).unwrap();

        let s1 = db.snapshot();
        let s2 = db.snapshot(); // same seq -> refcount should be 2, one entry
        assert_eq!(db.snapshots.lock().unwrap().get(&s1.seq), Some(&2));

        drop(s2);
        assert_eq!(db.snapshots.lock().unwrap().get(&s1.seq), Some(&1));

        drop(s1);
        // Last holder gone -> the entry is removed entirely, not left at 0.
        assert!(db.snapshots.lock().unwrap().is_empty());

        let _ = fs::remove_dir_all(p);
    }
}
