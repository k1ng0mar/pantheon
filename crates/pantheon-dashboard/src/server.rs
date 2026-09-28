//! Minimal std-only HTTP/1.1 server for the dashboard.
//!
//! Pantheon is deliberately std-only (no tokio, no axum), so this is a
//! hand-rolled server: `TcpListener` plus a small fixed thread pool. It
//! only needs what the dashboard uses — GET/POST routing, path params,
//! query strings, JSON bodies, static file serving — and it stays small
//! on purpose: every line here is load-bearing.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{mpsc, Arc, Mutex};

/// A parsed HTTP request. Only the parts the dashboard routes on.
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn body_str(&self) -> &str {
        std::str::from_utf8(&self.body).unwrap_or("")
    }
}

/// An HTTP response to serialize.
pub enum Response {
    Buffered {
        status: u16,
        content_type: &'static str,
        body: Vec<u8>,
        /// Extra headers, e.g. `Content-Disposition` for downloads.
        extra_headers: Vec<(&'static str, String)>,
    },
    /// The handler takes over the socket after the headers: `stream`
    /// writes chunked frames until it returns (used for SSE log tailing).
    /// The server writes `Transfer-Encoding: chunked` and closes when the
    /// closure returns or the client goes away.
    ChunkedStream {
        content_type: &'static str,
        stream: Box<dyn Fn(&TcpStream) + Send + Sync>,
    },
}

impl Response {
    fn buffered(status: u16, content_type: &'static str, body: Vec<u8>) -> Self {
        Self::Buffered {
            status,
            content_type,
            body,
            extra_headers: Vec::new(),
        }
    }
    pub fn ok_json(body: String) -> Self {
        Self::buffered(200, "application/json", body.into_bytes())
    }
    pub fn accepted_json(body: String) -> Self {
        Self::buffered(202, "application/json", body.into_bytes())
    }
    pub fn ok_html(body: &'static str) -> Self {
        Self::buffered(200, "text/html; charset=utf-8", body.as_bytes().to_vec())
    }
    pub fn ok_css(body: &'static str) -> Self {
        Self::buffered(200, "text/css; charset=utf-8", body.as_bytes().to_vec())
    }
    pub fn ok_js(body: &'static str) -> Self {
        Self::buffered(
            200,
            "application/javascript; charset=utf-8",
            body.as_bytes().to_vec(),
        )
    }
    /// Arbitrary static bytes with an explicit content type (manifest, icons).
    pub fn ok_bytes(content_type: &'static str, body: &'static [u8]) -> Self {
        Self::buffered(200, content_type, body.to_vec())
    }
    /// A file download: sets `Content-Disposition: attachment`.
    pub fn download(filename: &str, content_type: &'static str, body: Vec<u8>) -> Self {
        Self::Buffered {
            status: 200,
            content_type,
            body,
            extra_headers: vec![(
                "Content-Disposition",
                format!("attachment; filename=\"{filename}\""),
            )],
        }
    }
    fn text(status: u16, msg: &str) -> Self {
        Self::buffered(status, "text/plain; charset=utf-8", msg.as_bytes().to_vec())
    }
    pub fn bad_request(msg: &str) -> Self {
        Self::text(400, msg)
    }
    pub fn unauthorized() -> Self {
        Self::text(401, "missing or invalid dashboard token")
    }
    pub fn not_found() -> Self {
        Self::text(404, "not found")
    }
    pub fn method_not_allowed() -> Self {
        Self::text(405, "method not allowed")
    }
    pub fn forbidden(msg: &str) -> Self {
        Self::text(403, msg)
    }
    pub fn internal(msg: String) -> Self {
        Self::text(500, &msg)
    }
}

/// Percent-decode a URL component. Invalid sequences are kept literally —
/// a malformed id must not 500 the request.
pub fn percent_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        if b[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(b[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Parse `a=1&b=2` query strings (or urlencoded bodies).
pub fn parse_query(s: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in s.split('&') {
        if pair.is_empty() {
            continue;
        }
        match pair.split_once('=') {
            Some((k, v)) => {
                map.insert(percent_decode(k), percent_decode(v));
            }
            None => {
                map.insert(percent_decode(pair), String::new());
            }
        }
    }
    map
}

fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let line = line.trim_end_matches(|c| c == '\r' || c == '\n');
    let mut parts = line.splitn(3, ' ');
    let method = parts.next()?.to_string();
    let target = parts.next()?;
    let _version = parts.next()?;
    if method != "GET" && method != "POST" && method != "PUT" && method != "DELETE" {
        return None;
    }
    let (raw_path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), parse_query(q)),
        // The path stays raw here: route matching splits on '/' first and
        // percent-decodes each segment, so an id containing an encoded '/'
        // (an approval scope with a file path in its args) is not split.
        None => (target.to_string(), HashMap::new()),
    };
    let mut headers = HashMap::new();
    loop {
        let mut h = String::new();
        reader.read_line(&mut h).ok()?;
        let h = h.trim_end_matches(|c| c == '\r' || c == '\n');
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
        .min(1 << 20); // 1 MiB cap: approval decisions are tiny; this is a guard, not a limit anyone hits.
    let mut body = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut body).ok()?;
    }
    Some(Request {
        method,
        path: raw_path,
        query,
        headers,
        body,
    })
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

/// Write one chunked frame. The SSE log tailer uses this.
pub fn write_chunk(stream: &TcpStream, data: &[u8]) -> std::io::Result<()> {
    let mut s = stream;
    write!(s, "{:x}\r\n", data.len())?;
    s.write_all(data)?;
    s.write_all(b"\r\n")?;
    s.flush()
}

fn write_response(mut stream: &TcpStream, resp: &Response) {
    match resp {
        Response::Buffered {
            status,
            content_type,
            body,
            extra_headers,
        } => {
            let mut head = format!(
                "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n",
                status,
                reason(*status),
                content_type,
                body.len()
            );
            for (k, v) in extra_headers {
                head.push_str(&format!("{k}: {v}\r\n"));
            }
            head.push_str("Connection: close\r\n\r\n");
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body);
            let _ = stream.flush();
        }
        Response::ChunkedStream {
            content_type,
            stream: run,
        } => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nCache-Control: no-cache\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                content_type
            );
            if stream.write_all(head.as_bytes()).is_err() {
                return;
            }
            run(&stream);
            // Terminal zero-chunk; a client that already went away makes
            // this fail silently, which is fine.
            let _ = write_chunk(&stream, &[]);
        }
    }
}

