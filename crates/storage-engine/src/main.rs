use storage_engine::Db;

fn main() {
    let mut db = Db::open("database.db").expect("falhou ao abrir o banco");
    db.set("nome".to_string(), "matheus".to_string())
        .expect("Falha ao salvar");
    println!("{:?}", db.get("nome"));
}
