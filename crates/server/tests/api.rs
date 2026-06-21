//! End-to-end test: start the real server on an ephemeral port and drive it over TCP with
//! a hand-written HTTP client. No HTTP crate on either side — the same dependency-free
//! stance as the engine. Proves the full path: socket -> parse -> engine -> response.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, RwLock};

use bedrock_server::serve;
use storage_engine::Db;

fn start_server(name: &str) -> SocketAddr {
    let dir = std::env::temp_dir().join(format!("bedrock-api-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Arc::new(RwLock::new(Db::open(dir.to_str().unwrap()).unwrap()));

    // Port 0 lets the OS pick a free port; we read it back before spawning the server.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || serve(listener, db));
    addr
}

/// Send one request and return (status code, body). Mirrors the server's no-keep-alive
/// subset: send Content-Length + Connection: close, then read one framed response.
fn request(addr: SocketAddr, method: &str, path: &str, body: &[u8]) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    stream.flush().unwrap();

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).unwrap();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();

    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = v.trim().parse().unwrap();
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).unwrap();
    (status, String::from_utf8(body).unwrap())
}

#[test]
fn put_get_delete_roundtrip() {
    let addr = start_server("roundtrip");

    // Missing key -> 404.
    assert_eq!(request(addr, "GET", "/keys/foo", b"").0, 404);

    // Store it -> 204.
    assert_eq!(request(addr, "PUT", "/keys/foo", b"bar").0, 204);

    // Read it back -> 200 "bar".
    let (status, body) = request(addr, "GET", "/keys/foo", b"");
    assert_eq!((status, body.as_str()), (200, "bar"));

    // Delete -> 204, then it's gone -> 404.
    assert_eq!(request(addr, "DELETE", "/keys/foo", b"").0, 204);
    assert_eq!(request(addr, "GET", "/keys/foo", b"").0, 404);
}

#[test]
fn unknown_route_and_method() {
    let addr = start_server("routes");
    assert_eq!(request(addr, "GET", "/nope", b"").0, 404);
    assert_eq!(request(addr, "POST", "/keys/foo", b"x").0, 405);
}

#[test]
fn bad_requests() {
    let addr = start_server("bad");
    // A non-UTF-8 body can't be a value (the engine stores Strings) -> 400.
    assert_eq!(request(addr, "PUT", "/keys/x", &[0xff, 0xfe]).0, 400);

    // An absurd Content-Length is refused up front, before allocating, with no body sent.
    let mut stream = TcpStream::connect(addr).unwrap();
    let head = "PUT /keys/x HTTP/1.1\r\nContent-Length: 999999999\r\nConnection: close\r\n\r\n";
    stream.write_all(head.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut status_line = String::new();
    BufReader::new(stream).read_line(&mut status_line).unwrap();
    assert_eq!(status_line.split_whitespace().nth(1).unwrap(), "413");
}

// The headline reason this crate exists: many readers and a writer hammering the shared
// Arc<RwLock<Db>> over real TCP must never corrupt a read or return an error. Concurrent
// GETs run under the read lock; each PUT serializes under the write lock.
#[test]
fn concurrent_readers_and_a_writer() {
    let addr = start_server("concurrent");
    assert_eq!(request(addr, "PUT", "/keys/k", b"v0").0, 204);

    let mut readers = Vec::new();
    for _ in 0..8 {
        readers.push(std::thread::spawn(move || {
            for _ in 0..50 {
                let (status, body) = request(addr, "GET", "/keys/k", b"");
                assert_eq!(status, 200);
                // Always a whole, valid value — never a torn or empty read.
                assert!(
                    body.starts_with('v') && body.len() >= 2,
                    "corrupt read: {body:?}"
                );
            }
        }));
    }
    let writer = std::thread::spawn(move || {
        for i in 1..=50 {
            assert_eq!(
                request(addr, "PUT", "/keys/k", format!("v{i}").as_bytes()).0,
                204
            );
        }
    });

    for r in readers {
        r.join().unwrap();
    }
    writer.join().unwrap();
}
