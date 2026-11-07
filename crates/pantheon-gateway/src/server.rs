//! Gateway server merged group.

pub mod http {
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
        /// JSON error body with a real HTTP error status. The pre-parse
        /// guards used to return `ok_json` (200) with an error body, so a
        /// client that checked only the status saw oversize requests
        /// succeed; the docstrings already promised 431/413.
        pub fn status_json(status: u16, body: String) -> Self {
            Self::buffered(status, "application/json", body.into_bytes())
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
            // Fail closed: a predictable token is worse than no
            // token. Panic at startup rather than mint a guessable one.
            panic!("cannot read /dev/urandom to generate the dashboard token");
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
        /// Dashboard `/api/*`: token via `X-Pantheon-Token` header, or a
        /// session cookie minted by `POST /api/session` (see below). The
        /// token never appears in a URL: URLs leak into browser history,
        /// server logs, and `Referer` headers. `POST`/`PUT`/`PATCH`/`DELETE`
        /// additionally require the Host header to match the bind address
        /// (DNS-rebinding guard) and reject a mismatched `Origin`/`Referer`.
        DashboardApi,
        /// AG-UI `/agui/*`: token via `Authorization: Bearer`, or
        /// `X-Pantheon-Token`, or the session cookie (EventSource and
        /// WebSocket clients cannot set headers, but the browser sends the
        /// cookie automatically). A present `Origin` must be loopback.
        /// `/agui/health` needs no token (but keeps the origin check, as
        /// before).
        Agui,
    }

    /// Short-lived browser sessions minted from the per-instance token.
    /// `POST /api/session` with `X-Pantheon-Token` returns an `HttpOnly`
    /// cookie holding a random session id (not the token itself); the id
    /// lives here for [`SESSION_TTL_SECS`] and is accepted by [`check_auth`]
    /// wherever the token is. This is what lets header-less browser
    /// clients (EventSource streams, downloads, WebSockets) authenticate
    /// without putting the token in the URL. No `Secure` flag: the
    /// dashboard is plain HTTP on loopback, and `Secure` would stop the
    /// cookie being sent at all there.
    pub type SessionStore =
        std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>>;

    /// Session cookie name and lifetime (1 hour, fixed from minting).
    pub const SESSION_COOKIE: &str = "pantheon_session";
    pub const SESSION_TTL_SECS: u64 = 3600;

    /// The single auth context for the listener: one token, one bind.
    #[derive(Debug, Clone)]
    pub struct AuthCtx {
        pub token: String,
        pub bind: String,
        pub bind_all: bool,
        pub sessions: SessionStore,
    }

    impl AuthCtx {
        pub fn new(token: String, bind: String, bind_all: bool) -> Self {
            Self {
                token,
                bind,
                bind_all,
                sessions: SessionStore::default(),
            }
        }

        /// Mint a session id valid for [`SESSION_TTL_SECS`].
        pub fn mint_session(&self) -> String {
            let id = generate_token();
            if let Ok(mut sessions) = self.sessions.lock() {
                sessions.insert(
                    id.clone(),
                    std::time::Instant::now() + std::time::Duration::from_secs(SESSION_TTL_SECS),
                );
            }
            id
        }

        /// Is the request's session cookie a live minted session? Expired
        /// ids are pruned on sight so the map cannot grow without bound.
        pub fn valid_session(&self, req: &Request) -> bool {
            let Some(cookie) = req.headers.get("cookie") else {
                return false;
            };
            let Some(id) = cookie.split(';').find_map(|part| {
                part.trim()
                    .strip_prefix(&format!("{SESSION_COOKIE}="))
                    .map(str::to_string)
            }) else {
                return false;
            };
            let Ok(mut sessions) = self.sessions.lock() else {
                return false;
            };
            sessions.retain(|_, exp| *exp > std::time::Instant::now());
            sessions.contains_key(&id)
        }

        /// Drop one session id (logout); a no-op for unknown ids.
        pub fn revoke_session(&self, id: &str) {
            if let Ok(mut sessions) = self.sessions.lock() {
                sessions.remove(id);
            }
        }

        /// Parse this request's session cookie value, if any.
        pub fn session_cookie(&self, req: &Request) -> Option<String> {
            req.headers.get("cookie").and_then(|cookie| {
                cookie.split(';').find_map(|part| {
                    part.trim()
                        .strip_prefix(&format!("{SESSION_COOKIE}="))
                        .map(str::to_string)
                })
            })
        }
    }

