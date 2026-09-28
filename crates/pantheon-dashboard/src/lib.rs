//! Pantheon's web dashboard: control plane in the browser.
//!
//! A std-only HTTP/1.1 server ([`server`]) serving a self-contained vanilla
//! SPA (embedded via `include_str!`, no build step, no npm) over Pantheon's
//! real stores: the event ledger, the scheduler's `schedule.json`, the
//! config file, the `.env` key store, log files, skills, MCP declarations,
//! the gateway service, and the reflection/consolidation engines.
//!
//! Security model (Hermes v0.17 learned this the hard way):
//! - A random 256-bit per-instance token is generated at startup and
//!   printed as `http://<bind>:<port>/?token=...`. Every `/api/*`
//!   request must carry it as `X-Pantheon-Token` or `?token=`.
//! - `POST`/`PUT`/`DELETE` additionally require the `Host` header to
//!   match the bind address (DNS-rebinding guard) and reject a mismatched
//!   `Origin`/`Referer` (an absent one means a non-browser client like
//!   curl, which the token already authenticates).
//! - Default bind is 127.0.0.1. Binding anywhere else prints a loud
//!   warning: put a reverse proxy with auth in front.
//!
//! Design references (named, per brief):
//! - VibePrompt: metric-card row, sparklines, compact KPI tiles.
//! - Raycast via Refero Styles ("midnight command center"): `#040506`
//!   canvas, hairline borders, inset highlights, Inter + monospace
//!   micro-labels, neutral buttons, mono footer metadata. Accent is a
//!   restrained cyan (`#22d3ee`); green/amber/red are statuses only.

pub mod server;

mod approvals;
mod config;
mod env;
mod logs;
mod mcp;
mod memory;
mod runs;
mod schedule;
mod skills;
mod stats;
mod system;

use server::{Request, Response};
use std::collections::HashMap;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;

/// Dashboard startup configuration, from `pantheon dashboard` flags.
pub struct DashboardConfig {
    pub data_dir: PathBuf,
    pub bind: String,
    pub port: u16,
    pub open_browser: bool,
    pub on_approval: Option<ApprovalCallback>,
}

/// Callback fired with `(run_id, granted)` after an approval decision is
/// durably recorded. The TUI wires this to its session-resume path;
/// the dashboard itself never resumes runs.
pub type ApprovalCallback = Arc<dyn Fn(&str, bool) + Send + Sync>;

/// Shared request state.
pub struct App {
    pub data_dir: PathBuf,
    pub token: String,
    pub bind: String,
    pub bind_all: bool,
    pub on_approval: Option<ApprovalCallback>,
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

/// Spawn the pantheon binary itself as a subprocess: the dashboard calls
/// back into the real CLI code paths (`schedule run`, `gateway restart`,
/// `reflect`, `consolidate`) instead of reimplementing them. Fire and
/// forget — the caller already returned 202.
pub fn spawn_pantheon(args: &[&str]) {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("pantheon"));
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args);
    // Detach: the child must outlive this worker thread's handling, and
    // must not inherit the socket.
    #[cfg(unix)]
    {
        // Double-fork via setsid is overkill; detaching stdio and not
        // waiting is enough for a dashboard-triggered background job.
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
    }
    let _ = cmd.spawn();
}

// ---------------------------------------------------------------------------
// Auth guards
// ---------------------------------------------------------------------------

fn token_of(req: &Request) -> Option<&str> {
    if let Some(h) = req.headers.get("x-pantheon-token") {
        if !h.is_empty() {
            return Some(h.as_str());
        }
    }
    req.query.get("token").map(String::as_str)
}

fn host_authority(req: &Request) -> Option<&str> {
    req.headers.get("host").map(String::as_str)
}

/// All `/api/*` requests need the per-instance token.
fn guard_api(app: &App, req: &Request) -> Result<(), Response> {
    match token_of(req) {
        Some(t) if t == app.token => Ok(()),
        _ => Err(Response::unauthorized()),
    }
}

