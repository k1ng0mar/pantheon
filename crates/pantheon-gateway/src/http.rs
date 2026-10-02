//! Pantheon's single HTTP serve surface.
//!
//! The gateway owns the ONE HTTP listener. Mounts plug in route groups:
//! the dashboard's control-plane routes + PWA assets, and the AGUI routes
//! (chat RPC, SSE stream, generative-UI blobs, voice endpoints, live-voice
//! websocket). One listener, one port, one token.
//!
//! This module is std-only (no tokio, no axum): a hand-rolled HTTP/1.1
//! server with a thread per connection. It merges the two servers it
//! replaces (`pantheon-dashboard`'s pool server and `pantheon-runtime`'s
//! AG-UI shim) and keeps the stronger hardening of each:
//! - at most [`MAX_CONNS`] in-flight connections (a flood of half-open
//!   connections must not exhaust threads/fds),
//! - a 10 s read timeout on the request head (slowloris),
//! - request heads over 32 KiB are rejected with 431 before parsing,
//! - request bodies over 1 MiB are rejected with 413 before allocation.
//!
//! [`Response`] supports buffered bodies, chunked streams (SSE log
//! tailing), raw streams (the AG-UI event stream, which is
//! connection-close terminated rather than chunked), and socket takeover
//! (the live-voice websocket upgrade, handed to tungstenite verbatim).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

