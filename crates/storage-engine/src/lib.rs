use std::collections::HashMap;
use std::fs;
use std::fs::OpenOptions;
use std::io::{self, BufReader, Read, Write};

pub struct Db {
    index: HashMap<String, String>,
    file: fs::File,
}

/// Serializa um registro no formato binário do log (ver doc do módulo).
/// `op`: `0` = SET, `1` = DEL. Para um DEL, passe `val = &[]`.
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

        loop {
            //checksum
            let mut cksum_bytes = [0u8; 4];
            match reader.read_exact(&mut cksum_bytes) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
                Err(e) => return Err(e),
            }
            let checksum_lido = u32::from_le_bytes(cksum_bytes);

            //header: op(1) + key_len(4) + val_len(4) = 9 bytes.
            let mut header = [0u8; 9];
            if reader.read_exact(&mut header).is_err() {
                break;
            }
            let op = header[0];
            let key_len = u32::from_le_bytes(header[1..5].try_into().unwrap()) as usize;
            let val_len = u32::from_le_bytes(header[5..9].try_into().unwrap()) as usize;

            //payload: key + val, know size know
            let total = key_len + val_len;
            let mut payload = vec![0u8; total];
            if reader.read_exact(&mut payload).is_err() {
                break;
            }

            let mut body = Vec::new();
            body.extend_from_slice(&header);
            body.extend_from_slice(&payload);
            if crc32fast::hash(&body) != checksum_lido {
                break;
            }

            //cut payload
            let key = String::from_utf8(payload[..key_len].to_vec()).unwrap();
            if op == 0 {
                let val = String::from_utf8(payload[key_len..].to_vec()).unwrap();
                index.insert(key, val);
            } else if op == 1 {
                index.remove(&key);
            }
        }

        Ok(Db { index, file })
    }

    pub fn set(&mut self, key: String, value: String) -> io::Result<()> {
        let rec = encode_record(0, key.as_bytes(), value.as_bytes());

        self.file.write_all(&rec)?;
        self.file.sync_all()?;
        self.index.insert(key, value);
        Ok(())
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.index.get(key).map(|s| s.as_str())
    }

    pub fn delete(&mut self, key: &str) -> io::Result<()> {
        let rec = encode_record(1, key.as_bytes(), &[]);

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

    // Cada teste precisa do seu PRÓPRIO arquivo de log: testes rodam em paralelo
    // e compartilhar "database.db" faria um teste sujar o estado do outro.
    // process::id() + nome do teste dão um caminho único; removemos resíduo de
    // execuções anteriores antes de começar.
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

        db.set("nome".to_string(), "matheus".to_string()).unwrap();

        assert_eq!(db.get("nome"), Some("matheus"));
        assert_eq!(db.get("inexistente"), None);

        let _ = fs::remove_file(p);
    }

    #[test]
    fn delete_removes_key_in_memory() {
        let path = temp_path("del-mem");
        let p = path.to_str().unwrap();
        let mut db = Db::open(p).unwrap();

        db.set("nome".to_string(), "matheus".to_string()).unwrap();
        db.delete("nome").unwrap();

        assert_eq!(db.get("nome"), None);

        let _ = fs::remove_file(p);
    }

    // TESTE DE FOGO do degrau 4: o DEL precisa sobreviver ao restart.
    // Escrevemos e deletamos, derrubamos o Db (fim do escopo), reabrimos do
    // ZERO (índice vazio -> replay le o log inteiro) e a chave deve continuar
    // ausente. Se o replay NAO processasse o DEL, o SET anterior ressuscitaria.
    // Garantia provada aqui: durabilidade a RESTART LIMPO (ainda nao a crash).
    #[test]
    fn delete_survives_reopen() {
        let path = temp_path("del-reopen");
        let p = path.to_str().unwrap();

        {
            let mut db = Db::open(p).unwrap();
            db.set("nome".to_string(), "matheus".to_string()).unwrap();
            db.set("cidade".to_string(), "sampa".to_string()).unwrap();
            db.delete("nome").unwrap();
        } // db sai de escopo aqui

        let db = Db::open(p).unwrap();
        assert_eq!(db.get("nome"), None); // o DEL sobreviveu ao replay
        assert_eq!(db.get("cidade"), Some("sampa")); // o que nao foi deletado fica

        let _ = fs::remove_file(p);
    }
}
