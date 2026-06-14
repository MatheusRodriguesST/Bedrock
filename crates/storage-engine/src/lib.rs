//! Bedrock — an append-only key/value storage engine with an in-memory index
//! (Bitcask-style). Writes append a length-prefixed, CRC32-checked record to the
//! active segment and fsync it; data lives in a directory of segment files, and the
//! index maps each key to its value's location (segment, offset, length).
//! See README.md for the on-disk format and durability guarantees.

use std::collections::HashMap;
use std::fs;
use std::fs::OpenOptions;
use std::io::{self, BufReader, Read, Write};
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub struct Db {
    dir: std::path::PathBuf,
    index: HashMap<String, ValueLoc>,
    active_id: u64,
    active: fs::File,
    active_size: u64,
    readers: HashMap<u64, fs::File>,
}

// Where a key's value lives in the log: byte offset of the value + its length.
struct ValueLoc {
    seg: u64,
    offset: u64,
    len: u32,
}

// Header bytes before the value: crc(4) + op(1) + key_len(4) + val_len(4).
const HEADER_LEN: u64 = 13;
const OP_SET: u8 = 0;
const OP_DEL: u8 = 1;
const SEGMENT_MAX: u64 = 64 * 1024;

// Serialize one record: [crc u32][op u8][key_len u32][val_len u32][key][val], LE.
fn encode_record(op: u8, key: &[u8], val: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(op);
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
    index: &mut HashMap<String, ValueLoc>,
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

        let mut header = [0u8; 9];
        if reader.read_exact(&mut header).is_err() {
            break;
        }
        let op = header[0];
        let key_len = u32::from_le_bytes(header[1..5].try_into().unwrap()) as usize;
        let val_len = u32::from_le_bytes(header[5..9].try_into().unwrap()) as usize;

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

        let key = String::from_utf8(payload[..key_len].to_vec()).unwrap();
        if op == OP_SET {
            let value_offset = offset + HEADER_LEN + key_len as u64;
            index.insert(
                key,
                ValueLoc {
                    seg,
                    offset: value_offset,
                    len: val_len as u32,
                },
            );
        } else if op == OP_DEL {
            index.remove(&key);
        }
        offset += 4 + 9 + key_len as u64 + val_len as u64;
    }

    Ok(offset)
}

impl Db {
    /// Open (or create) the segment directory and rebuild the index by replaying
    /// every segment in order (oldest id first, so newer writes win).
    pub fn open(path: &str) -> io::Result<Db> {
        let dir = PathBuf::from(path);
        fs::create_dir_all(&dir)?;

        // Discover existing segment ids (ascending = oldest -> newest).
        let mut ids: Vec<u64> = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let p = entry?.path();
            if p.extension().and_then(|e| e.to_str()) == Some("seg") {
                if let Some(id) = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .and_then(|s| s.parse().ok())
                {
                    ids.push(id);
                }
            }
        }
        ids.sort_unstable();

        // Replay each segment into the index, keeping a read handle per segment.
        // replay_segment returns the offset just past the last valid record; for the
        // last (active) segment that is the boundary future appends must start at.
        let mut index = HashMap::new();
        let mut readers = HashMap::new();
        let mut active_end = 0u64;
        for &id in &ids {
            let r = OpenOptions::new().read(true).open(seg_path(&dir, id))?;
            active_end = replay_segment(&r, id, &mut index)?;
            readers.insert(id, r);
        }

        // Active segment = highest existing id, or 1 for a fresh directory.
        let active_id = ids.last().copied().unwrap_or(1);
        let seg = seg_path(&dir, active_id);
        let active = OpenOptions::new().create(true).append(true).open(&seg)?;
        // A fresh directory has no reader for the active segment yet.
        if ids.is_empty() {
            readers.insert(active_id, OpenOptions::new().read(true).open(&seg)?);
        }
        // Drop a torn tail left by a crash so future appends stay contiguous
        // (no-op on a clean segment, where active_end == the file size).
        active.set_len(active_end)?;
        let active_size = active_end;

        Ok(Db {
            dir,
            index,
            active_id,
            active,
            active_size,
            readers,
        })
    }

    pub fn set(&mut self, key: String, value: String) -> io::Result<()> {
        let rec = encode_record(OP_SET, key.as_bytes(), value.as_bytes());
        let (seg, record_start) = self.append(&rec)?;

        let value_offset = record_start + HEADER_LEN + key.len() as u64;
        self.index.insert(
            key,
            ValueLoc {
                seg,
                offset: value_offset,
                len: value.len() as u32,
            },
        );
        Ok(())
    }

    pub fn get(&self, key: &str) -> io::Result<Option<String>> {
        let loc = match self.index.get(key) {
            Some(loc) => loc,
            None => return Ok(None),
        };

        // Seek to the value's offset and read exactly its bytes from disk.
        let mut r = &self.readers[&loc.seg];
        r.seek(SeekFrom::Start(loc.offset))?;
        let mut buf = vec![0u8; loc.len as usize];
        r.read_exact(&mut buf)?;

        Ok(Some(String::from_utf8(buf).unwrap()))
    }

    pub fn delete(&mut self, key: &str) -> io::Result<()> {
        let rec = encode_record(OP_DEL, key.as_bytes(), &[]);
        self.append(&rec)?;
        self.index.remove(key);
        Ok(())
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
        self.active_id += 1;
        let seg = seg_path(&self.dir, self.active_id);
        self.active = OpenOptions::new().create(true).append(true).open(&seg)?;
        self.readers
            .insert(self.active_id, OpenOptions::new().read(true).open(&seg)?);
        self.active_size = 0;
        Ok(())
    }
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
            let segs = fs::read_dir(p).unwrap().count();
            assert!(
                segs > 1,
                "expected multiple segments after rollover, got {segs}"
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
}
