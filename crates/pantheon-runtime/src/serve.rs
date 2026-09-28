//! AG-UI serve: std-only HTTP shim for the interactive path.
//! GET /agui/stream (SSE replay + 25s long-poll), POST /agui/rpc (JSON-RPC),
//! GET /agui/blob/<task> (signed generative-UI bytes), GET /agui/health.
//! One TcpListener, one thread per connection, ledger polling for liveness.
//!
//! OPERATING NOTES (group-C audit):
//! - Request bodies are capped at 1 MiB: a bigger Content-Length gets 413
//!   BEFORE any allocation (see `MAX_BODY` + `handle_one`).
//! - Every RPC method opens a fresh `Supervisor` (3 SQLite connections +
//!   migrations) and drops it. Milliseconds for a local single-user
//!   server; a multi-user server needs a SupervisorPool (not built).
//! - SSE streams poll the ledger every 500 ms for 25 s max, then close.
//!   Terminal runs (completed/failed/canceled) close early; an
//!   awaiting_approval run stays open for the window and the WEB CLIENT
//!   is expected to reconnect (Last-Event-ID / ?after= supported).
use crate::rpc::Dispatcher;
use pantheon_gateway::{
    frames_for_entries, parse_last_event_id, valid_task_id, GenUiSigner, SseEncoder, UiFrame,
};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[derive(Debug, Clone)]
pub struct ServeConfig {
    pub data_dir: PathBuf,
    pub host: String,
    pub port: u16,
    pub genui_base: String,
    /// Bearer token for every /agui route except /agui/health. The CLI reads
    /// PANTHEON_SERVE_TOKEN; when it is unset, `serve()` generates a random
    /// one-time token and prints it once at startup, so the server never
    /// runs with no auth. `pantheon serve` prints the bound URL for the web UI.
    pub auth_token: Option<String>,
}

/// Opens a Session for one turn. Installed by `pantheon serve` through
/// `agui::set_session_factory` rather than threaded through every
/// constructor, so this crate does not depend on pantheon-tui, which owns
/// config.toml and the key-name conventions.
pub type SessionFactory = std::sync::Arc<
    dyn Fn(&PathBuf) -> Result<crate::session::Session, pantheon_api::error::PantheonError>
        + Send
        + Sync,
>;
impl ServeConfig {
    fn effective_genui_base(&self) -> String {
        self.genui_base
            .replace("http://0.0.0.0", "http://127.0.0.1")
            .replace("http://[::]", "http://[::1]")
    }
    pub fn signer(&self) -> GenUiSigner {
        let secret = std::env::var("PANTHEON_GENUI_SECRET")
            .map(|s| s.into_bytes())
            .unwrap_or_else(|_| b"pantheon-dev-genui-secret".to_vec());
        GenUiSigner::new(self.effective_genui_base(), secret)
    }
    pub fn dispatcher(&self) -> Dispatcher {
        crate::agui::dispatcher_for_with_hint_and_host(
            self.data_dir.clone(),
            self.port,
            self.effective_genui_base(),
            &self.host,
        )
    }
}
/// Minimal, dependency-free AG-UI client. It is intentionally a smoke-test
/// surface rather than a framework: create a run, follow its SSE stream, and
/// answer approval frames through the same RPC endpoint.
pub const WEB_UI: &str = r##"<!doctype html>
<meta charset="utf-8">
<title>Pantheon AG-UI</title>
<style>
body{font:16px system-ui,sans-serif;max-width:900px;margin:2rem auto;padding:0 1rem}#log{white-space:pre-wrap;border:1px solid #ddd;padding:1rem;min-height:18rem}button{padding:.45rem .8rem;margin:.2rem}
</style>
<h1>Pantheon</h1>
<form id="send"><input id="text" required placeholder="Ask something" style="width:70%"><button>Send</button></form>
<button id="cancel" disabled>Cancel run</button><div id="actions"></div><pre id="log"></pre>
<script>
const $=s=>document.querySelector(s), log=s=>{$('#log').textContent+=s+'\n'};
let rpcId=1, run='', thread='', cursor=0, es;
const TOKEN=__PANTHEON_TOKEN__;
async function rpc(method,params={}){let r=await fetch('/agui/rpc',{method:'POST',headers:{'content-type':'application/json','x-pantheon-token':TOKEN},body:JSON.stringify({jsonrpc:'2.0',id:rpcId++,method,params})});let j=await r.json();if(j.error)throw Error(j.error.message);return j.result}
function showActions(f){if(f.name!=='requested')return;const actions=$('#actions');actions.innerHTML='';for(const [label,answer] of [['Grant','grant'],['Deny','deny']]){const b=document.createElement('button');b.textContent=label;b.onclick=async()=>{try{await rpc('agui.'+answer,{run_id:run,scope:f.text});actions.innerHTML=''}catch(e){log(e.message)}};actions.appendChild(b)}}
function openStream(){if(es)es.close();es=new EventSource('/agui/stream?run='+encodeURIComponent(run)+'&thread='+encodeURIComponent(thread)+'&after='+cursor+(TOKEN?'&token='+encodeURIComponent(TOKEN):''));for(const kind of ['run','text','tool','state','genui'])es.addEventListener(kind,e=>{const f=JSON.parse(e.data);cursor=Math.max(cursor,f.id||0);log(kind+': '+f.text)});es.addEventListener('approval',e=>showActions(JSON.parse(e.data)))}
$('#cancel').onclick=async()=>{if(!run)return;try{await rpc('agui.cancel',{run_id:run});$('#cancel').disabled=true;log('run canceled')}catch(e){log(e.message)}};
$('#send').onsubmit=async e=>{e.preventDefault();const text=$('#text').value;$('#text').value='';try{const r=await rpc('agui.send',run?{run_id:run,text}:{text});run=r.run_id;thread=r.thread_id;$('#cancel').disabled=false;openStream()}catch(e){log(e.message)}};
</script>"##;

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Error",
        _ => "Error",
    }
}

