//! A deliberately small HTTP/1.1 subset — enough to talk to `curl` and a browser, no
//! more. We parse the request line, the headers we care about (`Content-Length`), and a
//! `Content-Length`-delimited body; we answer with `Connection: close` and never reuse
//! the socket. Chunked transfer encoding, keep-alive, and pipelining are out of scope on
//! purpose: hand-rolling the wire keeps the project dependency-free and legible, the same
//! reason the on-disk format avoids a serialization framework.

use std::io::{self, BufRead, Write};

/// One parsed request. The body is raw bytes; routing decides how to interpret it.
pub struct Request {
    pub method: String,
    pub path: String,
    pub body: Vec<u8>,
}

/// Why a request failed to parse, so the caller can pick the right status.
pub enum ParseError {
    /// Syntactically invalid or truncated request -> 400.
    Malformed,
    /// Advertised body exceeds `MAX_BODY` -> 413.
    TooLarge,
    /// Transport error reading the socket; the connection is just dropped.
    Io(io::Error),
}

impl From<io::Error> for ParseError {
    fn from(e: io::Error) -> Self {
        ParseError::Io(e)
    }
}

/// Hard cap on the request body. We reject an absurd `Content-Length` up front instead of
/// pre-allocating gigabytes for bytes a client may never send (a framing-amplification DoS).
pub const MAX_BODY: usize = 64 * 1024 * 1024; // 64 MiB

/// Parse a single request from `reader`. `Ok(None)` means the peer closed before sending
/// anything (a clean idle close); `Ok(Some)` is a complete request; `Err` carries why it
/// failed so the caller can answer 400 or 413.
pub fn parse_request<R: BufRead>(reader: &mut R) -> Result<Option<Request>, ParseError> {
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(None); // peer closed without a request
    }

    // Request line: METHOD SP PATH SP HTTP-VERSION. We ignore the version.
    let mut parts = request_line.split_whitespace();
    let (method, path) = match (parts.next(), parts.next()) {
        (Some(m), Some(p)) => (m.to_string(), p.to_string()),
        _ => return Err(ParseError::Malformed),
    };

    // Headers until a blank line. We only need Content-Length to frame the body.
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break; // tolerate EOF mid-headers
        }
        let line = line.trim_end(); // drop the trailing CRLF
        if line.is_empty() {
            break; // end of headers
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().map_err(|_| ParseError::Malformed)?;
            }
        }
    }

    if content_length > MAX_BODY {
        return Err(ParseError::TooLarge);
    }

    // Read exactly the advertised body. A short read means the client under-sent it:
    // that's a malformed request (400), not a transport failure.
    let mut body = vec![0u8; content_length];
    match reader.read_exact(&mut body) {
        Ok(()) => Ok(Some(Request { method, path, body })),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Err(ParseError::Malformed),
        Err(e) => Err(ParseError::Io(e)),
    }
}

/// Write a complete response: status line, framing headers, then the body. We always send
/// `Content-Length` (so the client knows when to stop) and `Connection: close`.
pub fn write_response<W: Write>(
    w: &mut W,
    status: u16,
    reason: &str,
    body: &[u8],
) -> io::Result<()> {
    write!(
        w,
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    w.write_all(body)
}
