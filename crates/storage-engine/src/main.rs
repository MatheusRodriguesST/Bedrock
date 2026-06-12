use std::collections::HashMap;
use std::fs;
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Write};


struct Db {
    index: HashMap<String, String>,
    file: fs::File
}

impl Db {
    fn open(path: &str) -> io::Result<Db>{
        let file = OpenOptions::new().create(true).read(true).append(true).open(path)?;
        let mut index = HashMap::new();
        for line in BufReader::new(&file).lines(){
            let line = line?;
            let mut parts = line.splitn(3, '\t');

            match (parts.next(), parts.next(), parts.next() ) {
                (Some("SET"), Some(key), Some(value)) => {
                    index.insert(key.to_string(), value.to_string());
                }
                _ => {}
            }
        }
        Ok(Db { index, file})   
    }

    fn set(&mut self, key: String, value: String) -> io::Result<()> {
        let log = format!("SET\t{}\t{}\n", key, value);

        self.file.write_all(log.as_bytes())?;
        self.file.flush()?;
        self.index.insert(key,value);
        Ok(())
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.index.get(key).map(|s| s.as_str())
    }
}

fn main() {
    let mut db = Db::open("database.db").expect("falhou ao abrir o banco");
    db.set("nome".to_string(), "matheus".to_string())
        .expect("Falha ao salvar");
    println!("{:?}", db.get("nome"));
    
}
