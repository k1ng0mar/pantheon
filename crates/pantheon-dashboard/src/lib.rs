//! Pantheon's web dashboard: control plane in the browser.
//!
//! Route handlers + PWA assets mounted on the gateway's single HTTP serve
//! surface (`pantheon_gateway::http`): a self-contained vanilla SPA
//! (embedded via `include_str!`, no build step, no npm) over Pantheon's
//! real stores: the event ledger, the scheduler's `schedule.json`, the
//! config file, the `.env` key store, log files, skills, MCP declarations,
//! the gateway service, and the reflection/consolidation engines.
//!
//! Security model (Hermes v0.17 learned this the hard way):
//! - A random 256-bit per-instance token is generated at startup and
//!   printed as `http://<bind>:<port>/?token=...`. Every `/api/*`
//!   request must carry it as `X-Pantheon-Token` or `?token=`.
//! - `POST`/`PUT`/`PATCH`/`DELETE` additionally require the `Host` header to
//!   match the bind address (DNS-rebinding guard) and reject a mismatched
//!   `Origin`/`Referer` (an absent one means a non-browser client like
//!   curl, which the token already authenticates).
//! - Default bind is 127.0.0.1. Binding anywhere else prints a loud
//!   warning: put a reverse proxy with auth in front.
//!
//! The token + mutation guards are enforced by the gateway's `check_auth`
//! before dispatch, from the [`AuthCtx`] the mount builds; this crate no
//! longer runs its own server - [`DashboardMount`] plugs the routing table
//! into the gateway listener.
//!
//! Design references (named, per brief):
//! - VibePrompt: metric-card row, sparklines, compact KPI tiles.
//! - Raycast via Refero Styles ("midnight command center"): `#040506`
//!   canvas, hairline borders, inset highlights, Inter + monospace
//!   micro-labels, neutral buttons, mono footer metadata. Accent is a
//!   restrained cyan (`#22d3ee`); green/amber/red are statuses only.

mod approvals;
mod browser;
mod config;
mod env;
mod experts;
mod ideas;
mod link_preview;
mod logins;
mod logs;
mod mcp;
mod memory;
mod plugin_import;
mod plugins;
mod profiles;
pub(crate) mod runs;
mod schedule;
mod session_factory;
mod skills;
mod stats;
/// Swarm routes: multi-agent fan-out through real subprocess turns.
/// Public so hosts constructing [`App`] (the TUI mounts) can build the
/// production orchestrator via
/// [`swarm::orchestrator_for`].
pub mod swarm;
mod system;
mod teams;
mod templates;
/// Public so the turn child and integration tests can resolve upload ids
/// to vision image parts without going through HTTP.
pub mod uploads;
mod util;

use pantheon_gateway::http::{AuthCtx, AuthGroup, HttpMount, Request, Response};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

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
    /// Per-run mutexes serializing turn-starting requests (`send_message`,
    /// `retry_turn`, approval resume). Guards the busy-check → drain →
    /// spawn sequence so two concurrent POSTs can't both see idle, both
    /// spawn, and silently drop the loser's message when its turn dies
    /// with RT_LEASE_BUSY.
    pub send_locks:
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<std::sync::Mutex<()>>>>,
    /// PIDs of turn children spawned by this dashboard process
    /// (`run --taskID <run_id> --say ...`), keyed by run id. Powers
    /// `POST /api/runs/:id/kill`: a PID recorded here is the only
    /// process the dashboard will ever signal, and the runtime
    /// identity-checks it against `/proc` before signaling.
    pub turn_children: std::sync::Mutex<std::collections::HashMap<String, u32>>,
    /// Multi-agent swarm orchestrator (see `swarm.rs`): production agents
    /// run as real subprocess turns via
    /// [`swarm::SubprocessWorker`]; the `[judge]` aux transport resolves
    /// from the dashboard's config.toml at construction.
    pub swarm: std::sync::Arc<pantheon_runtime::swarm_exec::SwarmOrchestrator>,
}

impl App {
    /// The serialization lock for one run's turn-starting requests.
    /// Cheap to clone; hold the guard across the whole check → spawn
    /// sequence.
    pub fn send_guard(&self, run_id: &str) -> std::sync::Arc<std::sync::Mutex<()>> {
        let mut map = self.send_locks.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(run_id.to_string())
            .or_insert_with(|| std::sync::Arc::new(std::sync::Mutex::new(())))
            .clone()
    }

    /// Record the turn child just spawned for `run_id`.
    pub fn register_turn_child(&self, run_id: &str, pid: u32) {
        let mut map = self.turn_children.lock().unwrap_or_else(|e| e.into_inner());
        map.insert(run_id.to_string(), pid);
    }

    /// The recorded turn-child PID for `run_id`, if any.
    pub fn turn_child_pid(&self, run_id: &str) -> Option<u32> {
        self.turn_children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(run_id)
            .copied()
    }

    /// Forget the recorded turn child: after a kill, or when the PID
    /// proved stale.
    pub fn forget_turn_child(&self, run_id: &str) {
        self.turn_children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(run_id);
    }
}

/// Spawn the pantheon binary itself as a subprocess: the dashboard calls
/// back into the real CLI code paths (`schedule run`, `gateway restart`,
/// `reflect`, `consolidate`) instead of reimplementing them. Fire and
/// forget - the caller already responded; the child must not inherit the
/// socket.
///
/// Returns the spawn result so turn-starting callers can keep the queued
/// message on failure instead of dropping a queue head they already
/// drained.
pub fn spawn_pantheon(args: &[&str]) -> std::io::Result<()> {
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
    cmd.spawn().map(|_| ())
}

/// Spawn the pantheon binary as a turn child with the turn text piped on
/// stdin while argv carries `--say -`. Keeps user message text out of
/// argv (and out of process listings) - the child resolves the `-`
/// through the same `resolve_say_stdin` path the CLI documents. The
/// payload is written before returning; the child is already running so
/// a large message cannot deadlock the pipe.
pub fn spawn_turn_child_with_stdin(args: &[&str], stdin_payload: &[u8]) -> std::io::Result<u32> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("pantheon"));
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args);
    cmd.stdin(std::process::Stdio::piped());
    #[cfg(unix)]
    {
        cmd.stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // Own process group: killpg(pid) later only ever signals
        // this turn's processes, never the dashboard itself.
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        // Best-effort: if the write fails the child exits 1 on empty
        // stdin rather than running a turn with no message.
        let _ = stdin.write_all(stdin_payload);
        // stdin closes on drop, signalling EOF to the child's read.
    }
    // Refuse to record PID 1 or our own PID, defensively.
    let pid = child.id();
    if pid <= 1 || pid == std::process::id() {
        return Err(std::io::Error::other(format!(
            "refusing to track turn child pid {pid}"
        )));
    }
    Ok(pid)
}