/// `POST`/`PUT`/`DELETE`: token (via [`guard_api`]) plus the
/// DNS-rebinding / CSRF guards.
///
/// - `Host` must match the bind address (skipped only for an explicit
///   `--bind 0.0.0.0`, where the token is the whole defense and startup
///   already warned loudly).
/// - A present `Origin`/`Referer` must be same-authority as `Host`;
///   absent means a non-browser client (curl), which the token covers.
fn guard_mutation(app: &App, req: &Request) -> Result<(), Response> {
    guard_api(app, req)?;
    let host = host_authority(req).ok_or_else(|| Response::forbidden("missing Host header"))?;
    if !app.bind_all {
        let host_part = host.split(':').next().unwrap_or("");
        let bind_ok = host_part == app.bind
            || (is_loopback_bind(&app.bind)
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

// ---------------------------------------------------------------------------
// JSON helpers
// ---------------------------------------------------------------------------

pub fn json_ok(v: serde_json::Value) -> Response {
    Response::ok_json(serde_json::to_string(&v).unwrap_or_else(|_| "{}".into()))
}

pub fn err_json(status: u16, code: &str, msg: &str) -> Response {
    let body = serde_json::json!({"ok": false, "error": {"code": code, "message": msg}});
    let text = serde_json::to_string(&body).unwrap_or_else(|_| "{}".into());
    match status {
        400 => Response::bad_request(&text),
        401 => Response::unauthorized(),
        403 => Response::forbidden(&text),
        404 => Response::not_found(),
        _ => Response::internal(text),
    }
}

pub fn bad_json(msg: &str) -> Response {
    err_json(400, "BAD_REQUEST", msg)
}

/// Parse a JSON body into `serde_json::Value`. Empty body = empty object.
pub fn body_json(req: &Request) -> Result<serde_json::Value, Response> {
    if req.body.is_empty() {
        return Ok(serde_json::Value::Object(Default::default()));
    }
    serde_json::from_slice(&req.body).map_err(|e| bad_json(&format!("invalid JSON body: {e}")))
}

fn query_usize(q: &HashMap<String, String>, key: &str, default: usize) -> usize {
    q.get(key).and_then(|v| v.parse().ok()).unwrap_or(default)
}

// ---------------------------------------------------------------------------
// Static assets
// ---------------------------------------------------------------------------

const INDEX_HTML: &str = include_str!("../assets/index.html");
const STYLE_CSS: &str = include_str!("../assets/style.css");
const APP_JS: &str = include_str!("../assets/app.js");
const MANIFEST_JSON: &str = include_str!("../assets/manifest.json");
const SW_JS: &str = include_str!("../assets/sw.js");
const ICON_192: &[u8] = include_bytes!("../assets/icon-192.png");
const ICON_512: &[u8] = include_bytes!("../assets/icon-512.png");

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

fn split_path(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(server::percent_decode)
        .collect()
}

fn dispatch(app: &App, req: &Request) -> Response {
    let segs = split_path(&req.path);
    // Static shell: no token needed (it cannot do anything without one).
    if req.method == "GET" && segs.is_empty() {
        return Response::ok_html(INDEX_HTML);
    }
    if req.method == "GET" && segs == ["style.css"] {
        return Response::ok_css(STYLE_CSS);
    }
    if req.method == "GET" && segs == ["app.js"] {
        return Response::ok_js(APP_JS);
    }
    // PWA shell assets: no token needed (they cannot do anything without one).
    if req.method == "GET" && segs == ["manifest.json"] {
        return Response::ok_bytes("application/manifest+json", MANIFEST_JSON.as_bytes());
    }
    if req.method == "GET" && segs == ["sw.js"] {
        return Response::ok_js(SW_JS);
    }
    if req.method == "GET" && segs == ["icon-192.png"] {
        return Response::ok_bytes("image/png", ICON_192);
    }
    if req.method == "GET" && segs == ["icon-512.png"] {
        return Response::ok_bytes("image/png", ICON_512);
    }
    if segs.first().map(String::as_str) != Some("api") {
        return Response::not_found();
    }
    if let Err(r) = guard_api(app, req) {
        return r;
    }
    let rest: &[String] = &segs[1..];
    let needs_mutation_guard = matches!(req.method.as_str(), "POST" | "PUT" | "DELETE");
    if needs_mutation_guard {
        if let Err(r) = guard_mutation(app, req) {
            return r;
        }
    }
    route(app, req, rest)
}

fn route(app: &App, req: &Request, rest: &[String]) -> Response {
    let s = |i: usize| rest.get(i).map(String::as_str).unwrap_or("");
    match (req.method.as_str(), s(0), s(1), s(2), s(3)) {
        // Overview / runs
        ("GET", "overview", _, _, _) => runs::overview(app),
        ("GET", "runs", "", _, _) => runs::list(app, req),
        ("GET", "runs", id, "", _) => runs::detail(app, id),
        ("GET", "runs", id, "export", _) => runs::export(app, req, id),
        ("DELETE", "runs", id, _, _) => runs::prune(app, req, id),
        // Approvals
        ("GET", "approvals", _, _, _) => approvals::list(app),
        ("POST", "approvals", id, "grant", _) => approvals::decide(app, id, true),
        ("POST", "approvals", id, "deny", _) => approvals::decide(app, id, false),
        // Schedule
        ("GET", "schedule", "jobs", "", _) => schedule::list_jobs(app),
        ("POST", "schedule", "jobs", "", _) => schedule::create_job(app, req),
        ("PUT", "schedule", "jobs", id, _) => schedule::update_job(app, req, id),
        ("DELETE", "schedule", "jobs", id, _) => schedule::delete_job(app, id),
        ("POST", "schedule", "jobs", id, "trigger") => schedule::trigger_job(app, req, id),
        ("GET", "schedule", "templates", _, _) => schedule::list_templates(app),
        // Stats / memory
        ("GET", "stats", _, _, _) => stats::stats(app, req),
        ("GET", "memory", _, _, _) => memory::browse(app, req),
        // Config
        ("GET", "config", "", _, _) => config::get(app),
        ("GET", "config", "schema", _, _) => config::schema(app),
        ("PUT", "config", _, _, _) => config::put(app, req),
        ("GET", "config", "export", _, _) => config::export(app),
        ("POST", "config", "import", _, _) => config::import(app, req),
        // Env / keys
        ("GET", "env", "", _, _) => env::list(app),
        ("PUT", "env", _, _, _) => env::put(app, req),
        ("DELETE", "env", key, _, _) => env::delete(app, key),
        // Logs
        ("GET", "logs", "", _, _) => logs::tail(app, req),
        ("GET", "logs", "stream", _, _) => logs::stream(app, req),
        // Skills
        ("GET", "skills", "", _, _) => skills::list(app, req),
        ("POST", "skills", name, "toggle", _) => skills::toggle(app, name),
        ("POST", "skills", "import", _, _) => skills::import(app, req),
        ("DELETE", "skills", name, _, _) => skills::delete(app, name),
        // MCP
        ("GET", "mcp", "servers", "", _) => mcp::list(app),
        ("POST", "mcp", "servers", "", _) => mcp::add(app, req),
        ("DELETE", "mcp", "servers", name, _) => mcp::delete(app, name),
        ("POST", "mcp", "servers", name, "test") => mcp::test(app, name),
        ("POST", "mcp", "servers", name, "enable") => mcp::set_enabled(app, name, true),
        ("POST", "mcp", "servers", name, "disable") => mcp::set_enabled(app, name, false),
        ("POST", "mcp", "reload", _, _) => mcp::reload(app),
        // Gateway
        ("GET", "gateway", "status", _, _) => system::gateway_status(app),
        ("POST", "gateway", "restart", _, _) => system::gateway_restart(app, req),
        // Reflection / consolidation
        ("GET", "reflect", "status", _, _) => system::reflect_status(app),
        ("POST", "reflect", "run", _, _) => system::reflect_run(app, req),
        ("GET", "consolidate", "status", _, _) => system::consolidate_status(app),
        ("POST", "consolidate", "run", _, _) => system::consolidate_run(app, req),
        _ => Response::not_found(),
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Test/embedder hook: bind an ephemeral loopback port, serve the
/// dashboard on a background thread, and return `(port, token)`. The
/// server runs until the process exits.
pub fn spawn_test_server(data_dir: PathBuf) -> (u16, String) {
    let token = generate_token();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral dashboard port");
    let port = listener.local_addr().expect("local addr").port();
    let app = App {
        data_dir,
        token: token.clone(),
        bind: "127.0.0.1".to_string(),
        bind_all: false,
        on_approval: None,
    };
    std::thread::spawn(move || server::serve(listener, move |req| dispatch(&app, req)));
    // Give the listener a moment to start accepting.
    std::thread::sleep(std::time::Duration::from_millis(50));
    (port, token)
}

/// Start the dashboard. Never returns.
pub fn run(cfg: DashboardConfig) -> ! {
    let token = generate_token();
    let bind_all = cfg.bind == "0.0.0.0" || cfg.bind == "::";
    let addr = format!("{}:{}", cfg.bind, cfg.port);
    let listener = TcpListener::bind(&addr).unwrap_or_else(|e| {
        eprintln!("dashboard: cannot bind {addr}: {e}");
        std::process::exit(1);
    });
    let url = format!("http://{}:{}/?token={}", cfg.bind, cfg.port, token);
    println!("pantheon dashboard on {url}");
    if bind_all {
        eprintln!(
            "WARNING: dashboard is bound to a non-loopback address. The token is the only \
             protection. Put a reverse proxy with real auth in front, or keep it on 127.0.0.1."
        );
    } else {
        eprintln!("keep this URL private: the token is the dashboard's password.");
    }
    if cfg.open_browser {
        open_browser(&url);
    }
    let app = App {
        data_dir: cfg.data_dir,
        token,
        bind: cfg.bind,
        bind_all,
        on_approval: cfg.on_approval,
    };
    server::serve(listener, move |req| dispatch(&app, req))
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(target_os = "windows")]
    let opener = "cmd";
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let opener = "xdg-open";
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new(opener)
            .args(["/c", "start", url])
            .spawn();
    }
    #[cfg(not(target_os = "windows"))]
    {
        if std::process::Command::new(opener).arg(url).spawn().is_err() {
            eprintln!("dashboard: could not launch a browser for {url}");
        }
    }
}
