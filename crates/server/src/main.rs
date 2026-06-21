//! Bedrock server binary: `bedrock-server [ADDR] [DB_PATH]`.
//! Defaults to `127.0.0.1:7878` and a `bedrock.db` directory in the working dir.

use std::net::TcpListener;
use std::sync::{Arc, RwLock};

use bedrock_server::serve;
use storage_engine::Db;

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:7878".to_string());
    let path = args.next().unwrap_or_else(|| "bedrock.db".to_string());

    let db = Arc::new(RwLock::new(Db::open(&path)?));
    let listener = TcpListener::bind(&addr)?;
    println!("Bedrock listening on http://{addr} (data: {path})");
    serve(listener, db);
    Ok(())
}