/// A parsed HTTP request. Only the parts routes need.
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
    /// Like [`Response::ChunkedStream`] but without chunk framing: the
    /// server writes the head, calls `stream`, then closes the connection.
    /// The AG-UI event stream uses this (it is connection-close
    /// terminated, exactly as before the serve-surface move).
    RawStream {
        content_type: &'static str,
        stream: Box<dyn Fn(&TcpStream) + Send + Sync>,
    },
    /// The handler takes over the socket entirely (websocket upgrade).
    /// The server performs no further I/O on the stream afterwards.
    Takeover {
        run: Box<dyn FnOnce(TcpStream) + Send>,
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
    pub fn created_json(body: String) -> Self {
        Self::buffered(201, "application/json", body.into_bytes())
    }
    pub fn conflict_json(body: String) -> Self {
        Self::buffered(409, "application/json", body.into_bytes())
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
    /// 404 in the dashboard's error envelope
    /// (`{"ok":false,"error":{"code","message"}}`), for API paths. Unknown
    /// /api/* routes are API errors, not missing pages, so they get the
    /// envelope every other API error uses.
    pub fn not_found_json(code: &str, msg: &str) -> Self {
        let body = serde_json::json!({
            "ok": false,
            "error": { "code": code, "message": msg },
        });
        Self::buffered(
            404,
            "application/json",
            serde_json::to_string(&body)
                .unwrap_or_else(|_| "{}".into())
                .into_bytes(),
        )
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

/// Percent-decode a URL component. Invalid sequences are kept literally
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

/// Write one chunked frame. SSE log tailing uses this.
pub fn write_chunk(stream: &TcpStream, data: &[u8]) -> std::io::Result<()> {
    let mut s = stream;
    write!(s, "{:x}\r\n", data.len())?;
    s.write_all(data)?;
    s.write_all(b"\r\n")?;
    s.flush()
}

/// Generate the per-instance 256-bit token: 32 bytes from the OS RNG,
/// hex-encoded. Falls back to a (weaker) time+pid hash only when
/// `/dev/urandom` is unreadable, which on Linux effectively never happens.
pub fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    let ok = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| {
            use std::io::Read;
            f.read_exact(&mut bytes).map(|_| true)
        })
        .unwrap_or(false);
    if !ok {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        std::time::SystemTime::now().hash(&mut h);
        std::process::id().hash(&mut h);
        std::thread::current().id().hash(&mut h);
        let v = h.finish();
        for (i, b) in v.to_le_bytes().iter().enumerate() {
            bytes[i] = *b;
            bytes[i + 8] = b.wrapping_mul(0x9d);
            bytes[i + 16] = b.wrapping_add(0x3c);
            bytes[i + 24] = b ^ 0xa5;
        }
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Auth: one layer, one token, per-group rules preserved exactly
// ---------------------------------------------------------------------------

/// Which rule set guards a request. Declared by the mount that owns the
/// path; the rules themselves are the pre-move behavior of each surface,
/// enforced here behind the single token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthGroup {
    /// Static assets, unknown paths (which 404 in the handler): no auth.
    /// (The dashboard served these without a token before the move.)
    Public,
    /// Dashboard `/api/*`: token via `X-Pantheon-Token` header or
    /// `?token=` query. `POST`/`PUT`/`PATCH`/`DELETE` additionally require the
    /// Host header to match the bind address (DNS-rebinding guard) and
    /// reject a mismatched `Origin`/`Referer`.
    DashboardApi,
    /// AG-UI `/agui/*`: token via `Authorization: Bearer`, or
    /// `X-Pantheon-Token`, or `?token=` on `/agui/stream` only
    /// (EventSource cannot set headers). A present `Origin` must be
    /// loopback. `/agui/health` needs no token (but keeps the origin
    /// check, as before).
    Agui,
}

/// The single auth context for the listener: one token, one bind.
#[derive(Debug, Clone)]
pub struct AuthCtx {
    pub token: String,
    pub bind: String,
    pub bind_all: bool,
}

/// A route group mounted on the gateway's listener.
pub trait HttpMount: Send + Sync {
    /// The auth rule set for this request, or `None` when the path is not
    /// mine (the next mount is tried). Returning `Some(AuthGroup::Public)`
    /// for unknown paths preserves the old "404 without auth" behavior.
    fn auth_group(&self, req: &Request) -> Option<AuthGroup>;
    /// Handle an authorized request.
    fn handle(&self, req: &Request) -> Response;
}

/// Enforce the single auth layer. `Ok(())` when the request may proceed;
/// `Err(response)` carries the rejection with the exact pre-move status
/// and body shape for the group (401/403).
///
/// - [`AuthGroup::Public`]: no auth.
/// - [`AuthGroup::DashboardApi`]: token via `X-Pantheon-Token` (non-empty)
///   or `?token=`; on `POST`/`PUT`/`PATCH`/`DELETE` the dashboard's mutation
///   guard (Host match, same-authority `Origin`/`Referer`) applies too.
/// - [`AuthGroup::Agui`]: loopback-only `Origin` on every request; token
///   via `Authorization: Bearer`, `X-Pantheon-Token`, or `?token=` on
///   `/agui/stream` only; `/agui/health` needs no token.
pub fn check_auth(req: &Request, group: AuthGroup, ctx: &AuthCtx) -> Result<(), Response> {
    match group {
        AuthGroup::Public => Ok(()),
        AuthGroup::DashboardApi => {
            match token_of(req) {
                Some(t) if t == ctx.token => {}
                _ => return Err(Response::unauthorized()),
            }
            if matches!(req.method.as_str(), "POST" | "PUT" | "PATCH" | "DELETE") {
                guard_mutation(req, ctx)?;
            }
            Ok(())
        }
        AuthGroup::Agui => {
            if !origin_allowed(req.headers.get("origin").map(String::as_str)) {
                return Err(agui_forbidden());
            }
            let path = req.path.split('?').next().unwrap_or(&req.path);
            if path == "/agui/health" {
                return Ok(());
            }
            let provided = req
                .headers
                .get("authorization")
                .and_then(|v| v.strip_prefix("Bearer ").map(str::trim))
                .or_else(|| req.headers.get("x-pantheon-token").map(String::as_str))
                .or_else(|| {
                    // EventSource cannot set headers; allow ?token= on the
                    // event stream only, exactly as before the move.
                    (path == "/agui/stream")
                        .then(|| req.query.get("token").map(String::as_str))
                        .flatten()
                });
            if provided != Some(ctx.token.as_str()) {
                return Err(agui_unauthorized());
            }
            Ok(())
        }
    }
}

/// Dashboard token source: `X-Pantheon-Token` when non-empty, else
/// `?token=`. (Ported from `pantheon-dashboard`'s `token_of`.)
fn token_of(req: &Request) -> Option<&str> {
    if let Some(h) = req.headers.get("x-pantheon-token") {
        if !h.is_empty() {
            return Some(h.as_str());
        }
    }
    req.query.get("token").map(String::as_str)
}

/// The dashboard's `guard_mutation`, verbatim: DNS-rebinding / CSRF guards
/// for `POST`/`PUT`/`PATCH`/`DELETE`. The token check already ran in `check_auth`.
fn guard_mutation(req: &Request, ctx: &AuthCtx) -> Result<(), Response> {
    let host = req
        .headers
        .get("host")
        .map(String::as_str)
        .ok_or_else(|| Response::forbidden("missing Host header"))?;
    if !ctx.bind_all {
        let host_part = host.split(':').next().unwrap_or("");
        let bind_ok = host_part == ctx.bind
            || (is_loopback_bind(&ctx.bind)
                && (host_part == "localhost" || host_part == "127.0.0.1" || host_part == "::1"));
        if !bind_ok {
            return Err(Response::forbidden(
                "Host does not match the dashboard bind address",
            ));
        }
    }
    if let Some(origin) = req.headers.get("origin") {
        if authority_of(origin).as_deref() != Some(host) {
            return Err(Response::forbidden("cross-origin request rejected"));
        }
    } else if let Some(referer) = req.headers.get("referer") {
        if authority_of(referer).as_deref() != Some(host) {
            return Err(Response::forbidden("cross-origin request rejected"));
        }
    }
    Ok(())
}

fn is_loopback_bind(bind: &str) -> bool {
    bind == "127.0.0.1" || bind == "localhost" || bind == "::1"
}

/// The authority (`host[:port]`) of an absolute URL, lowercased.
fn authority_of(url: &str) -> Option<String> {
    let after_scheme = url.split("://").nth(1)?;
    Some(after_scheme.split('/').next().unwrap_or("").to_lowercase())
}

/// The AG-UI group's 401, in the exact pre-move JSON shape.
fn agui_unauthorized() -> Response {
    Response::buffered(
        401,
        "application/json",
        br#"{"error":"unauthorized: set Authorization: Bearer <token>"}"#.to_vec(),
    )
}

/// The AG-UI group's cross-origin 403, in the exact pre-move JSON shape.
fn agui_forbidden() -> Response {
    Response::buffered(
        403,
        "application/json",
        br#"{"error":"cross-origin request rejected"}"#.to_vec(),
    )
}

/// Pure: is this Origin header value acceptable for a loopback server?
/// Missing Origin (curl, scripts, non-browser clients) is allowed. A
/// present Origin must name a loopback host; anything else is a cross-origin
/// browser request and is rejected. (Ported from `pantheon-runtime`'s
/// `origin_allowed`.)
fn origin_allowed(origin: Option<&str>) -> bool {
    let origin = match origin {
        None => return true,
        Some(o) => o.trim(),
    };
    let after_scheme = match origin.split_once("://") {
        Some((_, rest)) => rest,
        None => return false, // "null", opaque origins, garbage
    };
    let host = if let Some(rest) = after_scheme.strip_prefix('[') {
        match rest.split_once(']') {
            Some((h, _)) => h,
            None => return false,
        }
    } else {
        after_scheme.split([':', '/']).next().unwrap_or("")
    };
    matches!(
        host.to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "::1"
    )
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Configuration for the gateway's single HTTP listener.
pub struct ServerConfig {
    /// e.g. `"127.0.0.1:7171"`.
    pub bind_addr: String,
    /// The one auth context for the whole listener.
    pub auth: AuthCtx,
    /// Mounts tried in order; the first returning `Some` from
    /// [`HttpMount::auth_group`] owns the request.
    pub mounts: Vec<Arc<dyn HttpMount>>,
    /// Human label for the startup line, e.g. `"pantheon serve"` or
    /// `"pantheon dashboard"`. The listener is shared by both commands;
    /// the label keeps the banner honest about which one started it.
    pub label: String,
}

/// Decrements the in-flight connection count when a handler thread exits,
/// including on panic.
struct ConnGuard {
    n: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
impl Drop for ConnGuard {
    fn drop(&mut self) {
        self.n.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Body cap for a raw request path: 1 MiB everywhere, except the voice
/// transcribe route (multi-minute voice notes as base64) and the
/// dashboard upload route (25 MiB file cap, re-checked after decode).
/// Pure so the routing decision is unit-testable without a socket.
fn body_cap_for(raw_path: &str) -> usize {
    if raw_path == crate::voice::TRANSCRIBE_PATH {
        crate::voice::MAX_VOICE_BODY_BYTES
    } else if raw_path == "/api/uploads" {
        MAX_UPLOAD_BODY
    } else {
        MAX_BODY
    }
}

/// Read one request head plus body off the socket. `Ok(None)` is an
/// unparseable request line (the caller answers 400); `Err(response)` is
/// one of the pre-parse guards: heads over [`MAX_HEADERS`] get 431,
/// bodies over the route's cap get 413, both before any parsing/allocation.
/// The cap is [`MAX_BODY`] everywhere except the voice transcribe path
/// ([`crate::voice::MAX_VOICE_BODY_BYTES`]) and the dashboard upload
/// route ([`MAX_UPLOAD_BODY`]).
fn read_request(stream: &TcpStream) -> Result<Option<Request>, Response> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return Ok(None);
    }
    let line = line.trim_end_matches(['\r', '\n']);
    let mut parts = line.splitn(3, ' ');
    let (method, target, _version) = match (parts.next(), parts.next(), parts.next()) {
        (Some(m), Some(t), Some(v)) => (m, t, v),
        _ => return Ok(None),
    };
    let method = method.to_string();
    if method != "GET"
        && method != "POST"
        && method != "PUT"
        && method != "PATCH"
        && method != "DELETE"
    {
        return Ok(None);
    }
    let (raw_path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), parse_query(q)),
        // The path stays raw here: route matching splits on '/' first and
        // percent-decodes each segment, so an id containing an encoded '/'
        // (an approval scope with a file path in its args) is not split.
        None => (target.to_string(), HashMap::new()),
    };
    let mut headers = HashMap::new();
    let mut header_bytes = 0usize;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).is_err() {
            return Ok(None);
        }
        header_bytes += h.len();
        if header_bytes > MAX_HEADERS {
            return Err(Response::ok_json(
                r#"{"error":"request headers too large"}"#.to_string(),
            ));
        }
        let h = h.trim_end_matches(['\r', '\n']);
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
        .unwrap_or(0);
    // 413 before allocation: a lying Content-Length never becomes a Vec.
    let body_cap = body_cap_for(&raw_path);
    if len > body_cap {
        return Err(Response::ok_json(
            r#"{"error":"request body too large"}"#.to_string(),
        ));
    }
    let mut body = vec![0u8; len];
    if len > 0 && reader.read_exact(&mut body).is_err() {
        return Ok(None);
    }
    Ok(Some(Request {
        method,
        path: raw_path,
        query,
        headers,
        body,
    }))
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
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

/// Serialize one response onto the socket. `ChunkedStream` and
/// `RawStream` hand the socket to the closure after the head;
/// `Takeover` hands it over wholesale and performs no further I/O.
fn write_response(mut stream: TcpStream, resp: Response) {
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
                reason(status),
                content_type,
                body.len()
            );
            for (k, v) in extra_headers {
                head.push_str(&format!("{k}: {v}\r\n"));
            }
            head.push_str("Connection: close\r\n\r\n");
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
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
        Response::RawStream {
            content_type,
            stream: run,
        } => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nConnection: close\r\n\r\n",
                content_type
            );
            if stream.write_all(head.as_bytes()).is_err() {
                return;
            }
            let _ = stream.flush();
            run(&stream);
            // The stream is connection-close terminated: no framing, no
            // trailer. Dropping the socket closes it.
        }
        Response::Takeover { run } => {
            run(stream);
        }
    }
}

