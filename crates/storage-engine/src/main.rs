use storage_engine::Db;

fn main() {
    let mut db = Db::open("database.db").expect("failed to open database");
    db.set("name".to_string(), "matheus".to_string())
        .expect("failed to write");
    println!("{:?}", db.get("name"));
}