/// Spawn the pantheon binary as a turn child (`run --taskID <run_id>
/// --say ...`), in its own process group so `POST /api/runs/:id/kill`
/// can signal the whole group without touching the dashboard. Returns
/// the child PID (== its process group id after setsid).
///
/// Detached like [`spawn_pantheon`]: the child outlives the request and
/// must not inherit the socket. Callers register the PID with
/// [`App::register_turn_child`] so the kill endpoint can find it.
///
/// Prefer [`spawn_turn_child_with_stdin`] when the turn text is user
/// content: this variant leaves the message on argv, visible in process
/// listings.
pub fn spawn_turn_child(args: &[&str]) -> std::io::Result<u32> {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("pantheon"));
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args);
    #[cfg(unix)]
    {
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        // Own process group: killpg(pid) later only ever signals
        // this turn's processes, never the dashboard itself.
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let child = cmd.spawn()?;
    // Refuse to record PID 1 or our own PID, defensively.
    let pid = child.id();
    if pid <= 1 || pid == std::process::id() {
        return Err(std::io::Error::other(format!(
            "refusing to track turn child pid {pid}"
        )));
    }
    Ok(pid)
}

// ---------------------------------------------------------------------------
// JSON helpers
// ---------------------------------------------------------------------------

pub fn json_ok(v: serde_json::Value) -> Response {
    Response::ok_json(serde_json::to_string(&v).unwrap_or_else(|_| "{}".into()))
}

/// 201 with a JSON body, for endpoints that create a resource.
pub fn created_json(v: serde_json::Value) -> Response {
    Response::created_json(serde_json::to_string(&v).unwrap_or_else(|_| "{}".into()))
}

/// 409 with the standard error envelope, for state conflicts (finished or
/// parked runs, busy leases). `err_json` only maps 400/401/403/404, so
/// this needs its own constructor.
pub fn conflict(code: &str, msg: &str) -> Response {
    let body = serde_json::json!({"ok": false, "error": {"code": code, "message": msg}});
    Response::conflict_json(serde_json::to_string(&body).unwrap_or_else(|_| "{}".into()))
}

