//! Bedrock — an append-only key/value storage engine with an in-memory index
//! (Bitcask-style). Writes append a length-prefixed, CRC32-checked record to a log
//! and fsync it; the index maps each key to the on-disk offset of its value.
//! See README.md for the on-disk format and durability guarantees.

use std::collections::HashMap;
use std::fs;
use std::fs::OpenOptions;
use std::io::{self, BufReader, Read, Write};
use std::io::{Seek, SeekFrom};

pub struct Db {
    index: HashMap<String, ValueLoc>,
    file: fs::File,
}

// Where a key's value lives in the log: byte offset of the value + its length.
struct ValueLoc {
    offset: u64,
    len: u32,
}

// Header bytes before the value: crc(4) + op(1) + key_len(4) + val_len(4).
const HEADER_LEN: u64 = 13;
const OP_SET: u8 = 0;
const OP_DEL: u8 = 1;

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

impl Db {
    pub fn open(path: &str) -> io::Result<Db> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)?;

        let mut index = HashMap::new();
        let mut reader = BufReader::new(&file);
        let mut offset: u64 = 0;

        loop {
            let mut cksum_bytes = [0u8; 4];
            match reader.read_exact(&mut cksum_bytes) {
                Ok(()) => {}
                // EOF here = clean end of log.
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
            let stored_checksum = u32::from_le_bytes(cksum_bytes);

            // op(1) + key_len(4) + val_len(4) = 9 bytes. A short read = torn tail.
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

            // Recompute the CRC over the exact bytes encode_record hashed.
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
                        offset: value_offset,
                        len: val_len as u32,
                    },
                );
            } else if op == OP_DEL {
                index.remove(&key);
            }
            offset += 4 + 9 + key_len as u64 + val_len as u64;
        }

        Ok(Db { index, file })
    }

    pub fn set(&mut self, key: String, value: String) -> io::Result<()> {
        let rec = encode_record(OP_SET, key.as_bytes(), value.as_bytes());

        // The record starts at the current end of file (writes are append-only).
        let record_start = self.file.metadata()?.len();
        self.file.write_all(&rec)?;
        self.file.sync_all()?; // fsync before the write counts as committed

        let value_offset = record_start + HEADER_LEN + key.len() as u64;
        self.index.insert(
            key,
            ValueLoc {
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
        (&self.file).seek(SeekFrom::Start(loc.offset))?;
        let mut buf = vec![0u8; loc.len as usize];
        (&self.file).read_exact(&mut buf)?;
        Ok(Some(String::from_utf8(buf).unwrap()))
    }

    pub fn delete(&mut self, key: &str) -> io::Result<()> {
        let rec = encode_record(OP_DEL, key.as_bytes(), &[]);

        self.file.write_all(&rec)?;
        self.file.sync_all()?;
        self.index.remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // Each test gets its own log file: tests run in parallel, so a shared path
    // would let one test corrupt another's state.
    fn temp_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("bedrock-test-{}-{}.db", std::process::id(), name));
        let _ = fs::remove_file(&p);
        p
    }

    #[test]
    fn set_then_get() {
        let path = temp_path("set-get");
        let p = path.to_str().unwrap();
        let mut db = Db::open(p).unwrap();

        db.set("name".to_string(), "matheus".to_string()).unwrap();

        assert_eq!(db.get("name").unwrap().as_deref(), Some("matheus"));
        assert_eq!(db.get("missing").unwrap(), None);

        let _ = fs::remove_file(p);
    }

    #[test]
    fn delete_removes_key() {
        let path = temp_path("del");
        let p = path.to_str().unwrap();
        let mut db = Db::open(p).unwrap();

        db.set("name".to_string(), "matheus".to_string()).unwrap();
        db.delete("name").unwrap();

        assert_eq!(db.get("name").unwrap(), None);

        let _ = fs::remove_file(p);
    }

    // Deletes must survive a restart: the tombstone is replayed on open, so a
    // previously SET key stays gone instead of resurrecting.
    #[test]
    fn delete_survives_reopen() {
        let path = temp_path("del-reopen");
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

        let _ = fs::remove_file(p);
    }
}