/// Serve forever on `listener` with `POOL` worker threads.
///
/// The handler is `Arc`'d and must be `Send + Sync`: request handling is
/// read-only against the ledger except for approval decisions, which take
/// the Supervisor's own locks.
pub fn serve<H>(listener: TcpListener, handler: H) -> !
where
    H: Fn(&Request) -> Response + Send + Sync + 'static,
{
    const POOL: usize = 8;
    let handler = Arc::new(handler);
    let (tx, rx) = mpsc::channel::<TcpStream>();
    let rx = Arc::new(Mutex::new(rx));
    for _ in 0..POOL {
        let rx = Arc::clone(&rx);
        let handler = Arc::clone(&handler);
        std::thread::spawn(move || loop {
            let stream = {
                let rx = rx.lock().expect("dashboard pool channel");
                rx.recv()
            };
            let Ok(stream) = stream else { break };
            let resp = match read_request(&stream) {
                Some(req) => handler(&req),
                None => Response::method_not_allowed(),
            };
            write_response(&stream, &resp);
        });
    }
    for stream in listener.incoming() {
        if let Ok(s) = stream {
            let _ = tx.send(s);
        }
    }
    // `serve` never returns; the process owns this thread.
    std::process::exit(0);
}

#[cfg(test)]
mod invariant_tests {
    use super::*;

    #[test]
    fn query_parsing_decodes_pairs() {
        let q = parse_query("limit=50&status=awaiting%20x&flag");
        assert_eq!(q.get("limit").map(String::as_str), Some("50"));
        assert_eq!(q.get("status").map(String::as_str), Some("awaiting x"));
        assert_eq!(q.get("flag").map(String::as_str), Some(""));
    }

    #[test]
    fn percent_decode_keeps_bad_sequences() {
        assert_eq!(percent_decode("a%2Fb%ZZc+d"), "a/b%ZZc d");
    }

    #[test]
    fn reason_covers_used_statuses() {
        for s in [200, 202, 400, 401, 403, 404, 405, 500] {
            assert_ne!(reason(s), "Unknown");
        }
    }
}
