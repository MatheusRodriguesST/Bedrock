use std::collections::HashMap;
use std::fs;
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Write};

pub struct Db {
    index: HashMap<String, String>,
    file: fs::File,
}

impl Db {
    pub fn open(path: &str) -> io::Result<Db> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(path)?;
        let mut index = HashMap::new();
        for line in BufReader::new(&file).lines() {
            let line = line?;
            let mut parts = line.splitn(3, '\t');

            match (parts.next(), parts.next(), parts.next()) {
                (Some("SET"), Some(key), Some(value)) => {
                    index.insert(key.to_string(), value.to_string());
                }
                (Some("DEL"), Some(key), None) => {
                    index.remove(key);
                }
                _ => {}
            }
        }
        Ok(Db { index, file })
    }

    pub fn set(&mut self, key: String, value: String) -> io::Result<()> {
        let log = format!("SET\t{}\t{}\n", key, value);

        self.file.write_all(log.as_bytes())?;
        self.file.flush()?;
        self.index.insert(key, value);
        Ok(())
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.index.get(key).map(|s| s.as_str())
    }

    pub fn delete(&mut self, key: &str) -> io::Result<()> {
        let log = format!("DEL\t{}\n", key);

        self.file.write_all(log.as_bytes())?;
        self.file.flush()?;
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