pub fn err_json(status: u16, code: &str, msg: &str) -> Response {
    let body = serde_json::json!({"ok": false, "error": {"code": code, "message": msg}});
    let text = serde_json::to_string(&body).unwrap_or_else(|_| "{}".into());
    match status {
        400 => Response::bad_request(&text),
        401 => Response::unauthorized(),
        403 => Response::forbidden(&text),
        404 => Response::Buffered {
            status: 404,
            content_type: "application/json",
            body: text.into_bytes(),
            extra_headers: Vec::new(),
        },
        409 => Response::Buffered {
            status: 409,
            content_type: "application/json",
            body: text.into_bytes(),
            extra_headers: Vec::new(),
        },
        422 => Response::Buffered {
            status: 422,
            content_type: "application/json",
            body: text.into_bytes(),
            extra_headers: Vec::new(),
        },
        413 => Response::Buffered {
            status: 413,
            content_type: "application/json",
            body: text.into_bytes(),
            extra_headers: Vec::new(),
        },
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
const TOKENS_CSS: &str = include_str!("../assets/tokens.css");
const APP_JS: &str = include_str!("../assets/app.js");
const MANIFEST_JSON: &str = include_str!("../assets/manifest.json");
const SW_JS: &str = include_str!("../assets/sw.js");
const ICON_192: &[u8] = include_bytes!("../assets/icon-192.png");
const ICON_512: &[u8] = include_bytes!("../assets/icon-512.png");

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// THE single path-normalization point for the dashboard mount.
///
/// Splits on '/', drops empty segments, then percent-decodes each segment,
/// in exactly this order, so an encoded '/' (`%2F`) never creates a new
/// segment. `auth_group` and `dispatch` MUST both go through this function:
/// the auth decision and the route decision have to see the same path, or
/// encoded/empty-segment variants (`/%61pi/runs`, `//api/runs`) slip past
/// auth into the real handler (P0 auth bypass, 2026-10-01). Do not add a
/// second normalizer; extend this one.
fn split_path(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(pantheon_gateway::http::percent_decode)
        .collect()
}

fn dispatch(app: &App, req: &Request) -> Response {
    let segs = split_path(&req.path);
    // Static shell: no token needed (it cannot do anything without one).
    if req.method == "GET" && segs.is_empty() {
        // Inject the build version so the sidebar can never drift from
        // the binary: the placeholder lives in assets/index.html and is
        // replaced from CARGO_PKG_VERSION at serve time.
        let html = INDEX_HTML.replace("__PANTHEON_VERSION__", env!("CARGO_PKG_VERSION"));
        return Response::Buffered {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: html.into_bytes(),
            extra_headers: Vec::new(),
        };
    }
    if req.method == "GET" && segs == ["tokens.css"] {
        return Response::ok_css(TOKENS_CSS);
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
        // Versioned cache name: injecting the build version here retires
        // the previous version's cached shell on upgrade instead of
        // serving it forever.
        let js = SW_JS.replace("__PANTHEON_VERSION__", env!("CARGO_PKG_VERSION"));
        return Response::Buffered {
            status: 200,
            content_type: "application/javascript; charset=utf-8",
            body: js.into_bytes(),
            extra_headers: Vec::new(),
        };
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
    // Auth (token for every /api/* request, Host/Origin guards for
    // mutations) is enforced by the gateway's `check_auth` before
    // dispatch, from the [`AuthGroup`] the mount declares for the path.
    let rest: &[String] = &segs[1..];
    route(app, req, rest)
}

fn route(app: &App, req: &Request, rest: &[String]) -> Response {
    let s = |i: usize| rest.get(i).map(String::as_str).unwrap_or("");
    // The table below destructures at most four path segments. A longer
    // path is never a real route: reject it here instead of silently
    // discarding the trailing segments (which once routed e.g.
    // `DELETE /api/runs/:id/queue/999/extra` to `delete_queue_item`).
    if rest.len() > 4 {
        // An /api/* path with more than four segments is never a route:
        // an API error, so the JSON envelope, not the plain 404 (B-15).
        return err_json(404, "not_found", "unknown api route");
    }
    match (req.method.as_str(), s(0), s(1), s(2), s(3)) {
        // Overview / runs
        ("GET", "overview", "", "", "") => runs::overview(app),
        ("GET", "runs", "", "", "") => runs::list(app, req),
        ("GET", "runs", id, "", "") => runs::detail(app, id),
        ("GET", "runs", id, "export", "") => runs::export(app, req, id),
        ("POST", "runs", "", "", "") => runs::create(app, req),
        ("POST", "runs", id, "message", "") => runs::send_message(app, id, req),
        ("POST", "runs", id, "retry", "") => runs::retry_turn(app, id),
        ("POST", "runs", id, "cancel", "") => runs::cancel_run(app, id),
        ("POST", "runs", id, "kill", "") => runs::kill_run(app, id),
        ("DELETE", "runs", id, "queue", idx) if !idx.is_empty() => {
            runs::delete_queue_item(app, id, idx)
        }
        ("PATCH", "runs", id, "queue", idx) if !idx.is_empty() => {
            runs::update_queue_item(app, id, idx, req)
        }
        ("DELETE", "runs", id, "queue", "") => runs::clear_queue(app, id),
        ("GET", "runs", id, "todos", "") => runs::get_todos(app, id),
        ("PUT", "runs", id, "todos", "") => runs::put_todos(app, id, req),
        ("POST", "runs", id, "compress", "") => runs::compress(app, id),
        ("POST", "runs", id, "fork", "") => runs::fork(app, id, req),
        ("PUT", "runs", id, "title", "") => runs::set_title(app, id, req),
        ("POST", "runs", id, "mode", "") => runs::set_mode(app, id, req),
        ("POST", "runs", id, "pin", "") => runs::set_pin(app, id, req),
        ("POST", "runs", id, "archive", "") => runs::set_archive(app, id, req),
        ("POST", "runs", id, "project", "") => runs::set_project(app, id, req),
        ("GET", "projects", "", "", "") => runs::projects(app),
        ("POST", "runs", id, "input", "") => runs::answer_input(app, id, req),
        // No catch-all here: only the exact run path prunes. Any other
        // DELETE under /api/runs/:id (e.g. a typoed sub-resource) falls
        // through to 404 instead of deleting the whole run.
        ("DELETE", "runs", id, "", "") if !id.is_empty() => runs::prune(app, req, id),
        // Link preview for chat link cards.
        ("GET", "link-preview", "", "", "") => link_preview::link_preview(req),
        // Ideas: nightly-gated proactive suggestions (see ideas.rs).
        // Additive section: no existing arm moved or reordered.
        ("GET", "ideas", "", "", "") => ideas::list(app),
        ("POST", "ideas", id, "accept", "") => ideas::accept(app, id),
        ("POST", "ideas", id, "dismiss", "") => ideas::dismiss(app, id),
        ("POST", "ideas", id, "feedback", "") => ideas::feedback(app, id, req),
        // Swarm: multi-agent fan-out through real subprocess turns.
        ("POST", "swarm", "", "", "") => swarm::create(app, req),
        ("GET", "swarm", "status", "", "") => swarm::status(app, req),
        ("GET", "swarm", "transcript", "", "") => swarm::transcript(app, req),
        ("POST", "swarm", id, "retry", "") => swarm::retry(app, id, req),
        // Teams: launchable multi-agent rosters (see teams.rs).
        ("GET", "teams", "", "", "") => teams::list(app),
        ("POST", "teams", "", "", "") => teams::create(app, req),
        ("GET", "teams", id, "", "") => teams::get(app, id),
        ("PUT", "teams", id, "", "") => teams::update(app, id, req),
        ("DELETE", "teams", id, "", "") => teams::delete(app, id),
        ("POST", "teams", id, "use", "") => teams::use_team(app, id, req),
        // Experts: individual expert-agent gallery (see experts.rs).
        ("GET", "experts", "", "", "") => experts::list(app),
        ("POST", "experts", "", "", "") => experts::create(app, req),
        ("GET", "experts", id, "", "") => experts::get(app, id),
        ("PUT", "experts", id, "", "") => experts::update(app, id, req),
        ("DELETE", "experts", id, "", "") => experts::delete(app, id),
        ("POST", "experts", id, "use", "") => experts::use_expert(app, id, req),
        // Approvals
        ("GET", "approvals", "", "", "") => approvals::list(app),
        ("POST", "approvals", id, "grant", "") => approvals::decide(app, id, true),
        ("POST", "approvals", id, "deny", "") => approvals::decide(app, id, false),
        // Schedule
        ("GET", "schedule", "jobs", "", "") => schedule::list_jobs(app),
        ("POST", "schedule", "jobs", "", "") => schedule::create_job(app, req),
        ("PUT", "schedule", "jobs", id, "") => schedule::update_job(app, req, id),
        ("DELETE", "schedule", "jobs", id, "") => schedule::delete_job(app, id),
        ("POST", "schedule", "jobs", id, "trigger") => schedule::trigger_job(app, req, id),
        ("GET", "schedule", "templates", "", "") => schedule::list_templates(app),
        ("POST", "schedule", "templates", "", "") => schedule::create_template(app, req),
        ("DELETE", "schedule", "templates", name, "") => schedule::delete_template(app, name),
        // Stats / memory
        ("GET", "stats", "", "", "") => stats::stats(app, req),
        ("GET", "memory", "", "", "") => memory::browse(app, req),
        ("POST", "memory", "", "", "") => memory::add(app, req),
        ("PUT", "memory", id, "", "") if !id.is_empty() => memory::replace(app, req, id),
        ("DELETE", "memory", id, "", "") if !id.is_empty() => memory::remove(app, req, id),
        // Config
        ("GET", "config", "", "", "") => config::get(app),
        ("GET", "config", "schema", "", "") => config::schema(app),
        ("PUT", "config", "", "", "") => config::put(app, req),
        ("GET", "config", "export", "", "") => config::export(app),
        ("POST", "config", "import", "", "") => config::import(app, req),
        // Env / keys
        ("GET", "env", "", "", "") => env::list(app),
        ("PUT", "env", "", "", "") => env::put(app, req),
        ("DELETE", "env", key, "", "") => env::delete(app, key),
        // Website-login credentials (browser take-control logins)
        ("GET", "logins", "", "", "") => logins::list(app),
        ("POST", "logins", "", "", "") => logins::create(app, req),
        ("PUT", "logins", id, "", "") => logins::update(app, id, req),
        ("DELETE", "logins", id, "", "") => logins::delete(app, id),
        // Browser take-control: live stream + input forwarding
        ("GET", "browser", "status", "", "") => browser::status(app, req),
        ("GET", "browser", "stream", "", "") => browser::stream(app, req),
        ("POST", "browser", "input", "", "") => browser::input(app, req),
        // Logs
        ("GET", "logs", "", "", "") => logs::tail(app, req),
        ("GET", "logs", "stream", "", "") => logs::stream(app, req),
        // Skills
        ("GET", "skills", "", "", "") => skills::list(app, req),
        ("POST", "skills", name, "toggle", "") => skills::toggle(app, name),
        ("POST", "skills", "import", "", "") => skills::import(app, req),
        ("DELETE", "skills", name, "", "") => skills::delete(app, name),
        // MCP
        ("GET", "mcp", "servers", "", "") => mcp::list(app),
        ("GET", "mcp", "health", "", "") => mcp::health(app),
        ("POST", "mcp", "servers", "", "") => mcp::add(app, req),
        ("DELETE", "mcp", "servers", name, "") => mcp::delete(app, name),
        ("POST", "mcp", "servers", name, "test") => mcp::test(app, name),
        ("POST", "mcp", "servers", name, "enable") => mcp::set_enabled(app, name, true),
        ("POST", "mcp", "servers", name, "disable") => mcp::set_enabled(app, name, false),
        ("POST", "mcp", "servers", name, "toggle") => mcp::toggle(app, name),
        ("POST", "mcp", "servers", name, "approve") => mcp::approve(app, name, req),
        ("POST", "mcp", "reload", "", "") => mcp::reload(app),
        // plugin-management routes
        ("GET", "plugins", "", "", "") => plugins::list(app, req),
        ("POST", "plugins", "import", "", "") => plugins::import(app, req),
        ("GET", "plugins", "registry", "search", "") => plugins::registry_search(app, req),
        ("POST", "plugins", kind, name, "approve") => plugins::approve(app, kind, name, req),
        ("POST", "plugins", kind, name, "disable") => plugins::disable(app, kind, name),
        // Agent profiles: persona-file manager (SOUL.md / USER.md / AGENTS.md)
        ("GET", "profiles", name, "files", "") => profiles::get_files(app, name),
        ("PUT", "profiles", name, "files", "") => profiles::put_files(app, req, name),
        ("DELETE", "profiles", name, "", "") => profiles::delete_profile(app, name),
        // Gateway
        ("GET", "gateway", "status", "", "") => system::gateway_status(app),
        ("POST", "gateway", "restart", "", "") => system::gateway_restart(app, req),
        // Reflection / consolidation
        ("GET", "reflect", "status", "", "") => system::reflect_status(app),
        ("POST", "reflect", "run", "", "") => system::reflect_run(app, req),
        ("GET", "consolidate", "status", "", "") => system::consolidate_status(app),
        ("POST", "consolidate", "run", "", "") => system::consolidate_run(app, req),
        // Nightly enable toggle: the dashboard / mobile-app surface for
        // the `[nightly] enabled` flag (enable path 4). Status reports
        // the resolved rule; the POST writes the explicit flag through
        // the shared config document.
        ("GET", "nightly", "status", "", "") => system::nightly_status(app),
        ("POST", "nightly", "enabled", "", "") => system::nightly_set_enabled(app, req),
        // File uploads backing chat attachments (mobile app): store first,
        // reference by id from `POST /api/runs/:id/message`.
        ("POST", "uploads", "", "", "") => uploads::create(app, req),
        ("GET", "uploads", id, "", "") => uploads::download(app, id),
        // Channel health, written by the gateway daemon(s) as
        // `<data_dir>/gateway/channel-health-*.json` (F-7). Unknown /api/*
        // routes are API errors: the envelope, not the plain 404 (B-15).
        ("GET", "health", "channels", "", "") => health_channels(app),
        _ => err_json(404, "not_found", "unknown api route"),
    }
}

/// `GET /api/health/channels` - per-channel state from the gateway
/// daemon's health snapshots (F-7): connected/degraded/dead, last error,
/// last successful poll. The gateway and the HTTP listener are different
/// processes, so the daemon writes JSON files and this route merges them.
fn health_channels(app: &App) -> Response {
    json_ok(pantheon_gateway::daemon::channel_health_json(&app.data_dir))
}

// ---------------------------------------------------------------------------
// Gateway mount
// ---------------------------------------------------------------------------

/// The dashboard as a route group on the gateway's single HTTP listener:
/// PWA shell assets + the `/api/*` control-plane routes. Auth groups are
/// declared per path; the gateway's `check_auth` enforces the dashboard's
/// pre-move rules (token for `/api/*`, Host/Origin guards for mutations)
/// before [`HttpMount::handle`] runs.
pub struct DashboardMount {
    pub app: App,
}

impl DashboardMount {
    pub fn new(app: App) -> Self {
        Self { app }
    }

    /// The one auth context for the listener, built from the same
    /// token/bind the dashboard was configured with. The CLI hands this
    /// to the gateway's [`ServerConfig`](pantheon_gateway::http::ServerConfig).
    pub fn auth_ctx(&self) -> AuthCtx {
        AuthCtx {
            token: self.app.token.clone(),
            bind: self.app.bind.clone(),
            bind_all: self.app.bind_all,
        }
    }
}

impl HttpMount for DashboardMount {
    fn auth_group(&self, req: &Request) -> Option<AuthGroup> {
        // ONE normalization point, shared with `dispatch` via `split_path`:
        // the auth decision must see exactly the path the handler will
        // route. Matching the raw request path here once let `/%61pi/runs`
        // (which normalizes to `["api", "runs"]`) slip through as Public,
        // running the real handler with no token and no Host/Origin check.
        // Any new routing-relevant path check must go through `split_path`,
        // never the raw `req.path`.
        let segs = split_path(&req.path);
        let first = segs.first().map(String::as_str);
        // Static shell assets (the exact set `dispatch` serves): public,
        // exactly as before the move.
        if req.method == "GET"
            && (segs.is_empty()
                || matches!(
                    first,
                    Some(
                        "style.css"
                            | "app.js"
                            | "manifest.json"
                            | "sw.js"
                            | "icon-192.png"
                            | "icon-512.png"
                    )
                ))
        {
            return Some(AuthGroup::Public);
        }
        if first == Some("api") {
            return Some(AuthGroup::DashboardApi);
        }
        // The AG-UI mount owns `/agui/*`: not ours.
        if first == Some("agui") {
            return None;
        }
        // Everything else (including unknown paths, which `dispatch` 404s)
        // stays unauthenticated, preserving the old "404 without auth"
        // behavior.
        Some(AuthGroup::Public)
    }

    fn handle(&self, req: &Request) -> Response {
        // The old serve surface rejected non GET/POST/PUT/PATCH/DELETE
        // methods with 405 before dispatch; preserve that here.
        match req.method.as_str() {
            "GET" | "POST" | "PUT" | "PATCH" | "DELETE" => dispatch(&self.app, req),
            _ => Response::method_not_allowed(),
        }
    }
}

// ---------------------------------------------------------------------------
// Legacy entry point (kept for API compatibility)
// ---------------------------------------------------------------------------

/// Dashboard startup configuration, from `pantheon dashboard` flags.
pub struct DashboardConfig {
    pub data_dir: PathBuf,
    pub bind: String,
    pub port: u16,
    pub open_browser: bool,
    pub on_approval: Option<ApprovalCallback>,
}

/// Start the dashboard. Never returns.
///
/// Compatibility wrapper over the mount-based internals: builds the same
/// `App` the gateway's unified listener uses (production swarm
/// orchestrator) and serves it on `bind:port` via
/// [`pantheon_gateway::http::serve`], preserving the old standalone
/// behavior - per-instance token printed at startup, bind-all warning,
/// `--open` handling.
pub fn run(cfg: DashboardConfig) -> ! {
    let token = pantheon_gateway::http::generate_token();
    let bind_all = cfg.bind == "0.0.0.0" || cfg.bind == "::";
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
        data_dir: cfg.data_dir.clone(),
        token,
        bind: cfg.bind.clone(),
        bind_all,
        on_approval: cfg.on_approval,
        send_locks: Default::default(),
        turn_children: Default::default(),
        swarm: swarm::orchestrator_for(&cfg.data_dir),
    };
    let mount = DashboardMount::new(app);
    let auth = mount.auth_ctx();
    let server_cfg = pantheon_gateway::http::ServerConfig {
        bind_addr: format!("{}:{}", cfg.bind, cfg.port),
        auth,
        mounts: vec![Arc::new(mount)],
        label: "pantheon dashboard".to_string(),
    };
    match pantheon_gateway::http::serve(server_cfg) {
        // `serve` diverges on success (`serve_on` is `-> !`); Ok is
        // unreachable in practice.
        Ok(()) => unreachable!("gateway serve() only returns on bind failure, as Err"),
        Err(e) => {
            eprintln!("dashboard: cannot serve {}:{}: {e}", cfg.bind, cfg.port);
            std::process::exit(1);
        }
    }
}

/// Best-effort `--open`: hand the URL to the OS browser; never fatal.
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

// ---------------------------------------------------------------------------
// (Other entry points live in the CLI: both `pantheon dashboard` and
// `pantheon serve` start the gateway's unified listener with this crate's
// `DashboardMount` plus the runtime's `AguiMount`.)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Regression tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod route_tests {
    use super::*;
    use pantheon_api::events::Event;
    use pantheon_api::message::{Message, ToolCallRef};
    use pantheon_api::provenance::Provenance;
    use pantheon_storage::Ledger;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(0);

    /// A dashboard `App` over a fresh temp ledger containing one run
    /// (`"run-1"`, seeded with a single `RunStarted` event).
    fn test_app() -> (App, PathBuf) {
        let n = TEST_DIR_SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "pantheon-dashboard-route-test-{}-{}",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp data dir");
        let ledger = Ledger::open(&dir.join("ledger.db")).expect("open ledger");
        ledger
            .append(&Event::RunStarted {
                run_id: "run-1".to_string(),
            })
            .expect("append RunStarted");
        drop(ledger);
        let app = App {
            data_dir: dir.clone(),
            token: "test-token".to_string(),
            bind: "127.0.0.1".to_string(),
            bind_all: false,
            on_approval: None,
            send_locks: Default::default(),
            turn_children: Default::default(),
            swarm: swarm::orchestrator_for(&dir),
        };
        (app, dir)
    }

    fn delete_req(path: &str, confirm: bool) -> Request {
        let mut query = HashMap::new();
        if confirm {
            query.insert("confirm".to_string(), "true".to_string());
        }
        Request {
            method: "DELETE".to_string(),
            path: path.to_string(),
            query,
            headers: HashMap::new(),
            body: Vec::new(),
        }
    }

    fn status_of(resp: &Response) -> u16 {
        match resp {
            Response::Buffered { status, .. } => *status,
            _ => panic!("expected a buffered response"),
        }
    }

    fn run_exists(data_dir: &Path, run_id: &str) -> bool {
        let ledger = Ledger::open(&data_dir.join("ledger.db")).expect("open ledger");
        !ledger.replay(run_id).expect("replay").is_empty()
    }

    /// P0 #2: a DELETE to a nonexistent sub-resource must 404 - and must
    /// NOT prune the run. (Before the fix the `("DELETE", "runs", id, _, _)`
    /// catch-all routed it to `runs::prune`, so `?confirm=true` silently
    /// deleted the whole run.)
    #[test]
    fn delete_unknown_subresource_404s_and_preserves_run() {
        let (app, dir) = test_app();
        let resp = dispatch(&app, &delete_req("/api/runs/run-1/todos", true));
        assert_eq!(
            status_of(&resp),
            404,
            "DELETE of an unknown sub-resource must 404"
        );
        assert!(
            run_exists(&dir, "run-1"),
            "the run must survive a mistyped DELETE"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The legitimate `DELETE /api/runs/:id` still prunes the run.
    #[test]
    fn delete_run_itself_still_prunes() {
        let (app, dir) = test_app();
        let resp = dispatch(&app, &delete_req("/api/runs/run-1", true));
        assert_eq!(
            status_of(&resp),
            200,
            "DELETE /api/runs/:id must still prune"
        );
        assert!(
            !run_exists(&dir, "run-1"),
            "the run must be gone after prune"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `DELETE /api/runs/:id/queue` (clear_queue) still routes after the
    /// catch-all is removed: no legitimate sub-route may break.
    #[test]
    fn delete_queue_still_routes() {
        let (app, dir) = test_app();
        let resp = dispatch(&app, &delete_req("/api/runs/run-1/queue", false));
        assert_eq!(
            status_of(&resp),
            200,
            "DELETE /api/runs/:id/queue must still clear the queue"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn plain_req(method: &str, path: &str) -> Request {
        Request {
            method: method.to_string(),
            path: path.to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: Vec::new(),
        }
    }

    fn json_req(method: &str, path: &str, body: &str) -> Request {
        let mut req = plain_req(method, path);
        req.body = body.as_bytes().to_vec();
        req
    }

    /// Seed `n` queued messages on `run-1` ("msg-0" .. "msg-{n-1}").
    fn seed_queue(data_dir: &Path, n: usize) {
        let ledger = Ledger::open(&data_dir.join("ledger.db")).expect("open ledger");
        for i in 0..n {
            ledger
                .set_queued_message("run-1", Some(&format!("msg-{i}")))
                .expect("seed queue item");
        }
    }

    fn queue_of(data_dir: &Path) -> Vec<String> {
        let ledger = Ledger::open(&data_dir.join("ledger.db")).expect("open ledger");
        ledger.queued_messages("run-1").expect("read queue")
    }

    /// P1 (trailing-segment truncation): `DELETE /api/runs/:id/queue/:idx`
    /// with a 5th segment must 404, not delete queue item 999. Before the
    /// fix the router discarded every segment past the 4th and routed this
    /// straight to `delete_queue_item`.
    #[test]
    fn delete_queue_item_with_extra_segment_404s_and_preserves_queue() {
        let (app, dir) = test_app();
        seed_queue(&dir, 1000);
        let resp = dispatch(
            &app,
            &plain_req("DELETE", "/api/runs/run-1/queue/999/extra"),
        );
        assert_eq!(
            status_of(&resp),
            404,
            "trailing path segments must not be silently dropped"
        );
        let q = queue_of(&dir);
        assert_eq!(
            q.len(),
            1000,
            "no queue item may be deleted by a malformed path"
        );
        assert_eq!(q[999], "msg-999", "queue item 999 must be intact");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Same class, mutating POST: `POST /api/runs/:id/cancel/extra` must
    /// 404. Before the fix the trailing `_` arm swallowed the 4th segment
    /// and routed it to `cancel_run` - the run was actually canceled (200).
    #[test]
    fn post_cancel_with_extra_segment_404s_and_preserves_run() {
        let (app, dir) = test_app();
        let resp = dispatch(&app, &plain_req("POST", "/api/runs/run-1/cancel/extra"));
        assert_eq!(
            status_of(&resp),
            404,
            "trailing path segments must not be silently dropped"
        );
        assert!(run_exists(&dir, "run-1"));
        let ledger = Ledger::open(&dir.join("ledger.db")).expect("open ledger");
        assert_eq!(
            ledger.status("run-1").expect("status").as_deref(),
            Some("running"),
            "the malformed cancel must not have canceled the run"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Same class, PATCH: `PATCH /api/runs/:id/queue/0/extra` must 404 and
    /// leave the queue item untouched. Before the fix it routed to
    /// `update_queue_item` and rewrote item 0 (200).
    #[test]
    fn patch_queue_item_with_extra_segment_404s_and_preserves_queue() {
        let (app, dir) = test_app();
        seed_queue(&dir, 1);
        let resp = dispatch(
            &app,
            &json_req(
                "PATCH",
                "/api/runs/run-1/queue/0/extra",
                r#"{"text":"edited"}"#,
            ),
        );
        assert_eq!(
            status_of(&resp),
            404,
            "trailing path segments must not be silently dropped"
        );
        assert_eq!(
            queue_of(&dir),
            vec!["msg-0".to_string()],
            "queue item 0 must be untouched"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Same class on a read route: `GET /api/runs/:id/todos/extra` must
    /// 404. Before the fix the trailing `_` arm accepted the 4th segment
    /// and returned the todos (200).
    #[test]
    fn get_todos_with_extra_segment_404s() {
        let (app, dir) = test_app();
        let resp = dispatch(&app, &plain_req("GET", "/api/runs/run-1/todos/extra"));
        assert_eq!(
            status_of(&resp),
            404,
            "trailing path segments must not be silently dropped"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- Positive controls: exact-shape routes still dispatch. ---

    /// The exact-shape `DELETE /api/runs/:id/queue/:idx` still deletes.
    #[test]
    fn delete_queue_item_exact_shape_still_works() {
        let (app, dir) = test_app();
        seed_queue(&dir, 2);
        let resp = dispatch(&app, &plain_req("DELETE", "/api/runs/run-1/queue/1"));
        assert_eq!(
            status_of(&resp),
            200,
            "DELETE /api/runs/:id/queue/:idx must still work"
        );
        assert_eq!(queue_of(&dir), vec!["msg-0".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The exact-shape `PATCH /api/runs/:id/queue/:idx` still updates.
    #[test]
    fn patch_queue_item_exact_shape_still_works() {
        let (app, dir) = test_app();
        seed_queue(&dir, 1);
        let resp = dispatch(
            &app,
            &json_req("PATCH", "/api/runs/run-1/queue/0", r#"{"text":"edited"}"#),
        );
        assert_eq!(
            status_of(&resp),
            200,
            "PATCH /api/runs/:id/queue/:idx must still work"
        );
        assert_eq!(queue_of(&dir), vec!["edited".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The exact-shape `POST /api/runs/:id/cancel` still reaches the
    /// handler (200 on a running run).
    #[test]
    fn post_cancel_exact_shape_still_reaches_handler() {
        let (app, dir) = test_app();
        // A bare RunStarted seed looks crash-orphaned to startup recovery
        // (running, no live lease) and gets settled before cancel runs.
        // A real in-flight run holds a live lease, so take one: this test
        // is about the route reaching the handler, not about recovery.
        let leases = pantheon_storage::RunLeaseStore::open(&dir.join("ledger.db"))
            .expect("open lease store");
        leases
            .acquire("run-1", "test-lease", 60_000)
            .expect("acquire test lease");
        let resp = dispatch(&app, &plain_req("POST", "/api/runs/run-1/cancel"));
        assert_eq!(
            status_of(&resp),
            200,
            "POST /api/runs/:id/cancel must still reach cancel_run"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The exact-shape `GET /api/runs/:id/todos` still works.
    #[test]
    fn get_todos_exact_shape_still_works() {
        let (app, dir) = test_app();
        let resp = dispatch(&app, &plain_req("GET", "/api/runs/run-1/todos"));
        assert_eq!(
            status_of(&resp),
            200,
            "GET /api/runs/:id/todos must still work"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- P0 (2026-10-01): auth_group must classify on the SAME normalized
    // path that dispatch routes. Before the fix it matched the RAW request
    // path, so encoded/empty-segment variants of /api/* (`/%61pi/runs`,
    // `//api/runs`) landed in AuthGroup::Public while dispatch decoded
    // them to ["api", ...] and ran the real handler with no token and no
    // Host/Origin check. ---

    /// Every raw path that normalizes to `["api", ...]` must be
    /// `DashboardApi` - exactly the set `dispatch` routes as API.
    #[test]
    fn auth_group_normalizes_api_prefix_before_classifying() {
        let (app, dir) = test_app();
        let mount = DashboardMount::new(app);
        let group = |method: &str, path: &str| mount.auth_group(&plain_req(method, path));
        for (method, path) in [
            ("GET", "/api/runs"),
            ("GET", "/api"),
            ("GET", "/%61pi/runs"),
            ("GET", "//api/runs"),
            ("GET", "//%61pi/runs"),
            ("GET", "/%61pi//runs"),
            ("GET", "/api//runs"),
            ("GET", "/api/../runs"),
            ("PATCH", "/%61pi/runs/run-1/queue/0"),
            ("PATCH", "/api/runs/run-1/%71ueue/0"),
            ("DELETE", "//api/runs/run-1"),
            ("DELETE", "/%61pi/runs/run-1"),
        ] {
            assert_eq!(
                group(method, path),
                Some(AuthGroup::DashboardApi),
                "{method} {path} normalizes to [\"api\", ...] and must be DashboardApi"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Shapes that do NOT normalize to `["api", ...]` keep their old
    /// groups: Public for unknown paths (dispatch 404s them), None for
    /// the AG-UI mount's paths.
    #[test]
    fn auth_group_leaves_non_api_shapes_alone() {
        let (app, dir) = test_app();
        let mount = DashboardMount::new(app);
        let group = |method: &str, path: &str| mount.auth_group(&plain_req(method, path));
        // "API" is not "api": Public, and dispatch 404s it.
        assert_eq!(group("GET", "/API/runs"), Some(AuthGroup::Public));
        // %2F decodes AFTER splitting, so this is one segment "api/runs".
        assert_eq!(group("GET", "/api%2f/runs"), Some(AuthGroup::Public));
        // Double-encoded: one decode pass leaves "%61pi", not "api".
        assert_eq!(group("GET", "/%2561pi/runs"), Some(AuthGroup::Public));
        // Static shell and unknown paths stay public (404 without auth).
        assert_eq!(group("GET", "/"), Some(AuthGroup::Public));
        assert_eq!(group("GET", "//"), Some(AuthGroup::Public));
        assert_eq!(group("GET", "/style.css"), Some(AuthGroup::Public));
        assert_eq!(group("POST", "/"), Some(AuthGroup::Public));
        assert_eq!(group("GET", "/nope"), Some(AuthGroup::Public));
        // The AG-UI mount owns /agui/* in any encoding that decodes to it.
        assert_eq!(group("GET", "/agui"), None);
        assert_eq!(group("GET", "/agui/stream"), None);
        assert_eq!(group("GET", "/%61gui/stream"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `PUT/DELETE /api/memory/:id` dispatch to the memory handlers, not
    /// the unknown-route 404: an unknown id must come back with the
    /// handlers' uppercase `NOT_FOUND` code (the unknown-route arm uses
    /// lowercase `not_found`).
    #[test]
    fn memory_item_routes_dispatch_to_handlers() {
        let (app, dir) = test_app();
        let mut q = HashMap::new();
        q.insert("namespace".to_string(), "dispatch-ns".to_string());
        for method in ["PUT", "DELETE"] {
            let req = Request {
                method: method.to_string(),
                path: "/api/memory/no-such-key".to_string(),
                query: q.clone(),
                headers: HashMap::new(),
                body: br#"{"text":"x"}"#.to_vec(),
            };
            match dispatch(&app, &req) {
                Response::Buffered { status, body, .. } => {
                    assert_eq!(status, 404, "{method}");
                    let v: serde_json::Value = serde_json::from_slice(&body).expect("json body");
                    assert_eq!(v["error"]["code"].as_str(), Some("NOT_FOUND"), "{method}");
                }
                _ => panic!("{method}: expected a buffered response"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The run detail transcript carries the message metadata the ledger
    /// already persists: `ts_ms`, per-request `tool_calls` with
    /// ledger-derived `started_ms`/`duration_ms`, and `tool_call_id` /
    /// top-level `duration_ms` on tool-result rows. Arguments stay on the
    /// redaction pass.
    /// A delegate step's `tool_calls` entry carries the child run it
    /// spawned, linked from the `AgentCompleted` ledger row; ordinary
    /// calls carry null. pre-link rows (no fields) stay null.
    #[test]
    fn run_detail_transcript_carries_child_run_link() {
        let (app, dir) = test_app();
        let ledger = Ledger::open(&dir.join("ledger.db")).expect("open ledger");
        let mut assistant = Message::assistant_tool_calls(vec![ToolCallRef {
            id: "call_9_0".to_string(),
            name: "delegate".to_string(),
            arguments: "{\"agent\": \"researcher\", \"task\": \"survey sources\"}".to_string(),
        }]);
        assistant.ts_ms = Some(1000);
        ledger
            .append(&Event::AssistantMessage {
                run_id: "run-1".to_string(),
                message: assistant,
            })
            .expect("append AssistantMessage");
        ledger
            .append(&Event::AgentCompleted {
                run_id: "run-1".to_string(),
                agent: "researcher".to_string(),
                child_run_id: Some("run-1-sub-1".to_string()),
                call_id: Some("call_9_0".to_string()),
            })
            .expect("append AgentCompleted");
        let mut tool = Message::tool("call_9_0", "done");
        tool.ts_ms = Some(2000);
        ledger
            .append(&Event::ToolMessage {
                run_id: "run-1".to_string(),
                message: tool,
            })
            .expect("append ToolMessage");
        drop(ledger);
        let resp = dispatch(&app, &plain_req("GET", "/api/runs/run-1"));
        assert_eq!(status_of(&resp), 200, "detail must load");
        let body = match &resp {
            Response::Buffered { body, .. } => body.clone(),
            _ => panic!("expected a buffered response"),
        };
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        let transcript = v["transcript"].as_array().expect("transcript array");
        let calls = transcript[0]["tool_calls"]
            .as_array()
            .expect("tool_calls array");
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0]["child_run_id"], "run-1-sub-1",
            "the delegate call links to its child run"
        );
    }

    #[test]
    fn run_detail_transcript_carries_tool_timing() {
        let (app, dir) = test_app();
        let ledger = Ledger::open(&dir.join("ledger.db")).expect("open ledger");
        let mut assistant = Message::assistant_tool_calls(vec![ToolCallRef {
            id: "call_1_0".to_string(),
            name: "shell".to_string(),
            arguments: "{\"cmd\": \"ls\"}".to_string(),
        }]);
        assistant.ts_ms = Some(1000);
        ledger
            .append(&Event::AssistantMessage {
                run_id: "run-1".to_string(),
                message: assistant,
            })
            .expect("append AssistantMessage");
        ledger
            .append(&Event::ToolStarted {
                run_id: "run-1".to_string(),
                call_id: "call_1_0".to_string(),
                tool: "shell".to_string(),
                args: "{\"cmd\": \"ls\"}".to_string(),
                provenance: Provenance::system("test"),
            })
            .expect("append ToolStarted");
        ledger
            .append(&Event::ToolCompleted {
                run_id: "run-1".to_string(),
                call_id: "call_1_0".to_string(),
                tool: "shell".to_string(),
                provenance: Provenance::system("test"),
            })
            .expect("append ToolCompleted");
        let mut tool = Message::tool("call_1_0", "total 0");
        tool.ts_ms = Some(2000);
        ledger
            .append(&Event::ToolMessage {
                run_id: "run-1".to_string(),
                message: tool,
            })
            .expect("append ToolMessage");
        drop(ledger);
        let resp = dispatch(&app, &plain_req("GET", "/api/runs/run-1"));
        assert_eq!(status_of(&resp), 200, "detail must load");
        let body = match &resp {
            Response::Buffered { body, .. } => body.clone(),
            _ => panic!("expected a buffered response"),
        };
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        let transcript = v["transcript"].as_array().expect("transcript array");
        assert_eq!(transcript.len(), 2, "assistant + tool rows");
        let assistant = &transcript[0];
        assert_eq!(assistant["type"], "message");
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["ts_ms"], 1000);
        let calls = assistant["tool_calls"]
            .as_array()
            .expect("tool_calls array");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "call_1_0");
        assert_eq!(calls[0]["name"], "shell");
        assert!(
            calls[0]["started_ms"].is_number(),
            "started_ms must come from the ToolStarted row"
        );
        assert!(
            calls[0]["duration_ms"].as_i64().unwrap_or(-1) >= 0,
            "duration_ms must come from ToolStarted -> ToolCompleted"
        );
        let tool = &transcript[1];
        assert_eq!(tool["role"], "tool");
        assert_eq!(tool["tool_call_id"], "call_1_0");
        assert_eq!(tool["ts_ms"], 2000);
        assert!(
            tool["duration_ms"].as_i64().unwrap_or(-1) >= 0,
            "tool-result rows carry the call duration at the top level"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// List helper: fetch `GET /api/runs<query>` and return the parsed
    /// `runs` array.
    fn run_list(app: &App, query: &str) -> serde_json::Value {
        let req = Request {
            method: "GET".to_string(),
            path: "/api/runs".to_string(),
            query: if query.is_empty() {
                HashMap::new()
            } else {
                query
                    .split('&')
                    .filter_map(|pair| {
                        let (k, v) = pair.split_once('=')?;
                        Some((k.to_string(), v.to_string()))
                    })
                    .collect()
            },
            headers: HashMap::new(),
            body: Vec::new(),
        };
        let resp = dispatch(app, &req);
        assert_eq!(status_of(&resp), 200, "run list must load");
        let body = match &resp {
            Response::Buffered { body, .. } => body.clone(),
            _ => panic!("expected a buffered response"),
        };
        serde_json::from_slice::<serde_json::Value>(&body).expect("json body")
    }

    fn listed_run(list: &serde_json::Value, id: &str) -> Option<serde_json::Value> {
        list["runs"]
            .as_array()
            .expect("runs array")
            .iter()
            .find(|r| r["id"] == id)
            .cloned()
    }

    /// `POST /api/runs/:id/pin` flips the pinned flag and the run list
    /// reflects it. Pinning never hides the run.
    #[test]
    fn pin_route_sets_flag_on_list() {
        let (app, dir) = test_app();
        let resp = dispatch(
            &app,
            &json_req("POST", "/api/runs/run-1/pin", r#"{"pinned": true}"#),
        );
        assert_eq!(status_of(&resp), 200, "pin must succeed");
        let run = listed_run(&run_list(&app, ""), "run-1").expect("run-1 listed");
        assert_eq!(run["pinned"], true, "pinned flag must be set");
        assert_eq!(run["archived"], false, "archived flag must be unset");
        assert!(
            run["project"].is_null(),
            "project must be null when unassigned"
        );
        // Unpin again.
        let resp = dispatch(
            &app,
            &json_req("POST", "/api/runs/run-1/pin", r#"{"pinned": false}"#),
        );
        assert_eq!(status_of(&resp), 200, "unpin must succeed");
        let run = listed_run(&run_list(&app, ""), "run-1").expect("run-1 listed");
        assert_eq!(run["pinned"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `POST /api/runs/:id/archive` hides the run from the list unless
    /// `?include_archived=1` is passed; unarchiving restores it.
    #[test]
    fn archive_route_hides_run_from_list() {
        let (app, dir) = test_app();
        let resp = dispatch(
            &app,
            &json_req("POST", "/api/runs/run-1/archive", r#"{"archived": true}"#),
        );
        assert_eq!(status_of(&resp), 200, "archive must succeed");
        assert!(
            listed_run(&run_list(&app, ""), "run-1").is_none(),
            "archived runs are hidden from the default list"
        );
        let run = listed_run(&run_list(&app, "include_archived=1"), "run-1")
            .expect("archived run listed with ?include_archived=1");
        assert_eq!(run["archived"], true);
        // Restore.
        let resp = dispatch(
            &app,
            &json_req("POST", "/api/runs/run-1/archive", r#"{"archived": false}"#),
        );
        assert_eq!(status_of(&resp), 200, "unarchive must succeed");
        assert!(
            listed_run(&run_list(&app, ""), "run-1").is_some(),
            "restored runs come back in the default list"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `POST /api/runs/:id/project` assigns and unassigns; `GET
    /// /api/projects` derives `[{name, runs}]` from the runs table.
    #[test]
    fn project_route_assigns_and_lists() {
        let (app, dir) = test_app();
        let resp = dispatch(
            &app,
            &json_req("POST", "/api/runs/run-1/project", r#"{"project": "alpha"}"#),
        );
        assert_eq!(status_of(&resp), 200, "project assign must succeed");
        let run = listed_run(&run_list(&app, ""), "run-1").expect("run-1 listed");
        assert_eq!(run["project"], "alpha");
        let resp = dispatch(&app, &plain_req("GET", "/api/projects"));
        assert_eq!(status_of(&resp), 200, "projects must load");
        let body = match &resp {
            Response::Buffered { body, .. } => body.clone(),
            _ => panic!("expected a buffered response"),
        };
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        let projects = v["projects"].as_array().expect("projects array");
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0]["name"], "alpha");
        assert_eq!(projects[0]["runs"].as_array().expect("runs array").len(), 1);
        assert_eq!(projects[0]["runs"][0], "run-1");
        // Unassign with null.
        let resp = dispatch(
            &app,
            &json_req("POST", "/api/runs/run-1/project", r#"{"project": null}"#),
        );
        assert_eq!(status_of(&resp), 200, "project unassign must succeed");
        let resp = dispatch(&app, &plain_req("GET", "/api/projects"));
        let body = match &resp {
            Response::Buffered { body, .. } => body.clone(),
            _ => panic!("expected a buffered response"),
        };
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert!(
            v["projects"].as_array().expect("projects array").is_empty(),
            "unassigning the only run drops the project"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Archived runs don't keep a project listed: archiving a project's
    /// last visible run drops it from `GET /api/projects` until a run is
    /// restored.
    #[test]
    fn projects_exclude_archived_runs() {
        let (app, dir) = test_app();
        let resp = dispatch(
            &app,
            &json_req("POST", "/api/runs/run-1/project", r#"{"project": "alpha"}"#),
        );
        assert_eq!(status_of(&resp), 200, "project assign must succeed");
        let resp = dispatch(
            &app,
            &json_req("POST", "/api/runs/run-1/archive", r#"{"archived": true}"#),
        );
        assert_eq!(status_of(&resp), 200, "archive must succeed");
        let resp = dispatch(&app, &plain_req("GET", "/api/projects"));
        assert_eq!(status_of(&resp), 200, "projects must load");
        let body = match &resp {
            Response::Buffered { body, .. } => body.clone(),
            _ => panic!("expected a buffered response"),
        };
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert!(
            v["projects"].as_array().expect("projects array").is_empty(),
            "a project whose only run is archived must not be listed"
        );
        // Restoring the run brings the project back.
        let resp = dispatch(
            &app,
            &json_req("POST", "/api/runs/run-1/archive", r#"{"archived": false}"#),
        );
        assert_eq!(status_of(&resp), 200, "unarchive must succeed");
        let resp = dispatch(&app, &plain_req("GET", "/api/projects"));
        let body = match &resp {
            Response::Buffered { body, .. } => body.clone(),
            _ => panic!("expected a buffered response"),
        };
        let v: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        let projects = v["projects"].as_array().expect("projects array");
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0]["name"], "alpha");
        assert_eq!(projects[0]["runs"][0], "run-1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Project body contract: an empty name unassigns (as documented,
    /// like null), an overlong name is rejected, and a non-string value
    /// is rejected.
    #[test]
    fn project_body_contract_edges() {
        let (app, dir) = test_app();
        let resp = dispatch(
            &app,
            &json_req("POST", "/api/runs/run-1/project", r#"{"project": "alpha"}"#),
        );
        assert_eq!(status_of(&resp), 200, "project assign must succeed");
        let resp = dispatch(
            &app,
            &json_req("POST", "/api/runs/run-1/project", r#"{"project": ""}"#),
        );
        assert_eq!(status_of(&resp), 200, "empty name must unassign, not 400");
        let run = listed_run(&run_list(&app, ""), "run-1").expect("run-1 listed");
        assert!(
            run["project"].is_null(),
            "empty name must clear the project"
        );
        let long = "x".repeat(129);
        let resp = dispatch(
            &app,
            &json_req(
                "POST",
                "/api/runs/run-1/project",
                &format!(r#"{{"project": "{long}"}}"#),
            ),
        );
        assert_eq!(status_of(&resp), 400, "overlong project name must 400");
        let resp = dispatch(
            &app,
            &json_req("POST", "/api/runs/run-1/project", r#"{"project": 42}"#),
        );
        assert_eq!(status_of(&resp), 400, "non-string project must 400");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The home session cannot be archived (mirrors prune's
    /// HOME_PROTECTED guard); restoring it is unaffected by the guard.
    #[test]
    fn archive_home_session_is_protected() {
        let (app, dir) = test_app();
        let resp = dispatch(
            &app,
            &json_req("POST", "/api/runs/home/archive", r#"{"archived": true}"#),
        );
        assert_eq!(status_of(&resp), 403, "archiving home must be refused");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Pin/archive/project on an unknown run 404s like the other
    /// run-scoped POSTs.
    #[test]
    fn pin_archive_project_unknown_run_404s() {
        let (app, dir) = test_app();
        for (path, body) in [
            ("/api/runs/nope/pin", r#"{"pinned": true}"#),
            ("/api/runs/nope/archive", r#"{"archived": true}"#),
            ("/api/runs/nope/project", r#"{"project": "alpha"}"#),
        ] {
            let resp = dispatch(&app, &json_req("POST", path, body));
            assert_eq!(status_of(&resp), 404, "{path} must 404");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