    /// `POST /api/session` mints a session cookie from the token header;
    /// `DELETE /api/session` revokes it. Handled in `serve_on` before mount
    /// dispatch (the mounts never see this path): it is the one endpoint
    /// that turns a token into a cookie, so it cannot itself be gated by
    /// the cookie. Minting still requires the token header plus the same
    /// mutation guard (`Host` match, same-authority `Origin`/`Referer`) as
    /// every other dashboard mutation.
    pub fn handle_session(ctx: &AuthCtx, req: &Request) -> Response {
        match req.method.as_str() {
            "POST" => {
                let ok = req.headers.get("x-pantheon-token").map(String::as_str)
                    == Some(ctx.token.as_str());
                if !ok {
                    return Response::unauthorized();
                }
                if let Err(r) = guard_mutation(req, ctx) {
                    return r;
                }
                let id = ctx.mint_session();
                Response::Buffered {
                status: 200,
                content_type: "application/json",
                body: br#"{"ok":true}"#.to_vec(),
                extra_headers: vec![(
                    "Set-Cookie",
                    format!(
                        "{SESSION_COOKIE}={id}; HttpOnly; SameSite=Strict; Path=/; Max-Age={SESSION_TTL_SECS}"
                    ),
                )],
            }
            }
            "DELETE" => {
                if let Some(id) = ctx.session_cookie(req) {
                    ctx.revoke_session(&id);
                }
                Response::Buffered {
                    status: 200,
                    content_type: "application/json",
                    body: br#"{"ok":true}"#.to_vec(),
                    extra_headers: vec![(
                        "Set-Cookie",
                        format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"),
                    )],
                }
            }
            _ => Response::method_not_allowed(),
        }
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
    ///   or a live session cookie. On `POST`/`PUT`/`PATCH`/`DELETE` the
    ///   dashboard's mutation guard (Host match, same-authority
    ///   `Origin`/`Referer`) applies too.
    /// - [`AuthGroup::Agui`]: loopback-only `Origin` on every request; token
    ///   via `Authorization: Bearer`, `X-Pantheon-Token`, or a live session
    ///   cookie; `/agui/health` needs no token.
    pub fn check_auth(req: &Request, group: AuthGroup, ctx: &AuthCtx) -> Result<(), Response> {
        match group {
            AuthGroup::Public => Ok(()),
            AuthGroup::DashboardApi => {
                let header_ok = token_of(req) == Some(ctx.token.as_str());
                if !header_ok && !ctx.valid_session(req) {
                    return Err(Response::unauthorized());
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
                // EventSource and WebSocket clients cannot set headers;
                // they authenticate with the session cookie instead.
                let provided = req
                    .headers
                    .get("authorization")
                    .and_then(|v| v.strip_prefix("Bearer ").map(str::trim))
                    .or_else(|| req.headers.get("x-pantheon-token").map(String::as_str));
                if provided != Some(ctx.token.as_str()) && !ctx.valid_session(req) {
                    return Err(agui_unauthorized());
                }
                Ok(())
            }
        }
    }

    /// Dashboard token source: the `X-Pantheon-Token` header when
    /// non-empty. The query string is deliberately NOT a source: a token
    /// in a URL leaks into logs, history, and referers. (Ported from
    /// `pantheon-dashboard`'s `token_of`, minus its query fallback.)
    fn token_of(req: &Request) -> Option<&str> {
        req.headers.get("x-pantheon-token").and_then(|h| {
            if h.is_empty() {
                None
            } else {
                Some(h.as_str())
            }
        })
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
                    && (host_part == "localhost"
                        || host_part == "127.0.0.1"
                        || host_part == "::1"));
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
        if raw_path == crate::voice_group::voice::TRANSCRIBE_PATH {
            crate::voice_group::voice::MAX_VOICE_BODY_BYTES
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
    /// ([`crate::voice_group::voice::MAX_VOICE_BODY_BYTES`]) and the dashboard upload
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
                return Err(Response::status_json(
                    431,
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
            return Err(Response::status_json(
                413,
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
                            if req.path == "/api/session" {
                                return handle_session(&cfg.auth, &req);
                            }
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
        // Report the port actually bound, not just the requested one: a
        // `:0` (ephemeral) bind used to print `http://host:0`, a URL
        // nothing could connect to, and remotes refused port 0 outright.
        let actual = listener
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| bind_addr.clone());
        eprintln!("{} on http://{actual}", cfg.label);
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
    /// [`crate::voice_group::voice::MAX_VOICE_BODY_BYTES`] for multi-minute voice notes,
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
            AuthCtx::new("test-token".to_string(), "127.0.0.1".to_string(), false)
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

        fn read_request_from(written: &[u8]) -> Result<Option<Request>, Response> {
            use std::io::Write;
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test listener");
            let addr = listener.local_addr().expect("listener addr");
            let handle = std::thread::spawn(move || {
                let (stream, _) = listener.accept().expect("accept");
                read_request(&stream)
            });
            let mut client = std::net::TcpStream::connect(addr).expect("connect");
            client.write_all(written).expect("write request");
            handle.join().expect("server thread")
        }

        /// Oversize heads used to answer 200 with an error body, contradicting
        /// the docstring and the reason() table.
        #[test]
        fn oversize_headers_get_431_not_200() {
            let big = "x-pad: ".to_string() + &"a".repeat(40_000) + "\r\n\r\n";
            let req = format!("GET /api/health HTTP/1.1\r\n{big}");
            match read_request_from(req.as_bytes()) {
                Err(Response::Buffered { status, .. }) => assert_eq!(status, 431),
                _ => panic!("expected 431 guard"),
            }
        }

        #[test]
        fn oversize_body_get_413_not_200() {
            let req = b"POST /api/runs HTTP/1.1\r\nContent-Length: 2000000\r\n\r\n";
            match read_request_from(req) {
                Err(Response::Buffered { status, .. }) => assert_eq!(status, 413),
                _ => panic!("expected 413 guard"),
            }
        }
    }

    // ---------------------------------------------------------------------------
    // Session-cookie tests (token-in-URL removal)
    // ---------------------------------------------------------------------------

    #[cfg(test)]
    mod session_tests {
        use super::*;

        fn ctx() -> AuthCtx {
            AuthCtx::new("test-token".to_string(), "127.0.0.1".to_string(), false)
        }

        fn req(method: &str, headers: &[(&str, &str)], query: &[(&str, &str)]) -> Request {
            Request {
                method: method.to_string(),
                path: "/api/runs".to_string(),
                query: query
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                headers: headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                body: Vec::new(),
            }
        }

        fn set_cookie(res: &Response) -> Option<String> {
            match res {
                Response::Buffered { extra_headers, .. } => extra_headers
                    .iter()
                    .find(|(k, _)| *k == "Set-Cookie")
                    .map(|(_, v)| v.clone()),
                _ => None,
            }
        }

        /// A token in the query string no longer authenticates: URLs leak
        /// into logs, history, and referers.
        #[test]
        fn query_token_is_rejected() {
            let r = req("GET", &[], &[("token", "test-token")]);
            assert!(check_auth(&r, AuthGroup::DashboardApi, &ctx()).is_err());
        }

        /// A minted session cookie authenticates dashboard API requests.
        #[test]
        fn session_cookie_is_accepted() {
            let ctx = ctx();
            let id = ctx.mint_session();
            let r = req("GET", &[("cookie", &format!("{SESSION_COOKIE}={id}"))], &[]);
            assert!(check_auth(&r, AuthGroup::DashboardApi, &ctx).is_ok());
        }

        /// An unminted cookie value authenticates nothing.
        #[test]
        fn unknown_session_cookie_is_rejected() {
            let r = req("GET", &[("cookie", "pantheon_session=nope")], &[]);
            assert!(check_auth(&r, AuthGroup::DashboardApi, &ctx()).is_err());
        }

        /// Minting requires the token header and returns a session cookie;
        /// deleting revokes it.
        #[test]
        fn session_mint_and_revoke_round_trip() {
            let ctx = ctx();
            let mint = req(
                "POST",
                &[
                    ("host", "127.0.0.1:7171"),
                    ("x-pantheon-token", "test-token"),
                ],
                &[],
            );
            let res = handle_session(&ctx, &mint);
            let cookie = set_cookie(&res).expect("mint must set a cookie");
            assert!(cookie.starts_with("pantheon_session="), "{cookie}");
            assert!(cookie.contains("HttpOnly"), "{cookie}");
            let value = cookie
                .split(';')
                .next()
                .unwrap()
                .trim_start_matches("pantheon_session=")
                .to_string();
            assert_ne!(
                value, "test-token",
                "cookie must not carry the token itself"
            );
            let authed = req(
                "GET",
                &[("cookie", &format!("{SESSION_COOKIE}={value}"))],
                &[],
            );
            assert!(check_auth(&authed, AuthGroup::DashboardApi, &ctx).is_ok());
            let del = req(
                "DELETE",
                &[("cookie", &format!("{SESSION_COOKIE}={value}"))],
                &[],
            );
            let _ = handle_session(&ctx, &del);
            assert!(check_auth(&authed, AuthGroup::DashboardApi, &ctx).is_err());
        }

        /// Minting without the token header is a 401, not a cookie.
        #[test]
        fn session_mint_without_token_is_unauthorized() {
            let res = handle_session(&ctx(), &req("POST", &[("host", "127.0.0.1:7171")], &[]));
            assert!(set_cookie(&res).is_none());
            match res {
                Response::Buffered { status, .. } => assert_eq!(status, 401),
                _ => panic!("expected a buffered 401"),
            }
        }
    }
}
pub mod mcp_boot {
    //! MCP health check at gateway boot (F-8).
    //!
    //! A dead MCP server used to surface only as a mid-turn tool error. This
    //! module handshakes every enabled, consented MCP server once at startup
    //! with a timeout and logs the outcome, so a broken server is a startup
    //! warning instead of a surprise mid-conversation.
    //!
    //! The smaller surface was picked: log lines at startup. The dashboard's
    //! `GET /api/mcp/health` (the session launcher's live snapshot) stays the
    //! live view; dashboard surfacing of this boot report is another leaf's job.
    //!
    //! Hook (one line, in the gateway's boot path - `pantheon gateway run`):
    //! ```ignore
    //! pantheon_gateway::mcp_boot::check_and_log(&data_dir, std::time::Duration::from_secs(10));
    //! ```
    //!
    //! Safety: only servers the operator already consented to are probed:
    //! the canonical bundled recipe (exact transport/command/args/env/url,
    //! never a mere bundled name) and servers with an approval record. An
    //! unapproved third-party server is never spawned by a health check; it is
    //! reported as waiting for approval.

    use pantheon_mcp::client::{McpClient, McpServerConfig};
    use pantheon_mcp::manager::{McpServerSpec, McpTransport};
    use std::collections::HashMap;
    use std::path::Path;
    use std::time::Duration;

    /// One server's boot handshake outcome.
    #[derive(Debug, Clone)]
    pub struct McpBootHealth {
        pub name: String,
        pub transport: &'static str,
        pub target: String,
        pub ok: bool,
        /// Human detail: negotiated version on success, the failure on error.
        pub detail: String,
    }

    /// Configured `[mcp.servers.<name>]` entries, enabled ones only.
    /// The `[mcp]` master switch gates the whole set: `enabled = false`
    /// or `PANTHEON_MCP_ENABLED=0` yields no specs, so boot never probes
    /// a server the operator switched off.
    fn config_specs(data_dir: &Path) -> Vec<McpServerSpec> {
        if std::env::var("PANTHEON_MCP_ENABLED")
            .map(|v| v == "0")
            .unwrap_or(false)
        {
            return Vec::new();
        }
        let raw = std::fs::read_to_string(data_dir.join("config.toml")).unwrap_or_default();
        let cfg: Result<pantheon_api::config::Config, _> = toml::from_str(&raw);
        let (master, servers) = cfg
            .map(|c| {
                (
                    c.mcp.as_ref().and_then(|m| m.enabled).unwrap_or(true),
                    c.mcp
                        .map(|m| m.servers.into_iter().collect::<Vec<_>>())
                        .unwrap_or_default(),
                )
            })
            .unwrap_or((true, Vec::new()));
        if !master {
            return Vec::new();
        }
        servers
            .into_iter()
            .filter(|(_, e)| e.enabled)
            .filter_map(|(name, e)| McpServerSpec::from_entry(&name, &e))
            .collect()
    }

    /// Convertible, enabled declaration servers not already covered by a
    /// config entry: boot probes the same merged view sessions use.
    fn declaration_specs(data_dir: &Path) -> Vec<McpServerSpec> {
        let mut out = Vec::new();
        for decl in pantheon_migration::read_mcp_declarations(data_dir) {
            for s in &decl.servers {
                if !s.enabled {
                    continue;
                }
                let transport = match s.transport.as_str() {
                    "stdio" => McpTransport::Stdio,
                    "sse" => McpTransport::Sse,
                    "http" => McpTransport::Http,
                    _ => continue,
                };
                let command = s.command.clone().filter(|c| !c.trim().is_empty());
                let url = s.url.clone().filter(|u| !u.trim().is_empty());
                let ready = match transport {
                    McpTransport::Stdio => command.is_some(),
                    _ => url.is_some(),
                };
                if !ready {
                    continue;
                }
                out.push(McpServerSpec {
                    name: s.name.clone(),
                    transport,
                    command,
                    args: s.args.clone(),
                    env: s
                        .requires_env
                        .iter()
                        .map(|v| (v.clone(), format!("env:{v}")))
                        .collect(),
                    url,
                    enabled: s.enabled,
                    timeout: Duration::from_secs(30),
                    headers: Default::default(),
                    auth: pantheon_mcp::McpAuth::None,
                    scopes: Vec::new(),
                    client_id: None,
                });
            }
        }
        out
    }
    /// (unknown transport/auth, missing command/url): reported as
    /// unhealthy at boot instead of silently dropped.
    fn dropped_declaration_specs(data_dir: &Path) -> Vec<McpBootHealth> {
        let mut out = Vec::new();
        for decl in pantheon_migration::read_mcp_declarations(data_dir) {
            for s in &decl.servers {
                if !s.enabled {
                    continue;
                }
                let convertible = matches!(s.transport.as_str(), "stdio" | "sse" | "http")
                    && match s.transport.as_str() {
                        "stdio" => !s.command.as_deref().map(str::trim).unwrap_or("").is_empty(),
                        _ => !s.url.as_deref().map(str::trim).unwrap_or("").is_empty(),
                    };
                if !convertible {
                    out.push(McpBootHealth {
                        name: s.name.clone(),
                        transport: match s.transport.as_str() {
                            "stdio" => "stdio",
                            "sse" => "sse",
                            "http" => "http",
                            _ => "unknown",
                        },
                        target: s
                            .command
                            .clone()
                            .or_else(|| s.url.clone())
                            .unwrap_or_else(|| "-".to_string()),
                        ok: false,
                        detail: format!(
                            "dropped at boot: {}",
                            pantheon_migration::server_readiness(s)
                                .unwrap_or_else(|| "unconvertible declaration".to_string())
                        ),
                    });
                }
            }
        }
        out
    }

    /// Resolve `env:NAME` secret refs against the process environment (the
    /// manager's resolver falls back to the same). Returns the resolved env
    /// or the first missing variable name.
    fn resolve_env(env: &HashMap<String, String>) -> Result<HashMap<String, String>, String> {
        let mut out = HashMap::new();
        for (k, v) in env {
            match v.strip_prefix("env:") {
                Some(name) => match std::env::var(name) {
                    Ok(val) => {
                        out.insert(k.clone(), val);
                    }
                    Err(_) => return Err(name.to_string()),
                },
                None => {
                    out.insert(k.clone(), v.clone());
                }
            }
        }
        Ok(out)
    }

    /// Split an http(s) URL into (host, port) for the TCP reachability probe.
    fn url_host_port(url: &str) -> Option<(String, u16)> {
        let after_scheme = url.split("://").nth(1)?;
        let host_port = after_scheme.split('/').next()?;
        let (host, port) = match host_port.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().ok()?),
            None => {
                let default = if url.starts_with("https") { 443 } else { 80 };
                (host_port.to_string(), default)
            }
        };
        if host.is_empty() {
            return None;
        }
        Some((host, port))
    }

    fn probe_stdio(spec: &McpServerSpec, timeout: Duration) -> McpBootHealth {
        let base = McpBootHealth {
            name: spec.name.clone(),
            transport: spec.transport.as_str(),
            target: spec.target(),
            ok: false,
            detail: String::new(),
        };
        let command = match &spec.command {
            Some(c) => c.clone(),
            None => {
                return McpBootHealth {
                    detail: "stdio server has no command".to_string(),
                    ..base
                }
            }
        };
        let env = match resolve_env(&spec.env) {
            Ok(e) => e,
            Err(missing) => {
                return McpBootHealth {
                    detail: format!("secret env {missing} is not set"),
                    ..base
                }
            }
        };
        let cfg = McpServerConfig::new(&spec.name, command)
            .with_args(spec.args.clone())
            .with_env(env)
            .with_timeout(timeout);
        // `None` policy: the manager gates spawns through the capability
        // policy at session time; the boot probe mirrors the pre-policy
        // ungated spawn for servers the operator already approved.
        match McpClient::connect(&cfg, None) {
            Ok(client) => {
                let version = if client.negotiated_version.is_empty() {
                    "handshake ok".to_string()
                } else {
                    format!("handshake ok (protocol {})", client.negotiated_version)
                };
                McpBootHealth {
                    ok: true,
                    detail: version,
                    ..base
                }
            }
            Err(e) => McpBootHealth {
                detail: format!("{e:?}"),
                ..base
            },
        }
    }

    fn probe_remote(spec: &McpServerSpec) -> McpBootHealth {
        let base = McpBootHealth {
            name: spec.name.clone(),
            transport: spec.transport.as_str(),
            target: spec.target(),
            ok: false,
            detail: String::new(),
        };
        let url = match &spec.url {
            Some(u) => u.clone(),
            None => {
                return McpBootHealth {
                    detail: format!("{} server has no url", spec.transport.as_str()),
                    ..base
                }
            }
        };
        // A full MCP HTTP handshake at boot is overkill; TCP reachability
        // proves the server is alive. The session-time manager does the real
        // handshake before first use.
        match url_host_port(&url).and_then(|(h, p)| {
            std::net::TcpStream::connect_timeout(
                &format!("{h}:{p}").parse().ok()?,
                Duration::from_secs(5),
            )
            .ok()
            .map(|_| (h, p))
        }) {
            Some(_) => McpBootHealth {
                ok: true,
                detail: "tcp reachable (handshake deferred to first use)".to_string(),
                ..base
            },
            None => McpBootHealth {
                detail: format!("cannot reach {url}"),
                ..base
            },
        }
    }

    /// Handshake every enabled, consented MCP server with a timeout.
    /// `per_server_timeout` caps the stdio handshake (the config's own
    /// `timeout_secs` still applies below the cap).
    pub fn check(data_dir: &Path, per_server_timeout: Duration) -> Vec<McpBootHealth> {
        let approved = pantheon_api::approval::ApprovalStore::open(&data_dir.join("mcp")).names();
        let mut out = Vec::new();
        let mut specs = config_specs(data_dir);
        for spec in declaration_specs(data_dir) {
            if !specs.iter().any(|s| s.name == spec.name) {
                specs.push(spec);
            }
        }
        for spec in specs {
            // Consent gate: the canonical bundled recipe probes freely;
            // anything else needs an approval record. A bundled NAME alone
            // is not consent: a hand-written `[mcp.servers.playwright]`
            // with an attacker command used to probe (spawn) at boot on
            // name match alone. A name-colliding spec is a custom server
            // and, without an approval record, is skipped exactly like any
            // other unapproved third-party server. (Session time still
            // enforces the hash/version gate and the capability policy
            // before any tool call; the boot probe only ever handshakes
            // already-consented servers.)
            let consented = pantheon_mcp::manager::is_canonical_bundled(&spec)
                || approved.iter().any(|a| a == &spec.name);
            if !consented {
                out.push(McpBootHealth {
                    name: spec.name.clone(),
                    transport: spec.transport.as_str(),
                    target: spec.target(),
                    ok: false,
                    detail: "not approved - skipped at boot (approve in the dashboard)".to_string(),
                });
                continue;
            }
            let timeout = spec
                .timeout
                .min(per_server_timeout)
                .max(Duration::from_secs(1));
            let health = match spec.transport {
                McpTransport::Stdio => probe_stdio(&spec, timeout),
                McpTransport::Sse | McpTransport::Http => probe_remote(&spec),
            };
            out.push(health);
        }
        for dropped in dropped_declaration_specs(data_dir) {
            if !out.iter().any(|r| r.name == dropped.name) {
                out.push(dropped);
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Run [`check`] and log one line per server, plus a summary. Returns the
    /// results for callers that want them. Never panics, never exits: a broken
    /// MCP server must not take the gateway down with it.
    pub fn check_and_log(data_dir: &Path, per_server_timeout: Duration) -> Vec<McpBootHealth> {
        let results = check(data_dir, per_server_timeout);
        if results.is_empty() {
            eprintln!("mcp boot: no enabled MCP servers configured");
            return results;
        }
        let mut bad = 0;
        for r in &results {
            if r.ok {
                eprintln!("mcp boot: {} ok ({})", r.name, r.detail);
            } else {
                bad += 1;
                eprintln!("mcp boot: {} UNHEALTHY: {}", r.name, r.detail);
            }
        }
        if bad > 0 {
            eprintln!(
            "mcp boot: {bad} of {} servers unhealthy - they will fail at tool-call time; fix or disable them in [mcp.servers]",
            results.len()
        );
        }
        results
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn url_host_port_parses() {
            assert_eq!(
                url_host_port("https://example.com/mcp"),
                Some(("example.com".to_string(), 443))
            );
            assert_eq!(
                url_host_port("http://localhost:8000/sse"),
                Some(("localhost".to_string(), 8000))
            );
            assert_eq!(url_host_port("not a url"), None);
        }

        #[test]
        fn resolve_env_missing_reports_name() {
            let mut env = HashMap::new();
            env.insert(
                "TOKEN".to_string(),
                "env:PANTHEON_TEST_DEFINITELY_UNSET".to_string(),
            );
            let err = resolve_env(&env).unwrap_err();
            assert_eq!(err, "PANTHEON_TEST_DEFINITELY_UNSET");
        }

        #[test]
        fn check_empty_config_is_empty() {
            let d = std::env::temp_dir().join("pantheon-mcp-boot-test-empty");
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).expect("create test dir");
            std::fs::write(d.join("config.toml"), "[mcp]\n").expect("write config");
            let out = check(&d, Duration::from_secs(2));
            assert!(out.is_empty());
        }

        #[test]
        fn check_reports_broken_and_unapproved() {
            let d = std::env::temp_dir().join("pantheon-mcp-boot-test-probe");
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).expect("create test dir");
            // "broken" is bundled-approved? No - use a name the bundled check
            // will not know, but approve it via the approval store dir so the
            // probe actually runs and fails on the bogus command.
            std::fs::write(
            d.join("config.toml"),
            "[mcp.servers.broken]\nenabled = true\ncommand = \"pantheon-definitely-not-a-real-binary-xyz\"\n",
        )
        .expect("write config");
            let mcp_dir = d.join("mcp");
            std::fs::create_dir_all(&mcp_dir).expect("create mcp dir");
            std::fs::write(
                mcp_dir.join(".approvals.json"),
                r#"{"broken":{"plugin":"broken","version":"1","dir_hash":"x","approved_at_ms":1}}"#,
            )
            .unwrap();
            let out = check(&d, Duration::from_secs(5));
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].name, "broken");
            assert_eq!(out[0].transport, "stdio");
            assert!(!out[0].ok, "bogus command should not handshake");
            assert!(!out[0].detail.is_empty());
        }

        #[test]
        fn check_skips_unapproved_third_party() {
            let d = std::env::temp_dir().join("pantheon-mcp-boot-test-unapproved");
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).expect("create test dir");
            std::fs::write(
                d.join("config.toml"),
                "[mcp.servers.sneaky]\nenabled = true\ncommand = \"echo\"\n",
            )
            .unwrap();
            // No approval record: the server must never be spawned.
            let out = check(&d, Duration::from_secs(5));
            assert_eq!(out.len(), 1);
            assert!(!out[0].ok);
            assert!(out[0].detail.contains("not approved"));
        }

        #[test]
        fn check_skips_bundled_name_collision() {
            // Cluster 1 (c): a hand-written [mcp.servers.playwright] with
            // an attacker command and NO approval record must never be
            // spawned at boot. Before the fix the gate consented on the
            // bundled name alone, so the attacker command was probed.
            let d = std::env::temp_dir().join("pantheon-mcp-boot-test-collision");
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).expect("create test dir");
            std::fs::write(
            d.join("config.toml"),
            "[mcp.servers.playwright]\nenabled = true\ncommand = \"pantheon-definitely-not-a-real-binary-xyz\"\nargs = [\"--evil\"]\n",
        )
        .unwrap();
            // No approval record, and the spec is not the canonical
            // playwright recipe: skipped, never spawned.
            let out = check(&d, Duration::from_secs(5));
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].name, "playwright");
            assert!(!out[0].ok);
            assert!(
                out[0].detail.contains("not approved"),
                "collision must be treated as unapproved, got: {}",
                out[0].detail
            );
        }
    }
}
pub mod rendezvous {
    //! Single-flight rendezvous for `gateway run`: one gateway per data dir.
    //!
    //! Two gateway processes would double-fire the scheduler and double-deliver
    //! webhook-driven channels (Telegram's persisted offset cursor survives a
    //! replay, but the scheduler and Discord webhooks do not). The rendezvous is
    //! an exclusive file lock on `<data_dir>/gateway.lock`, held for the whole
    //! process lifetime. A second `gateway run` sees the contention and exits
    //! instead of starting.
    //!
    //! The lock is OS-held: if the holder dies, the kernel releases it, so
    //! there is no stale state to clean up. The holder's pid is written into
    //! the file after locking (readable without the lock) purely so the
    //! "already running" message can name it.

