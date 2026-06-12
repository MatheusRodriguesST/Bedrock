use std::collections::HashMap;
use std::fs;
use std::fs::OpenOptions;
use std::io::{self, Write};

struct Db {
    index: HashMap<String, String>,
    file: fs::File
}

impl Db {
    fn open(path: &str) -> io::Result<Db>{
        let file = OpenOptions::new().create(true).read(true).append(true).open(path)?;
        let index = HashMap::new();
        Ok(Db { index, file})   
    }

    fn set(&mut self, key: String, value: String) -> io::Result<()> {
        let log = format!("SET\t{}\t{}\n", key, value);

        self.file.write_all(log.as_bytes())?;
        self.file.flush()?;
        self.index.insert(key,value);
        Ok(())
    }
}

fn main() {
    let mut db = Db::open("database.db").expect("falhou ao abrir o banco");
    db.set("nome".to_string(), "matheus".to_string())
        .expect("Falha ao salvar");
    
}