/// Bind `cfg.bind_addr` and serve forever on `listener`.
///
/// Thread per connection, capped at [`MAX_CONNS`] in flight; each
/// connection gets a [`READ_TIMEOUT`] read timeout on the request head,
/// the [`MAX_HEADERS`]/[`MAX_BODY`] caps, then auth → mount dispatch: the
/// first mount whose [`HttpMount::auth_group`] returns `Some` owns the
/// request, [`check_auth`] gates it, and the mount handles it. Requests
/// no mount claims get a 404 (mounts return `Some(Public)` for unknown
/// paths, so this is a backstop).
pub fn serve_on(listener: TcpListener, cfg: ServerConfig) -> ! {
    let cfg = Arc::new(cfg);
    let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for stream in listener.incoming() {
        let s = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("gateway accept: {e}");
                continue;
            }
        };
        if in_flight.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= MAX_CONNS {
            in_flight.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            eprintln!("gateway: connection cap ({MAX_CONNS}) reached; dropping");
            continue; // dropping the stream closes the connection
        }
        let flights = Arc::clone(&in_flight);
        let cfg = Arc::clone(&cfg);
        std::thread::spawn(move || {
            let _guard = ConnGuard { n: flights };
            // Slowloris: a connection that trickles bytes must not hold a
            // thread forever waiting for the request head.
            let _ = s.set_read_timeout(Some(READ_TIMEOUT));
            // A panicking handler must not drop the connection silently: a
            // diagnostic eprintln! hitting a closed stderr (EPIPE) is
            // enough to panic a request thread, and without this the
            // client sees an empty reply instead of an honest 500.
            let resp = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                match read_request(&s) {
                    Err(r) => r, // 431 / 413 guards
                    Ok(None) => Response::bad_request("bad request"),
                    Ok(Some(req)) => {
                        let claimed = cfg
                            .mounts
                            .iter()
                            .find_map(|m| m.auth_group(&req).map(|g| (m, g)));
                        match claimed {
                            Some((mount, group)) => match check_auth(&req, group, &cfg.auth) {
                                Ok(()) => mount.handle(&req),
                                Err(r) => r,
                            },
                            None => {
                                // Backstop: no mount claimed the request. API
                                // paths get the JSON error envelope (B-15);
                                // everything else keeps the plain 404.
                                if req.path == "/api" || req.path.starts_with("/api/") {
                                    Response::not_found_json("not_found", "unknown api route")
                                } else {
                                    Response::not_found()
                                }
                            }
                        }
                    }
                }
            })) {
                Ok(resp) => resp,
                Err(_) => Response::internal("request handler panicked".to_string()),
            };
            write_response(s, resp);
        });
    }
    // `serve_on` never returns; the process owns this thread.
    std::process::exit(0);
}