    use fs2::FileExt;
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};

    /// The rendezvous file inside a data dir.
    pub fn lock_path(data_dir: &Path) -> PathBuf {
        data_dir.join("gateway.lock")
    }

    /// A held gateway singleton lock. Keep it alive for the process lifetime;
    /// dropping it releases the lock.
    pub struct GatewayLock {
        _file: File,
    }

    /// Outcome of a non-blocking acquire attempt.
    pub enum AcquireOutcome {
        /// This process now holds the lock.
        Acquired(GatewayLock),
        /// Another process holds the lock. `pid` is the holder's recorded pid,
        /// when it could be read and parsed.
        HeldBy { pid: Option<u32> },
    }

    /// Try to take the singleton lock without blocking.
    pub fn try_acquire(data_dir: &Path) -> std::io::Result<AcquireOutcome> {
        let path = lock_path(data_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        match file.try_lock_exclusive() {
            Ok(()) => {
                // Record our pid for the contention message. A contender reads
                // this without needing the lock.
                file.set_len(0)?;
                write!(file, "{}", std::process::id())?;
                file.sync_all().ok();
                Ok(AcquireOutcome::Acquired(GatewayLock { _file: file }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let pid = File::open(&path)
                    .and_then(|mut f| {
                        let mut text = String::new();
                        f.read_to_string(&mut text)?;
                        Ok::<_, std::io::Error>(text)
                    })
                    .ok()
                    .and_then(|text| text.trim().parse::<u32>().ok());
                Ok(AcquireOutcome::HeldBy { pid })
            }
            Err(e) => Err(e),
        }
    }

    /// True when another process currently holds the singleton lock.
    pub fn is_running(data_dir: &Path) -> bool {
        matches!(try_acquire(data_dir), Ok(AcquireOutcome::HeldBy { .. }))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::atomic::{AtomicU64, Ordering};

        static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

        fn test_dir() -> PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "pantheon-rendezvous-test-{}-{}",
                std::process::id(),
                TEST_COUNTER.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        #[test]
        fn second_acquire_reports_holder_pid() {
            let dir = test_dir();
            let _first = try_acquire(&dir).expect("first acquire succeeds");
            match try_acquire(&dir).expect("contention probe succeeds") {
                AcquireOutcome::HeldBy { pid } => {
                    assert_eq!(pid, Some(std::process::id()), "holder pid is reported")
                }
                AcquireOutcome::Acquired(_) => panic!("second acquire must not succeed"),
            }
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn lock_releases_on_drop() {
            let dir = test_dir();
            {
                let _held = try_acquire(&dir).expect("acquire succeeds");
                assert!(is_running(&dir), "held lock reports running");
            }
            assert!(!is_running(&dir), "dropped lock reports not running");
            // Re-acquire after release works: no stale state.
            let _again = try_acquire(&dir).expect("re-acquire succeeds");
            std::fs::remove_dir_all(&dir).ok();
        }

        #[test]
        fn lock_path_is_inside_data_dir() {
            let dir = test_dir();
            assert_eq!(lock_path(&dir), dir.join("gateway.lock"));
            std::fs::remove_dir_all(&dir).ok();
        }
    }
}
pub mod genui {
    //! Generative-UI references: task id + signed URL, never embedded payloads.
    //! A frame carries `GenUiRef`; the client fetches bytes from the URL.
    //! Signing is std-only HMAC-SHA256 (compact local implementation) so no
    //! new crates are needed. Secrets come from env at serve time.
    use pantheon_api::logging::now_ms;
    use serde::{Deserialize, Serialize};
    /// Reference to a generated artifact (image, file, card bundle).
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct GenUiRef {
        pub task_id: String,
        pub url: String,
        pub expires_ms: i64,
        pub mime: String,
    }
    /// Signed URL handed to the client.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct SignedUrl {
        pub url: String,
        pub expires_ms: i64,
    }
    /// Task ids are single URL path components. Keeping the alphabet explicit
    /// prevents query/header injection and makes signed URLs portable across the
    /// stdlib HTTP shim and external clients.
    pub fn valid_task_id(task_id: &str) -> bool {
        !task_id.is_empty()
            && task_id.len() <= 128
            && task_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    }

    // --- compact SHA-256 (FIPS 180-4), then HMAC over it. ~70 lines, std-only.
    fn sha256(msg: &[u8]) -> [u8; 32] {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
            0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
            0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
            0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
            0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
            0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
            0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
            0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
            0xc67178f2,
        ];
        let mut h: [u32; 8] = [
            0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
            0x5be0cd19,
        ];
        let mut buf = msg.to_vec();
        let bitlen = (msg.len() as u64).wrapping_mul(8);
        buf.push(0x80);
        while buf.len() % 64 != 56 {
            buf.push(0);
        }
        buf.extend_from_slice(&bitlen.to_be_bytes());
        for chunk in buf.as_chunks::<64>().0 {
            let mut w = [0u32; 64];
            for i in 0..16 {
                w[i] = u32::from_be_bytes([
                    chunk[4 * i],
                    chunk[4 * i + 1],
                    chunk[4 * i + 2],
                    chunk[4 * i + 3],
                ]);
            }
            for i in 16..64 {
                let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
                let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
                w[i] = w[i - 16]
                    .wrapping_add(s0)
                    .wrapping_add(w[i - 7])
                    .wrapping_add(s1);
            }
            let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
                (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
            for i in 0..64 {
                let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
                let ch = (e & f) ^ ((!e) & g);
                let t1 = hh
                    .wrapping_add(s1)
                    .wrapping_add(ch)
                    .wrapping_add(K[i])
                    .wrapping_add(w[i]);
                let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
                let maj = (a & b) ^ (a & c) ^ (b & c);
                let t2 = s0.wrapping_add(maj);
                hh = g;
                g = f;
                f = e;
                e = d.wrapping_add(t1);
                d = c;
                c = b;
                b = a;
                a = t1.wrapping_add(t2);
            }
            h[0] = h[0].wrapping_add(a);
            h[1] = h[1].wrapping_add(b);
            h[2] = h[2].wrapping_add(c);
            h[3] = h[3].wrapping_add(d);
            h[4] = h[4].wrapping_add(e);
            h[5] = h[5].wrapping_add(f);
            h[6] = h[6].wrapping_add(g);
            h[7] = h[7].wrapping_add(hh);
        }
        let mut out = [0u8; 32];
        for i in 0..8 {
            out[4 * i..4 * i + 4].copy_from_slice(&h[i].to_be_bytes());
        }
        out
    }
    fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
        let mut k = [0u8; 64];
        if key.len() > 64 {
            let h = sha256(key);
            k[..32].copy_from_slice(&h);
        } else {
            k[..key.len()].copy_from_slice(key);
        }
        let mut ipad = [0x36u8; 64];
        let mut opad = [0x5cu8; 64];
        for i in 0..64 {
            ipad[i] ^= k[i];
            opad[i] ^= k[i];
        }
        let mut inner = ipad.to_vec();
        inner.extend_from_slice(msg);
        let ih = sha256(&inner);
        let mut outer = opad.to_vec();
        outer.extend_from_slice(&ih);
        sha256(&outer)
    }
    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }
    /// Signer: mints and verifies task-scoped URLs.
    #[derive(Debug, Clone)]
    pub struct GenUiSigner {
        pub base_url: String,
        pub secret: Vec<u8>,
    }
    impl GenUiSigner {
        pub fn new(base_url: impl Into<String>, secret: Vec<u8>) -> Self {
            Self {
                base_url: base_url.into(),
                secret,
            }
        }
        /// Mint `base_url/<task_id>?exp=<ms>&sig=<hex>`. Payload stays server-side.
        pub fn sign(&self, task_id: &str, mime: &str, ttl_ms: i64) -> GenUiRef {
            let exp = now_ms() + ttl_ms;
            let msg = format!("{task_id}.{exp}");
            let sig = hex(&hmac_sha256(&self.secret, msg.as_bytes()));
            let base = self.base_url.trim_end_matches('/');
            GenUiRef {
                task_id: task_id.into(),
                mime: mime.into(),
                expires_ms: exp,
                url: format!("{base}/{task_id}?exp={exp}&sig={sig}"),
            }
        }
        /// Verify a minted URL. Checks task id, expiry, and signature (constant-time-ish).
        pub fn verify(&self, task_id: &str, expires_ms: i64, sig_hex: &str) -> bool {
            if !valid_task_id(task_id) || expires_ms < now_ms() {
                return false;
            }
            let msg = format!("{task_id}.{expires_ms}");
            let want = hex(&hmac_sha256(&self.secret, msg.as_bytes()));
            if want.len() != sig_hex.len() {
                return false;
            }
            let mut diff = 0u8;
            for (a, b) in want.bytes().zip(sig_hex.bytes()) {
                diff |= a ^ b;
            }
            diff == 0
        }
    }
}
pub mod sse {
    //! SSE encoding for UI frames (std-only, no HTTP crate).
    //! `SseEncoder` turns frames into `text/event-stream` bytes; any TCP/HTTP
    //! shim writes them. Resume via `Last-Event-ID` (ledger row id).
    use crate::server::stream::UiFrame;
    /// Encode one frame as a single SSE `data:` event with `id:` = ledger row.
    /// `event:` carries the kind so clients switch without parsing JSON first.
    pub fn encode_frame(frame: &UiFrame) -> String {
        let kind = match frame.kind {
            crate::server::stream::UiFrameKind::Run => "run",
            crate::server::stream::UiFrameKind::Text => "text",
            crate::server::stream::UiFrameKind::Tool => "tool",
            crate::server::stream::UiFrameKind::State => "state",
            crate::server::stream::UiFrameKind::Approval => "approval",
            crate::server::stream::UiFrameKind::GenUi => "genui",
            crate::server::stream::UiFrameKind::Component => "component",
        };
        let data = serde_json::to_string(frame).unwrap_or_else(|_| "{}".into());
        format!("id: {}\nevent: {kind}\ndata: {data}\n\n", frame.id)
    }
    /// Encode many frames in order.
    pub fn encode_frames(frames: &[UiFrame]) -> String {
        frames.iter().map(encode_frame).collect()
    }
    /// Parse a `Last-Event-ID` header value into a ledger row id.
    pub fn parse_last_event_id(value: &str) -> Option<i64> {
        value.trim().parse().ok()
    }
    /// Minimal SSE response head for a hand-rolled HTTP shim.
    pub fn response_head() -> String {
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nX-Accel-Buffering: no\r\n\r\n".to_string()
    }
    /// Stateless encoder handle (keeps the call sites readable).
    #[derive(Debug, Default, Clone, Copy)]
    pub struct SseEncoder;
    impl SseEncoder {
        pub fn frame(&self, frame: &UiFrame) -> String {
            encode_frame(frame)
        }
        pub fn frames(&self, frames: &[UiFrame]) -> String {
            encode_frames(frames)
        }
        pub fn head(&self) -> String {
            response_head()
        }
    }
}
pub mod stream {
    //! Ledger event -> AG-UI frame mapping (interactive path).
    //! One frame per UI-meaningful transition, keyed by run_id + thread_id.
    //! Approval frames park the run: client answers grant/deny.
    use pantheon_api::events::Event;
    use serde::{Deserialize, Serialize};
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum UiFrameKind {
        Run,
        Text,
        Tool,
        State,
        Approval,
        GenUi,
        /// Structured UI component (`Event::UiContent`/`UiUpdated`). The
        /// document rides `component` as JSON; `text` carries a one-line
        /// summary for clients that only read text.
        Component,
    }
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct UiFrame {
        pub id: i64,
        pub kind: UiFrameKind,
        pub run_id: String,
        pub thread_id: String,
        pub name: String,
        pub text: String,
        #[serde(default)]
        pub interrupt: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub genui: Option<serde_json::Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub component: Option<serde_json::Value>,
    }
    pub fn frame_for_event(
        entry_id: i64,
        thread_id: &str,
        event: &Event,
        genui: Option<serde_json::Value>,
    ) -> Vec<UiFrame> {
        let mk = |kind: UiFrameKind, name: &str, text: String, interrupt: bool| UiFrame {
            id: entry_id,
            kind,
            run_id: pantheon_storage::ledger::run_id_of(event).to_string(),
            thread_id: thread_id.to_string(),
            name: name.to_string(),
            text,
            interrupt,
            genui: genui.clone(),
            component: None,
        };
        let mk_component = |name: &str,
                            ui_id: &str,
                            revision: u64,
                            document: &pantheon_api::ui::UiDocument,
                            interrupt: bool| UiFrame {
            id: entry_id,
            kind: UiFrameKind::Component,
            run_id: pantheon_storage::ledger::run_id_of(event).to_string(),
            thread_id: thread_id.to_string(),
            name: name.to_string(),
            text: format!(
                "ui:{ui_id} rev {revision} ({})",
                document.root.component.tag()
            ),
            interrupt,
            genui: None,
            component: serde_json::to_value(document).ok(),
        };
        match event {
            Event::RunStarted { .. } => vec![mk(UiFrameKind::Run, "started", String::new(), false)],
            Event::RunProgress { detail, .. } => {
                vec![mk(UiFrameKind::Run, "progress", detail.clone(), false)]
            }
            Event::RunCompleted { .. } => {
                vec![mk(UiFrameKind::Run, "completed", String::new(), false)]
            }
            Event::RunFailed { code, .. } => {
                vec![mk(UiFrameKind::Run, "failed", code.clone(), false)]
            }
            Event::RunCanceled { reason, .. } => {
                vec![mk(UiFrameKind::Run, "canceled", reason.clone(), false)]
            }
            Event::RunRecovered { .. } => {
                vec![mk(UiFrameKind::Run, "recovered", String::new(), false)]
            }
            Event::TurnStarted { turn_id, .. } => vec![mk(
                UiFrameKind::State,
                "turn_started",
                turn_id.clone(),
                false,
            )],
            Event::TurnParked {
                turn_id, reason, ..
            } => vec![mk(
                UiFrameKind::State,
                "turn_parked",
                format!("{turn_id}: {reason}"),
                true,
            )],
            Event::TurnCompleted {
                turn_id, outcome, ..
            } => vec![mk(
                UiFrameKind::State,
                "turn_completed",
                format!("{turn_id}: {outcome}"),
                false,
            )],
            Event::TurnFailed { turn_id, code, .. } => vec![mk(
                UiFrameKind::State,
                "turn_failed",
                format!("{turn_id}: {code}"),
                false,
            )],
            Event::ModelDelta { delta, .. } => {
                vec![mk(UiFrameKind::Text, "delta", delta.clone(), false)]
            }
            Event::AssistantMessage { message, .. } => vec![mk(
                UiFrameKind::Text,
                "message",
                message.content.clone(),
                false,
            )],
            Event::ToolMessage { message, .. } => vec![mk(
                UiFrameKind::Text,
                "result",
                message.content.clone(),
                false,
            )],
            Event::ToolRequested { tool, .. } => {
                vec![mk(UiFrameKind::Tool, "requested", tool.clone(), false)]
            }
            Event::ToolStarted { tool, call_id, .. } => vec![mk(
                UiFrameKind::Tool,
                "started",
                format!("{tool} {call_id}"),
                false,
            )],
            Event::ToolOutput {
                tool, truncated, ..
            } => vec![mk(
                UiFrameKind::Tool,
                "output",
                if *truncated {
                    format!("{tool} (compacted)")
                } else {
                    tool.clone()
                },
                false,
            )],
            Event::ToolCompleted { tool, call_id, .. } => vec![mk(
                UiFrameKind::Tool,
                "completed",
                format!("{tool} {call_id}"),
                false,
            )],
            // Approval scopes are `call_id:tool:args` and ride out to
            // gateway clients verbatim here; the dashboard redacts them
            // (runs.rs timeline/detail), so do the same on this surface.
            Event::ApprovalRequested { scope, .. } => vec![mk(
                UiFrameKind::Approval,
                "requested",
                pantheon_api::logging::redact(scope),
                true,
            )],
            Event::ApprovalGranted { scope, .. } => vec![mk(
                UiFrameKind::Approval,
                "granted",
                pantheon_api::logging::redact(scope),
                false,
            )],
            Event::ApprovalDenied { scope, .. } => vec![mk(
                UiFrameKind::Approval,
                "denied",
                pantheon_api::logging::redact(scope),
                false,
            )],
            Event::UiContent {
                ui_id, document, ..
            } => vec![mk_component("ui_content", ui_id, 0, document, false)],
            Event::UiUpdated {
                ui_id,
                revision,
                document,
                ..
            } => vec![mk_component(
                "ui_updated",
                ui_id,
                *revision,
                document,
                false,
            )],
            Event::UiActionRequested { ui_id, .. } => vec![mk(
                UiFrameKind::State,
                "ui_action_requested",
                ui_id.clone(),
                true,
            )],
            Event::UiActionSubmitted {
                ui_id, action_id, ..
            } => vec![mk(
                UiFrameKind::State,
                "ui_action_submitted",
                format!("{ui_id}#{action_id}"),
                false,
            )],
            _ => vec![],
        }
    }
    pub fn frames_for_entries(
        entries: &[pantheon_storage::LedgerEntry],
        thread_id: &str,
    ) -> Vec<UiFrame> {
        let mut out = Vec::new();
        if let Some(first) = entries.first() {
            out.push(UiFrame {
                id: 0,
                kind: UiFrameKind::State,
                run_id: first.run_id.clone(),
                thread_id: thread_id.to_string(),
                name: "snapshot".into(),
                text: format!("{} events", entries.len()),
                interrupt: false,
                genui: None,
                component: None,
            });
        }
        for e in entries {
            out.extend(frame_for_event(e.id, thread_id, &e.event, None));
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use pantheon_api::ui::{Markdown, UiDocument, UiNode};

        fn doc() -> UiDocument {
            UiDocument {
                version: pantheon_api::ui::UI_PROTOCOL_VERSION,
                id: "t-ui_0".to_string(),
                root: UiNode {
                    id: "root".to_string(),
                    component: pantheon_api::ui::Component::Markdown(Markdown {
                        title: None,
                        text: "hi".to_string(),
                    }),
                },
            }
        }

        #[test]
        fn ui_content_produces_component_frame() {
            let frames = frame_for_event(
                7,
                "thread1",
                &Event::UiContent {
                    run_id: "r1".to_string(),
                    turn_id: "t".to_string(),
                    ui_id: "t-ui_0".to_string(),
                    document: doc(),
                },
                None,
            );
            assert_eq!(frames.len(), 1);
            let f = &frames[0];
            assert_eq!(f.kind, UiFrameKind::Component);
            assert_eq!(f.name, "ui_content");
            assert_eq!(f.id, 7);
            assert!(f.text.contains("t-ui_0"), "{}", f.text);
            let comp = f
                .component
                .as_ref()
                .expect("component frame carries component payload");
            assert_eq!(comp["id"], "t-ui_0");
            assert_eq!(comp["root"]["type"], "markdown");
        }

        #[test]
        fn ui_updated_produces_component_frame_with_revision() {
            let frames = frame_for_event(
                8,
                "thread1",
                &Event::UiUpdated {
                    run_id: "r1".to_string(),
                    ui_id: "t-ui_0".to_string(),
                    revision: 3,
                    document: doc(),
                },
                None,
            );
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0].kind, UiFrameKind::Component);
            assert_eq!(frames[0].name, "ui_updated");
            assert!(frames[0].text.contains("rev 3"), "{}", frames[0].text);
        }