/// Connection hardening: a flood of half-open connections must not exhaust
/// threads/fds, a header block bigger than 32 KiB is rejected before parsing,
/// and a connection that trickles bytes (slowloris) is dropped after 10 s
/// without a complete request head.
const MAX_CONNS: usize = 32;
const MAX_HEADERS: usize = 32 * 1024;
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

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

/// Generate a random bearer token, std-only: 24 bytes from the OS CSPRNG
/// (`/dev/urandom`), base64url-encoded to 32 chars. Falls back to a hashed
/// mix of pid/thread/time on platforms without it; the token is only a
/// loopback gate, and the fallback still varies per process start.
fn generate_token() -> String {
    let mut bytes = [0u8; 24];
    let from_os = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| {
            use std::io::Read;
            f.read_exact(&mut bytes).map(|_| ())
        })
        .is_ok();
    if !from_os {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        std::process::id().hash(&mut h);
        std::thread::current().id().hash(&mut h);
        std::time::SystemTime::now().hash(&mut h);
        (bytes.as_ptr() as usize).hash(&mut h);
        bytes[..8].copy_from_slice(&h.finish().to_le_bytes());
    }
    const ALPH: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(32);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for b in bytes {
        acc = (acc << 8) | b as u32;
        bits += 8;
        while bits >= 6 {
            bits -= 6;
            out.push(ALPH[((acc >> bits) & 63) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPH[((acc << (6 - bits)) & 63) as usize] as char);
    }
    out
}

/// Pure: is this Origin header value acceptable for a loopback server?
/// Missing Origin (curl, scripts, non-browser clients) is allowed. A
/// present Origin must name a loopback host; anything else is a cross-origin
/// browser request and is rejected.
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

/// Pure: inject `token` into the served page as a JS string literal.
/// serde_json produces the quoted literal (escaping quotes, backslashes,
/// control chars); `<` is additionally escaped as `\u003c` so a token can
/// never contain a literal `</script>` that would break out of the script
/// block.
fn inject_token(page: &str, token: Option<&str>) -> String {
    let lit = serde_json::to_string(token.unwrap_or("")).unwrap_or_else(|_| "\"\"".into());
    let lit = lit.replace('<', "\\u003c");
    page.replace("__PANTHEON_TOKEN__", &lit)
}
fn respond(stream: &mut TcpStream, code: u16, ctype: &str, body: &[u8]) {
    let head = format!("HTTP/1.1 {code} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", reason(code), body.len());
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
}
fn query_map(path: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(q) = path.split_once('?').map(|(_, q)| q) {
        for kv in q.split('&') {
            if let Some((k, v)) = kv.split_once('=') {
                out.insert(k.to_string(), v.to_string());
            }
        }
    }
    out
}
fn thread_for(data_dir: &Path, run_id: &str, fallback: &str) -> String {
    if !fallback.is_empty() {
        return fallback.to_string();
    }
    let p = data_dir.join("threads.json");
    if let Ok(raw) = std::fs::read_to_string(&p) {
        if let Ok(map) = serde_json::from_str::<HashMap<String, String>>(&raw) {
            if let Some(t) = map.get(run_id) {
                return t.clone();
            }
        }
    }
    format!("cli:{run_id}")
}
/// Serializes the threads.json read-modify-write: two concurrent
/// agui.send calls (different runs) must not lose each other's entries.
static THREADS_LOCK: std::sync::LazyLock<std::sync::Mutex<()>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(()));

