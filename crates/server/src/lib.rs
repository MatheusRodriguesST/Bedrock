//! A minimal HTTP front end for the Bedrock storage engine.
//!
//! The engine is synchronous and shared as `Arc<RwLock<Db>>` (N readers xor 1 writer), so
//! the server is synchronous too: one OS thread per connection, each holding a clone of
//! the `Arc`. A `GET` takes the read lock, a `PUT`/`DELETE` takes the write lock — which
//! is exactly where the engine's concurrency work shows up under real clients: concurrent
//! reads never block each other, a write serializes everyone.
//!
//! Thread-per-connection is the simplest correct model and the right fit for a workload
//! whose writes are already dominated by `fsync`. Its ceiling is the classic C10k limit
//! (one thread per idle connection); an async/epoll reactor is what you'd reach for if
//! idle connections — not the fsync barrier — became the bottleneck.

pub mod http;

use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, RwLock};

use http::Request;
use storage_engine::Db;

/// The engine shared across connection threads.
pub type SharedDb = Arc<RwLock<Db>>;

/// Accept connections forever, serving each on its own thread. Per-connection errors are
/// logged and isolated — one bad client never takes the server down.
pub fn serve(listener: TcpListener, db: SharedDb) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let db = Arc::clone(&db);
                std::thread::spawn(move || {
                    if let Err(e) = handle_connection(stream, db) {
                        eprintln!("connection error: {e}");
                    }
                });
            }
            Err(e) => eprintln!("accept error: {e}"),
        }
    }
}

/// Read one request off the socket, route it, and write one response. The socket is then
/// closed (`Connection: close`), matching the no-keep-alive subset in `http`.
pub fn handle_connection(stream: TcpStream, db: SharedDb) -> std::io::Result<()> {
    // Two handles to the same socket: a buffered reader for the request, the raw stream
    // for the response. try_clone dups the fd, so both see the same connection.
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    match http::parse_request(&mut reader) {
        Ok(Some(req)) => {
            let (status, reason, body) = route(&req, &db);
            http::write_response(&mut writer, status, reason, &body)?;
        }
        Ok(None) => {} // peer closed without sending a request
        Err(http::ParseError::Malformed) => {
            http::write_response(&mut writer, 400, "Bad Request", b"bad request\n")?;
        }
        Err(http::ParseError::TooLarge) => {
            http::write_response(&mut writer, 413, "Payload Too Large", b"body too large\n")?;
        }
        Err(http::ParseError::Io(e)) => return Err(e),
    }
    writer.flush()
}

/// Map a request to (status, reason, body). Keys are the path segment after `/keys/`,
/// taken verbatim — v1 does no percent-decoding, so keys may not contain spaces (the
/// request line is whitespace-delimited) or `/`.
fn route(req: &Request, db: &SharedDb) -> (u16, &'static str, Vec<u8>) {
    if req.method == "GET" && req.path == "/" {
        return (
            200,
            "OK",
            b"Bedrock storage engine. Try /keys/<key>.\n".to_vec(),
        );
    }

    let key = match req.path.strip_prefix("/keys/") {
        Some(k) if !k.is_empty() => k,
        _ => return (404, "Not Found", b"unknown route\n".to_vec()),
    };

    match req.method.as_str() {
        // Read under a shared lock: concurrent GETs run in parallel.
        "GET" => match db.read().unwrap().get(key) {
            Ok(Some(value)) => (200, "OK", value.into_bytes()),
            Ok(None) => (404, "Not Found", b"key not found\n".to_vec()),
            Err(_) => engine_error(),
        },
        // Write under an exclusive lock. The body is the value; it must be UTF-8 because
        // the engine stores Strings.
        "PUT" => match String::from_utf8(req.body.clone()) {
            Ok(value) => match db.write().unwrap().set(key.to_string(), value) {
                Ok(()) => (204, "No Content", Vec::new()),
                Err(_) => engine_error(),
            },
            Err(_) => (400, "Bad Request", b"value must be UTF-8\n".to_vec()),
        },
        // Idempotent: deleting an absent key still returns 204.
        "DELETE" => match db.write().unwrap().delete(key) {
            Ok(()) => (204, "No Content", Vec::new()),
            Err(_) => engine_error(),
        },
        _ => (405, "Method Not Allowed", b"method not allowed\n".to_vec()),
    }
}

fn engine_error() -> (u16, &'static str, Vec<u8>) {
    (500, "Internal Server Error", b"engine error\n".to_vec())
}