/// Bind and serve. Returns `Err` only when the bind fails (with an
/// actionable message naming the address and the remedy); otherwise never
/// returns.
pub fn serve(cfg: ServerConfig) -> std::io::Result<()> {
    let bind_addr = cfg.bind_addr.clone();
    // A bind failure is the one serve error users hit routinely, and the raw
    // io::Error says neither the port nor what to do about it. Name both.
    let listener = TcpListener::bind(&bind_addr).map_err(|e| {
        let remedy = match e.kind() {
            std::io::ErrorKind::AddrInUse => format!(
                "{bind_addr} is already in use; stop that process or pick a \
                 different port (or set it in [server])"
            ),
            std::io::ErrorKind::PermissionDenied => format!(
                "not allowed to bind {bind_addr}; ports below 1024 need elevated \
                 privileges, so use a port of 1024 or above"
            ),
            std::io::ErrorKind::AddrNotAvailable => {
                format!("cannot bind {bind_addr}; that address does not exist on this host")
            }
            _ => format!("could not bind {bind_addr}"),
        };
        std::io::Error::new(e.kind(), remedy)
    })?;
    eprintln!("{} on http://{bind_addr}", cfg.label);
    serve_on(listener, cfg);
}

/// Test/embedder hook: bind an ephemeral loopback port, serve on a
/// background thread, and return `(port, token)`. The server runs until
/// the process exits.
pub fn spawn_test_server(cfg: ServerConfig) -> (u16, String) {
    let listener =
        TcpListener::bind("127.0.0.1:0").expect("spawn_test_server: bind 127.0.0.1:0 failed");
    let port = listener
        .local_addr()
        .expect("spawn_test_server: local_addr failed")
        .port();
    let token = cfg.auth.token.clone();
    std::thread::spawn(move || serve_on(listener, cfg));
    std::thread::sleep(std::time::Duration::from_millis(50));
    (port, token)
}