pub fn remember_thread(data_dir: &PathBuf, run_id: &str, thread_id: &str) {
    let _guard = THREADS_LOCK.lock().unwrap();
    let p = data_dir.join("threads.json");
    let mut map: HashMap<String, String> = std::fs::read_to_string(&p)
        .ok()
        .and_then(|r| serde_json::from_str(&r).ok())
        .unwrap_or_default();
    map.insert(run_id.to_string(), thread_id.to_string());
    if let Ok(raw) = serde_json::to_string(&map) {
        let _ = std::fs::create_dir_all(data_dir);
        let _ = std::fs::write(&p, raw);
    }
}
pub fn snapshot_frames(data_dir: &Path, run_id: &str, thread: &str, after: i64) -> Vec<UiFrame> {
    let sup = match crate::Supervisor::open(data_dir.to_path_buf()) {
        Ok(s) => s,
        Err(_) => return vec![],
    };
    let entries = sup.replay(run_id).unwrap_or_default();
    let thread_id = thread_for(data_dir, run_id, thread);
    let mut frames = frames_for_entries(&entries, &thread_id);
    frames.retain(|f| f.id > after || f.id == 0);
    frames
}
fn handle_stream(
    stream: &mut TcpStream,
    cfg: &ServeConfig,
    path: &str,
    headers: &HashMap<String, String>,
) {
    let q = query_map(path);
    let run_id = q.get("run").cloned().unwrap_or_default();
    let thread = q.get("thread").cloned().unwrap_or_default();
    if run_id.is_empty() {
        respond(
            stream,
            400,
            "application/json",
            br#"{"error":"missing ?run="}"#,
        );
        return;
    }
    let mut after: i64 = q.get("after").and_then(|v| v.parse().ok()).unwrap_or(0);
    if after == 0 {
        if let Some(h) = headers.get("last-event-id") {
            if let Some(n) = parse_last_event_id(h) {
                after = n;
            }
        }
    }
    let enc = SseEncoder;
    let _ = stream.write_all(enc.head().as_bytes());
    let mut sent = after;
    for f in snapshot_frames(&cfg.data_dir, &run_id, &thread, after) {
        if f.id > sent {
            sent = f.id;
        }
        let _ = stream.write_all(enc.frame(&f).as_bytes());
    }
    let _ = stream.flush();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(25);
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let fresh = snapshot_frames(&cfg.data_dir, &run_id, &thread, sent);
        let mut progressed = false;
        for f in fresh {
            if f.id == 0 {
                continue;
            }
            if f.id > sent {
                sent = f.id;
                progressed = true;
                if stream.write_all(enc.frame(&f).as_bytes()).is_err() {
                    return;
                }
            }
        }
        if progressed && stream.flush().is_err() {
            return;
        }
        if let Ok(sup) = crate::Supervisor::open(cfg.data_dir.clone()) {
            if let Ok(Some(s)) = sup.ledger_status(&run_id) {
                if s == "completed" || s == "failed" || s == "canceled" {
                    break;
                }
            }
        }
    }
}
fn handle_rpc(stream: &mut TcpStream, cfg: &ServeConfig, body: &str) {
    let d = cfg.dispatcher();
    let mut out = Vec::new();
    for resp in d.handle_line(body.trim()) {
        out.push(serde_json::to_string(&resp).unwrap_or_else(|_| "{}".into()));
    }
    respond(stream, 200, "application/json", out.join("\n").as_bytes());
}
fn handle_blob(stream: &mut TcpStream, cfg: &ServeConfig, path: &str) {
    let without_q = path.split('?').next().unwrap_or(path);
    let task = without_q.trim_start_matches("/agui/blob/").to_string();
    let q = query_map(path);
    let exp: i64 = q.get("exp").and_then(|v| v.parse().ok()).unwrap_or(0);
    let sig = q.get("sig").cloned().unwrap_or_default();
    if !valid_task_id(&task) {
        respond(stream, 400, "application/json", br#"{"error":"bad task"}"#);
        return;
    }
    if !cfg.signer().verify(&task, exp, &sig) {
        respond(
            stream,
            403,
            "application/json",
            br#"{"error":"bad signature or expired"}"#,
        );
        return;
    }
    let sup = match crate::Supervisor::open(cfg.data_dir.clone()) {
        Ok(sup) => sup,
        Err(_) => {
            respond(
                stream,
                500,
                "application/json",
                br#"{"error":"ledger unavailable"}"#,
            );
            return;
        }
    };
    match sup.artifact(&task) {
        Ok(Some(artifact)) => respond(stream, 200, &artifact.mime, &artifact.bytes),
        Ok(None) => respond(
            stream,
            404,
            "application/json",
            br#"{"error":"no such artifact"}"#,
        ),
        Err(_) => respond(
            stream,
            500,
            "application/json",
            br#"{"error":"artifact read failed"}"#,
        ),
    }
}
fn route_is(path: &str, route: &str) -> bool {
    path == route || path.starts_with(&format!("{route}?"))
}

/// Request-body cap. JSON-RPC payloads and artifact uploads are small; a
/// bigger Content-Length is rejected before allocation (413).
const MAX_BODY: usize = 1024 * 1024;

fn handle_one(stream: TcpStream, cfg: ServeConfig) {
    let mut s = stream;
    // Slowloris: a connection that trickles bytes must not hold a thread
    // forever waiting for the request head.
    let _ = s.set_read_timeout(Some(READ_TIMEOUT));
    let (method, path, headers, body) = {
        let mut reader = match s.try_clone() {
            Ok(r) => BufReader::new(r),
            Err(_) => return, // fd exhaustion or closed socket: nothing to serve
        };
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            return;
        }
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let path = parts.next().unwrap_or("/").to_string();
        let mut headers = HashMap::new();
        let mut content_len = 0usize;
        let mut header_bytes = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                return;
            }
            header_bytes += line.len();
            if header_bytes > MAX_HEADERS {
                respond(
                    &mut s,
                    431,
                    "application/json",
                    br#"{"error":"request headers too large"}"#,
                );
                return;
            }
            let line = line.trim_end().to_string();
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                let k = k.trim().to_lowercase();
                let v = v.trim().to_string();
                if k == "content-length" {
                    content_len = v.parse().unwrap_or(0);
                }
                headers.insert(k, v);
            }
        }
        let mut body = vec![0u8; content_len.min(MAX_BODY)];
        if content_len > MAX_BODY {
            respond(
                &mut s,
                413,
                "application/json",
                br#"{"error":"request body too large"}"#,
            );
            return;
        }
        if content_len > 0 && reader.read_exact(&mut body).is_err() {
            return;
        }
        (
            method,
            path,
            headers,
            String::from_utf8_lossy(&body).to_string(),
        )
    };
    // Origin: a malicious webpage can POST to 127.0.0.1:<port> from any
    // origin, so cross-origin browser requests are rejected up front.
    // Non-browser clients (curl, scripts) omit Origin and are allowed.
    if !origin_allowed(headers.get("origin").map(String::as_str)) {
        respond(
            &mut s,
            403,
            "application/json",
            br#"{"error":"cross-origin request rejected"}"#,
        );
        return;
    }
    // Auth: everything except /agui/health requires the token when one
    // is configured. Accept Authorization: Bearer <t> or X-Pantheon-Token.
    if let Some(token) = &cfg.auth_token {
        let provided = headers
            .get("authorization")
            .and_then(|v| v.strip_prefix("Bearer ").map(|t| t.trim().to_string()))
            .or_else(|| headers.get("x-pantheon-token").cloned())
            .or_else(|| {
                // EventSource cannot set headers; allow ?token= on stream.
                path.split_once('?').and_then(|(_, q)| {
                    q.split('&')
                        .find_map(|kv| kv.strip_prefix("token=").map(|t| t.to_string()))
                })
            });
        let health = path.split('?').next() == Some("/agui/health");
        if !health && provided.as_deref() != Some(token.as_str()) {
            respond(
                &mut s,
                401,
                "application/json",
                br#"{"error":"unauthorized: set Authorization: Bearer <token>"}"#,
            );
            return;
        }
    }
    if method == "GET" && (path == "/" || path == "/agui" || path == "/agui/") {
        // Inject the token into the served UI so its fetch calls carry it.
        // The token is encoded as a JSON string literal: raw replacement
        // would let a quote or </script> in the token break out of the
        // script block.
        let page = inject_token(WEB_UI, cfg.auth_token.as_deref());
        respond(&mut s, 200, "text/html; charset=utf-8", page.as_bytes());
    } else if method == "GET" && route_is(&path, "/agui/stream") {
        handle_stream(&mut s, &cfg, &path, &headers);
    } else if method == "POST" && route_is(&path, "/agui/rpc") {
        handle_rpc(&mut s, &cfg, &body);
    } else if method == "GET" && path.starts_with("/agui/blob/") {
        handle_blob(&mut s, &cfg, &path);
    } else if method == "GET" && path == "/agui/health" {
        respond(&mut s, 200, "application/json", br#"{"ok":true}"#);
    } else {
        respond(
            &mut s,
            404,
            "application/json",
            br#"{"error":"unknown agui route"}"#,
        );
    }
}
/// Blocking serve loop: one thread per connection.
pub fn serve(mut cfg: ServeConfig) -> std::io::Result<()> {
    let loopback = matches!(
        cfg.host.as_str(),
        "127.0.0.1" | "localhost" | "::1" | "[::1]"
    );
    if !loopback && cfg.auth_token.is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "non-loopback AG-UI serving requires PANTHEON_SERVE_TOKEN",
        ));
    }
    if !loopback && std::env::var("PANTHEON_GENUI_SECRET").is_err() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "non-loopback AG-UI serving requires PANTHEON_GENUI_SECRET",
        ));
    }
    if cfg.auth_token.is_none() {
        // No token configured: generate a random one-time token rather than
        // serving the RPC unauthenticated. Printed once; set
        // PANTHEON_SERVE_TOKEN for a stable token across restarts.
        let token = generate_token();
        eprintln!("pantheon serve: no PANTHEON_SERVE_TOKEN set; generated a one-time token:");
        eprintln!("  {token}");
        cfg.auth_token = Some(token);
    }
    let addr = format!("{}:{}", cfg.host, cfg.port);
    // A bind failure is the one serve error users hit routinely, and the raw
    // io::Error says neither the port nor what to do about it. Name both.
    let listener = TcpListener::bind(&addr).map_err(|e| {
        let remedy = match e.kind() {
            std::io::ErrorKind::AddrInUse => format!(
                "port {} on {} is already in use; stop that process or pass \
                 --port <other> (or set the port in [server])",
                cfg.port, cfg.host
            ),
            std::io::ErrorKind::PermissionDenied => format!(
                "not allowed to bind {}:{}; ports below 1024 need elevated \
                 privileges, so use --port <1024 or above>",
                cfg.host, cfg.port
            ),
            std::io::ErrorKind::AddrNotAvailable => format!(
                "cannot bind {}:{}; that address does not exist on this host",
                cfg.host, cfg.port
            ),
            _ => format!("could not bind {addr}"),
        };
        std::io::Error::new(e.kind(), remedy)
    })?;
    let bound = listener.local_addr()?;
    if cfg.port == 0 {
        cfg.port = bound.port();
        cfg.genui_base = cfg
            .genui_base
            .replace(&":0/".to_string(), &format!(":{}/", cfg.port));
    }
    eprintln!(
        "pantheon agui on http://{}:{}/agui/stream",
        cfg.host, cfg.port
    );
    let cfg = Arc::new(cfg);
    let in_flight = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                if in_flight.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= MAX_CONNS {
                    in_flight.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                    eprintln!("agui: connection cap ({MAX_CONNS}) reached; dropping");
                    continue;
                }
                let flights = Arc::clone(&in_flight);
                let c = ServeConfig {
                    data_dir: cfg.data_dir.clone(),
                    host: cfg.host.clone(),
                    port: cfg.port,
                    genui_base: cfg.genui_base.clone(),
                    auth_token: cfg.auth_token.clone(),
                };
                std::thread::spawn(move || {
                    let _guard = ConnGuard { n: flights };
                    handle_one(s, c)
                });
            }
            Err(e) => eprintln!("agui accept: {e}"),
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "serve_tests.rs"]
mod tests;