        #[test]
        fn ui_action_events_produce_interrupt_state_frames() {
            let req = frame_for_event(
                9,
                "thread1",
                &Event::UiActionRequested {
                    run_id: "r1".to_string(),
                    ui_id: "t-ui_0".to_string(),
                    call_id: "c1".to_string(),
                },
                None,
            );
            assert_eq!(req.len(), 1);
            assert_eq!(req[0].kind, UiFrameKind::State);
            assert!(req[0].interrupt, "action requests interrupt the stream");
            assert!(req[0].component.is_none());

            let sub = frame_for_event(
                10,
                "thread1",
                &Event::UiActionSubmitted {
                    run_id: "r1".to_string(),
                    ui_id: "t-ui_0".to_string(),
                    revision: 0,
                    node_id: "root".to_string(),
                    action_id: "go".to_string(),
                    nonce: "n1".to_string(),
                },
                None,
            );
            assert_eq!(sub.len(), 1);
            assert!(!sub[0].interrupt);
            // Params are never on the wire frame (they were never in the event).
            assert!(!sub[0].text.contains("token"));
        }

        #[test]
        fn component_frame_serializes_for_sse() {
            let frames = frame_for_event(
                11,
                "thread1",
                &Event::UiContent {
                    run_id: "r1".to_string(),
                    turn_id: "t".to_string(),
                    ui_id: "t-ui_0".to_string(),
                    document: doc(),
                },
                None,
            );
            let encoded = crate::server::sse::encode_frame(&frames[0]);
            assert!(encoded.contains("event: component"), "{encoded}");
            assert!(encoded.contains("t-ui_0"), "{encoded}");
        }
    }
}
pub mod notify {
    //! Cross-platform desktop notifications.
    //!
    //! One best-effort notifier shared by the TUI turn-complete bell path and
    //! the scheduler's `--deliver notify` mode:
    //!
    //! - Linux: `notify-send` (libnotify)
    //! - macOS: `osascript` display notification
    //! - Windows: PowerShell toast via WinRT `Windows.UI.Notifications`
    //!
    //! Everything is fire-and-forget by contract: a missing helper or a failed
    //! spawn is an `Err`, never a panic, and callers are expected to ignore it.

    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    /// Which desktop notifier this host can use.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Notifier {
        /// `notify-send` at the given path (Linux/libnotify).
        NotifySend(PathBuf),
        /// `osascript` display notification (macOS).
        Osascript,
        /// PowerShell WinRT toast (Windows).
        PowerShell,
        /// Nothing usable found.
        None,
    }

    impl Notifier {
        pub fn name(&self) -> &'static str {
            match self {
                Notifier::NotifySend(_) => "notify-send",
                Notifier::Osascript => "osascript",
                Notifier::PowerShell => "powershell-toast",
                Notifier::None => "none",
            }
        }
    }

    #[cfg(unix)]
    fn is_runnable(p: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt as _;
        p.is_file()
            && p.metadata()
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
    }

    #[cfg(not(unix))]
    fn is_runnable(p: &Path) -> bool {
        p.is_file()
    }

    /// Find a helper binary on an explicit search path. Split out from PATH
    /// lookup so tests can pass a temp dir without touching the environment.
    fn find_on_path(dirs: &[PathBuf], name: &str) -> Option<PathBuf> {
        let file = if cfg!(target_os = "windows") && !name.contains('.') {
            format!("{name}.exe")
        } else {
            name.to_string()
        };
        dirs.iter().map(|d| d.join(&file)).find(|p| is_runnable(p))
    }

    /// Detect the notifier for this platform from an explicit search path
    /// (real callers pass PATH). Pure apart from the filesystem checks, so
    /// tests inject a temp dir.
    pub fn detect_notifier(path_dirs: &[PathBuf]) -> Notifier {
        if cfg!(target_os = "macos") {
            return if find_on_path(path_dirs, "osascript").is_some() {
                Notifier::Osascript
            } else {
                Notifier::None
            };
        }
        if cfg!(target_os = "windows") {
            return if find_on_path(path_dirs, "powershell").is_some() {
                Notifier::PowerShell
            } else {
                Notifier::None
            };
        }
        // Linux and other unixes: notify-send.
        match find_on_path(path_dirs, "notify-send") {
            Some(bin) => Notifier::NotifySend(bin),
            None => Notifier::None,
        }
    }

    /// AppleScript string-literal escaping for `display notification`.
    pub fn escape_applescript(s: &str) -> String {
        s.replace('\\', "\\\\").replace('"', "\\\"")
    }

    /// PowerShell single-quoted string escaping (interior quotes doubled).
    pub fn escape_powershell(s: &str) -> String {
        s.replace('\'', "''")
    }

    /// The WinRT toast script. Single-quoted PowerShell strings throughout;
    /// title/body are pre-escaped by [`escape_powershell`].
    fn powershell_toast_script(title: &str, body: &str) -> String {
        format!(
        "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null; \
         $t = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02); \
         $x = $t.GetElementsByTagName('text'); \
         $x.Item(0).AppendChild($t.CreateTextNode('{title}')) | Out-Null; \
         $x.Item(1).AppendChild($t.CreateTextNode('{body}')) | Out-Null; \
         [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('Pantheon').Show($t)",
        title = escape_powershell(title),
        body = escape_powershell(body),
    )
    }

    /// Build the notifier command without running it, so tests can assert the
    /// exact argv per platform without spawning anything.
    pub fn command_for(notifier: &Notifier, title: &str, body: &str) -> Option<Command> {
        match notifier {
            Notifier::NotifySend(bin) => {
                let mut cmd = Command::new(bin);
                cmd.arg("--app-name")
                    .arg("Pantheon")
                    .arg("--expire-time")
                    .arg("8000")
                    .arg(title)
                    .arg(body);
                Some(cmd)
            }
            Notifier::Osascript => {
                let script = format!(
                    "display notification \"{}\" with title \"{}\"",
                    escape_applescript(body),
                    escape_applescript(title)
                );
                let mut cmd = Command::new("osascript");
                cmd.arg("-e").arg(script);
                Some(cmd)
            }
            Notifier::PowerShell => {
                let mut cmd = Command::new("powershell");
                cmd.arg("-NoProfile")
                    .arg("-NonInteractive")
                    .arg("-Command")
                    .arg(powershell_toast_script(title, body));
                Some(cmd)
            }
            Notifier::None => None,
        }
    }

    /// PATH directories of this process.
    fn path_dirs() -> Vec<PathBuf> {
        std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default()
    }

    /// Best-effort desktop notification. `Ok` means the notifier ran cleanly;
    /// `Err` carries the reason (no notifier on this host, spawn failure, or a
    /// non-zero exit). Callers treat failure as "no notification", never fatal.
    pub fn desktop_notify(title: &str, body: &str) -> Result<(), String> {
        let notifier = detect_notifier(&path_dirs());
        let mut cmd = command_for(&notifier, title, body).ok_or_else(|| {
            format!(
                "no desktop notifier on this platform ({} backend)",
                notifier.name()
            )
        })?;
        let status = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| format!("{}: {e}", notifier.name()))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("{} exited with {status}", notifier.name()))
        }
    }
}