/// Max in-flight connections: a flood of half-open connections must not
/// exhaust threads/fds.
pub const MAX_CONNS: usize = 32;
/// Request heads over 32 KiB are rejected with 431 before parsing.
pub const MAX_HEADERS: usize = 32 * 1024;
/// Bodies over 1 MiB are rejected with 413 before allocation - except
/// `POST /agui/voice/transcribe`, which allows up to
/// [`crate::voice::MAX_VOICE_BODY_BYTES`] for multi-minute voice notes,
/// and `POST /api/uploads`, which allows up to [`MAX_UPLOAD_BODY`] to
/// match the dashboard's 25 MiB upload cap
/// (`pantheon-dashboard`'s `MAX_UPLOAD_BYTES`; keep the two in sync).
pub const MAX_BODY: usize = 1024 * 1024;
/// Body cap for the dashboard upload route: 25 MiB, mirroring
/// `pantheon-dashboard::uploads::MAX_UPLOAD_BYTES`.
pub const MAX_UPLOAD_BODY: usize = 25 * 1024 * 1024;
/// A connection that trickles bytes (slowloris) is dropped after 10 s
/// without a complete request head.
pub const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Regression tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod mutation_guard_tests {
    use super::*;

    fn ctx() -> AuthCtx {
        AuthCtx {
            token: "test-token".to_string(),
            bind: "127.0.0.1".to_string(),
            bind_all: false,
        }
    }

    fn patch_req(host: &str, origin: Option<&str>) -> Request {
        let mut headers = HashMap::new();
        headers.insert("host".to_string(), host.to_string());
        headers.insert("x-pantheon-token".to_string(), "test-token".to_string());
        if let Some(o) = origin {
            headers.insert("origin".to_string(), o.to_string());
        }
        Request {
            method: "PATCH".to_string(),
            path: "/api/runs/run-1/queue/0".to_string(),
            query: HashMap::new(),
            headers,
            body: br#"{"text":"edited"}"#.to_vec(),
        }
    }

    fn status_of(result: &Result<(), Response>) -> Option<u16> {
        match result {
            Err(Response::Buffered { status, .. }) => Some(*status),
            _ => None,
        }
    }

    /// P0 #12: a PATCH mutation whose Host does not match the bind address
    /// must be rejected (DNS-rebinding guard), like POST/PUT/DELETE.
    #[test]
    fn patch_with_mismatched_host_is_rejected() {
        let req = patch_req("attacker.example.com", None);
        let res = check_auth(&req, AuthGroup::DashboardApi, &ctx());
        assert_eq!(
            status_of(&res),
            Some(403),
            "PATCH with mismatched Host must be rejected"
        );
    }

    /// P0 #12: a PATCH mutation whose Origin authority differs from the
    /// Host must be rejected (CSRF guard).
    #[test]
    fn patch_with_cross_origin_is_rejected() {
        let req = patch_req("127.0.0.1:7171", Some("https://evil.example.com"));
        let res = check_auth(&req, AuthGroup::DashboardApi, &ctx());
        assert_eq!(
            status_of(&res),
            Some(403),
            "cross-origin PATCH must be rejected"
        );
    }

    /// A legitimate same-origin PATCH keeps working after the guard is
    /// extended: no false positives for the dashboard web UI.
    #[test]
    fn patch_same_origin_is_allowed() {
        let req = patch_req("127.0.0.1:7171", Some("http://127.0.0.1:7171"));
        assert!(check_auth(&req, AuthGroup::DashboardApi, &ctx()).is_ok());
    }

    /// Native clients (mobile app) send no Origin; a matching Host alone
    /// must still let their PATCH mutations through.
    #[test]
    fn patch_without_origin_is_allowed() {
        let req = patch_req("127.0.0.1:7171", None);
        assert!(check_auth(&req, AuthGroup::DashboardApi, &ctx()).is_ok());
    }
}
