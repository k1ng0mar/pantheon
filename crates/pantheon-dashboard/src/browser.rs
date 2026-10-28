//! Browser take-control endpoints: live screenshot stream + input forwarding.
//!
//! The dashboard drives the *same* configured browser backend the agent
//! loop uses ([`pantheon_runtime::build_browser_backend`] - identical
//! `[browser]` resolution, identical secret handling), through the
//! existing canonical tool surface
//! ([`BrowserBackend::invoke`] with `screenshot` / `eval` / `press` /
//! `navigate` / ... argv). No second browser driver.
//!
//! Endpoints (all under `/api/`, so the dashboard token auth applies):
//!
//! * `GET /api/browser/status` → `{enabled, backend, ready,
//!   binary_found, secret_present, detail, needs, last_activity}`.
//! * `GET /api/browser/stream?session=<name>&fps=<1-10>` → **WebSocket**.
//!   Server → client: one `{"type":"ready",...}` text frame, then binary PNG
//!   frames at the requested rate (default 2 fps). Capture failures
//!   arrive as `{"type":"error","message":...}` text frames; the stream
//!   keeps going. Client → server messages are ignored - input goes
//!   through `POST /api/browser/input`.
//! * `POST /api/browser/input` → `{session?, action, ...}`; forwards one
//!   action into the session. Actions: `tap` (`x`, `y` CSS px),
//!   `type` (`text` into the focused element), `scroll` (`dx`, `dy`),
//!   `press` (`key`), `navigate` (`url`), `back`, `forward`, `reload`.
//!
//! Why WebSocket and not MJPEG/SSE: the transport already has the WS
//! takeover primitive (used by live voice), tungstenite frames binary
//! PNGs with no extra encoding, and the socket stays open for the app's
//! future client → server input without a second connection. The stream
//! is one-way today by choice, not by limitation.
//!
//! Sessions are named (`?session=` / `"session"`, sanitized, default
//! `"default"`). One caveat the app must know: browser sessions are
//! per-process. The dashboard drives its *own* backend instance - with
//! the `gsd` backend, session names address the shared external daemon
//! (`--session`), so the dashboard can attach to the agent's session by
//! name; with `camofox` (in-process shim children) the dashboard's
//! sessions are separate from the agent loop's.

use crate::{bad_json, body_json, err_json, json_ok, App};
use pantheon_api::events::Event;
use pantheon_gateway::http::{Request, Response};
use pantheon_web::browser::tools::host_of;
use pantheon_web::browser::{sanitize_session_name, BrowserBackend};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

/// Default stream rate when `fps` is absent.
const DEFAULT_FPS: u32 = 2;
/// Hard bounds for `fps` (a 10 fps PNG loop is already generous).
const MAX_FPS: u32 = 10;

/// Build the configured browser backend for this dashboard server.
/// Errors are API responses: 503 when the browser tool is disabled,
/// 500 when the backend fails to build (bad config, missing binary...).
fn backend_for(app: &App) -> Result<Arc<dyn BrowserBackend>, Response> {
    let cfg = crate::session_factory::load_config(&app.data_dir)
        .map_err(|e| err_json(500, "BROWSER", &format!("load config: {e}")))?;
    let section = cfg
        .as_ref()
        .and_then(|c| c.browser.clone())
        .unwrap_or_default();
    let tool_cfg = pantheon_runtime::resolve_browser_section(&section);
    if !tool_cfg.enabled {
        return Err(err_json(
            503,
            "BROWSER_DISABLED",
            "browser tool is disabled ([browser] enabled = false)",
        ));
    }
    let secrets = crate::session_factory::secrets_broker(cfg.as_ref());
    pantheon_runtime::build_browser_backend(&tool_cfg, &secrets)
        .map_err(|e| err_json(500, "BROWSER", &format!("build browser backend: {e}")))?
        .ok_or_else(|| err_json(503, "BROWSER_DISABLED", "browser tool is disabled"))
}

/// Readiness of the configured browser backend, without launching
/// anything and without resolving secret *values* (presence only).
/// This is the honest version of `enabled`/`backend`: the old status
/// reported `backend: "gsd"` even with no `gsd-browser` on the host,
/// and the gap only surfaced as a 500 on the first input/stream call.
/// The blockers are named exactly: for the recommended `gsd` backend
/// the blocker is the binary (gsd-browser on PATH, or `[browser]
/// binary`) plus Chrome/Chromium on the host - never a provider API
/// key (GSD's vault key is optional: an unresolved vault key does not
/// block it). `steel` is the hosted/keyed one (its key, or a
/// self-hosted base URL); `camofox` is keyless (no API key at all -
/// only a proxy password if a proxy is configured).
struct BrowserReadiness {
    ready: bool,
    binary_found: Option<bool>,
    secret_present: Option<bool>,
    detail: String,
}

fn on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path)
        .any(|dir| dir.join(name).is_file())
}

fn binary_found(explicit: Option<&std::path::PathBuf>, path_name: &str) -> bool {
    match explicit {
        Some(p) => p.is_file(),
        None => on_path(path_name),
    }
}

fn browser_readiness(
    cfg: &Option<pantheon_api::config::Config>,
    tool_cfg: &pantheon_runtime::BrowserToolConfig,
) -> BrowserReadiness {
    if !tool_cfg.enabled {
        return BrowserReadiness {
            ready: false,
            binary_found: None,
            secret_present: None,
            detail: "browser tool is disabled ([browser] enabled = false)".to_string(),
        };
    }
    let broker = crate::session_factory::secrets_broker(cfg.as_ref());
    let secret_present = |name: Option<&String>| -> Option<bool> {
        name.map(|n| broker.resolve(n).ok().flatten().is_some())
    };
    match tool_cfg.backend.id() {
        "gsd" => {
            let found = binary_found(tool_cfg.binary.as_ref(), "gsd-browser");
            let vault = secret_present(tool_cfg.vault_key_secret.as_ref());
            let detail = if !found {
                "gsd-browser binary not found: install with `npm install -g @opengsd/gsd-browser` or set [browser] binary (plus Chrome/Chromium on the host)".to_string()
            } else {
                match vault {
                    Some(true) => "gsd-browser found; vault key resolved".to_string(),
                    Some(false) => {
                        "gsd-browser found; no vault key resolved (vault is optional, not a blocker)".to_string()
                    }
                    None => "gsd-browser found; no vault configured".to_string(),
                }
            };
            BrowserReadiness {
                ready: found,
                binary_found: Some(found),
                secret_present: vault,
                detail,
            }
        }
        "chromiumoxide" => {
            let found = match tool_cfg.chrome_binary.as_ref() {
                Some(p) => p.is_file(),
                None => [
                    "google-chrome",
                    "google-chrome-stable",
                    "chromium",
                    "chromium-browser",
                    "chrome",
                ]
                .iter()
                .any(|n| on_path(n)),
            };
            BrowserReadiness {
                ready: found,
                binary_found: Some(found),
                secret_present: None,
                detail: if found {
                    "Chrome/Chromium found".to_string()
                } else {
                    "no Chrome/Chromium binary found on PATH (or [browser] chrome_binary)".to_string()
                },
            }
        }
        "steel" => {
            let key = secret_present(tool_cfg.steel_api_key_secret.as_ref());
            let self_hosted = tool_cfg
                .steel_base_url
                .as_deref()
                .is_some_and(|u| !u.trim().is_empty());
            let ready = key == Some(true) || self_hosted;
            BrowserReadiness {
                ready,
                binary_found: None,
                secret_present: key,
                detail: if ready {
                    "steel configured".to_string()
                } else {
                    "steel needs an API key (STEEL_API_KEY secret) or a self-hosted steel_base_url".to_string()
                },
            }
        }
        "browserbase" => {
            let key = secret_present(tool_cfg.browserbase_api_key_secret.as_ref());
            let project = tool_cfg
                .browserbase_project_id
                .as_deref()
                .is_some_and(|p| !p.trim().is_empty());
            let ready = key == Some(true) && project;
            BrowserReadiness {
                ready,
                binary_found: None,
                secret_present: key,
                detail: if ready {
                    "browserbase configured".to_string()
                } else {
                    "browserbase needs BROWSERBASE_API_KEY and a browserbase project id".to_string()
                },
            }
        }
        "playwright" => {
            let found = binary_found(tool_cfg.playwright_binary.as_ref(), "playwright-cli");
            BrowserReadiness {
                ready: found,
                binary_found: Some(found),
                secret_present: None,
                detail: if found {
                    "playwright-cli found".to_string()
                } else {
                    "playwright-cli not found on PATH (or [browser] playwright_binary)".to_string()
                },
            }
        }
        // camofox: keyless by design - no API key exists for it. The
        // camoufox Python package and fetched browser are checked at
        // first use, so status reports ready with that caveat rather
        // than inventing a binary check it does not perform.
        _ => BrowserReadiness {
            ready: true,
            binary_found: None,
            secret_present: None,
            detail: "camofox is keyless: no API key needed (only a proxy password if a proxy is configured); the camoufox Python package and fetched browser are checked at first use".to_string(),
        },
    }
}

/// `GET /api/browser/status?session=<name>` →
/// `{enabled, backend, ready, binary_found, secret_present, detail,
/// needs, note, last_activity}`.
/// `last_activity` is `{action, detail, at}` (RFC-3339 UTC) for the
/// session's latest browser narration - what the agent or take-control
/// last did - or `null` when nothing was recorded yet. Never fails:
/// reports the configured state even when the backend itself would not
/// build, and a dead ledger degrades `last_activity` to null. `ready`
/// is the gap check the old status lacked: a configured backend with
/// a missing binary or key reports `ready: false` with the exact gap
/// in `detail`, instead of looking healthy until the first input call
/// 500s.
pub fn status(app: &App, req: &Request) -> Response {
    let cfg = crate::session_factory::load_config(&app.data_dir).unwrap_or(None);
    let section = cfg
        .as_ref()
        .and_then(|c| c.browser.clone())
        .unwrap_or_default();
    let tool_cfg = pantheon_runtime::resolve_browser_section(&section);
    let readiness = browser_readiness(&cfg, &tool_cfg);
    let needs = pantheon_web::browser::all_backends()
        .into_iter()
        .find(|b| b.kind == tool_cfg.backend)
        .map(|b| b.needs.to_string());
    let session = session_name(req.query.get("session").map(String::as_str));
    json_ok(serde_json::json!({
        "enabled": tool_cfg.enabled,
        "backend": tool_cfg.backend.id(),
        "ready": readiness.ready,
        "binary_found": readiness.binary_found,
        "secret_present": readiness.secret_present,
        "detail": readiness.detail,
        "needs": needs,
        "note": "sessions are per-process; see module docs",
        "last_activity": latest_activity(app, &session),
    }))
}

/// Latest browser narration for `session` as JSON, or null. Ledger
/// failures degrade to null - status must never fail.
fn latest_activity(app: &App, session: &str) -> serde_json::Value {
    let view = pantheon_storage::Ledger::open(&app.data_dir.join("ledger.db"))
        .and_then(|l| l.browser_activity(session));
    match view {
        Ok(Some(v)) => serde_json::json!({
            "action": v.action,
            "detail": v.detail,
            "at": ts_rfc3339(v.ts_ms),
        }),
        _ => serde_json::Value::Null,
    }
}

/// Format epoch millis as RFC-3339 UTC (`2026-09-30T15:14:27.123Z`).
/// Hand-rolled civil date (Howard Hinnant's algorithm): the dashboard
/// is std-only and this is the only place that needs an ISO timestamp.
pub(crate) fn ts_rfc3339(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let z = secs.div_euclid(86400) + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let sod = secs.rem_euclid(86400);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y,
        m,
        d,
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60,
        millis
    )
}

/// Session name from a query param or JSON body field: sanitized,
/// default `"default"`.
fn session_name(raw: Option<&str>) -> String {
    let s = raw.unwrap_or("default");
    sanitize_session_name(s)
}

/// Map a take-control action + body to narration `(action, detail)`.
/// Mirrors `pantheon_web::browser::tools::activity_of` for the
/// dashboard's own action vocabulary (tap/type/scroll/...). Typed text
/// never lands in `detail` - it may contain credentials or other
/// secrets, so it is replaced with a placeholder.
fn takeover_activity(action: &str, body: &serde_json::Value) -> (String, String) {
    let field = |key: &str| body.get(key).and_then(|v| v.as_str()).unwrap_or("");
    let detail = match action {
        "navigate" => host_of(field("url")),
        "press" => field("key").to_string(),
        "type" => "typing into a field".to_string(),
        _ => String::new(),
    };
    (action.to_string(), detail)
}

/// Record take-control narration into the ledger. Best-effort: a dead
/// ledger must never fail the input itself. No run context exists on
/// the take-control path, so `run_id` is empty and consumers key on
/// `session` - the same contract as the agent path in
/// `pantheon-runtime`.
fn record_takeover_activity(app: &App, session: &str, action: &str, detail: String) {
    let ledger = match pantheon_storage::Ledger::open(&app.data_dir.join("ledger.db")) {
        Ok(l) => l,
        Err(_) => return,
    };
    let _ = ledger.append(&Event::BrowserActivity {
        run_id: String::new(),
        session: session.to_string(),
        action: action.to_string(),
        detail,
    });
}

/// Translate one take-control action into the canonical browser argv.
/// Pure function - unit-tested. Everything goes through
/// [`BrowserBackend::invoke`]; coordinate actions use the backend's
/// `eval` command (JS in the page), exactly like the tool surface does.
pub(crate) fn input_argv(action: &str, body: &serde_json::Value) -> Result<Vec<String>, String> {
    let num = |key: &str| -> Result<f64, String> {
        body.get(key)
            .and_then(|v| v.as_f64())
            .filter(|f| f.is_finite())
            .ok_or_else(|| format!("'{key}' must be a finite number"))
    };
    let str_field = |key: &str| -> Result<String, String> {
        body.get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| format!("'{key}' must be a non-empty string"))
    };
    match action {
        "tap" => {
            let (x, y) = (num("x")?, num("y")?);
            Ok(vec![
                "eval".to_string(),
                format!(
                    "(() => {{ const el = document.elementFromPoint({x}, {y}); \
                     if (!el) return 'miss'; el.click(); return 'hit'; }})()"
                ),
            ])
        }
        "type" => {
            let text = str_field("text")?;
            // JSON string literal = valid JS string literal.
            let lit = serde_json::to_string(&text).map_err(|e| e.to_string())?;
            Ok(vec![
                "eval".to_string(),
                format!(
                    "(() => {{ const el = document.activeElement; \
                     if (!el || !('value' in el)) return 'no-focus'; \
                     el.focus(); el.value = {lit}; \
                     el.dispatchEvent(new Event('input', {{bubbles: true}})); \
                     el.dispatchEvent(new Event('change', {{bubbles: true}})); \
                     return 'ok'; }})()"
                ),
            ])
        }
        "scroll" => {
            let dx = body
                .get("dx")
                .and_then(|v| v.as_f64())
                .filter(|f| f.is_finite())
                .unwrap_or(0.0);
            let dy = body
                .get("dy")
                .and_then(|v| v.as_f64())
                .filter(|f| f.is_finite())
                .unwrap_or(0.0);
            Ok(vec![
                "eval".to_string(),
                format!("window.scrollBy({dx}, {dy}); 'ok'"),
            ])
        }
        "press" => Ok(vec!["press".to_string(), str_field("key")?]),
        "navigate" => Ok(vec!["navigate".to_string(), str_field("url")?]),
        "back" | "forward" | "reload" => Ok(vec![action.to_string()]),
        other => Err(format!(
            "unknown action '{other}': tap | type | scroll | press | navigate | back | forward | reload"
        )),
    }
}

/// `POST /api/browser/input` - forward one action into the browser session.
///
/// Body: `{session?, action, x?, y?, text?, key?, url?, dx?, dy?}`.
/// Response: `{"ok": true, "action": ..., "result": <backend JSON>}`.
pub fn input(app: &App, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let action = match body
        .get("action")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(a) => a.to_string(),
        None => return bad_json("action is required"),
    };
    let argv = match input_argv(&action, &body) {
        Ok(a) => a,
        Err(e) => return bad_json(&e),
    };
    let backend = match backend_for(app) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let session = session_name(body.get("session").and_then(|v| v.as_str()));
    // Narrate the take-control gesture the same way the agent's own tool
    // calls are narrated, so the app subtitle covers human input too.
    // Recorded before the invoke so the subtitle is live while the
    // action runs. Best-effort: a dead ledger must not fail the input.
    let (act, detail) = takeover_activity(&action, &body);
    record_takeover_activity(app, &session, &act, detail);
    // The action words are ours (input_argv); only the session name is
    // user-supplied, and it is sanitized. Nothing secret in the argv.
    match backend.invoke(&argv, &session) {
        Ok(result) => json_ok(serde_json::json!({
            "ok": true,
            "action": action,
            "session": session,
            "result": result,
        })),
        Err(e) => err_json(502, "BROWSER_INPUT", &e.to_string()),
    }
}

/// Replays the already-consumed HTTP request head, then delegates to the
/// socket, so `tungstenite::accept` can parse the WS handshake itself
/// the same trick live voice uses.
pub(crate) struct HeadReplay {
    pub(crate) head: Vec<u8>,
    pub(crate) pos: usize,
    pub(crate) stream: TcpStream,
}

impl HeadReplay {
    pub(crate) fn stream_mut(&mut self) -> &mut TcpStream {
        &mut self.stream
    }
}

impl Read for HeadReplay {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos < self.head.len() {
            let n = (self.head.len() - self.pos).min(buf.len());
            buf[..n].copy_from_slice(&self.head[self.pos..self.pos + n]);
            self.pos += n;
            return Ok(n);
        }
        self.stream.read(buf)
    }
}

impl Write for HeadReplay {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.stream.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

/// `GET /api/browser/stream?session=<name>&fps=<1-10>` - WebSocket
/// upgrade; binary PNG frames of the live page.
pub fn stream(app: &App, req: &Request) -> Response {
    let backend = match backend_for(app) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let session = session_name(req.query.get("session").map(String::as_str));
    let fps = req
        .query
        .get("fps")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(DEFAULT_FPS)
        .clamp(1, MAX_FPS);
    // Rebuild the raw head for the WS handshake (the HTTP layer already
    // consumed it). Query string included - tungstenite ignores it.
    let qs: Vec<String> = req.query.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let target = if qs.is_empty() {
        req.path.clone()
    } else {
        format!("{}?{}", req.path, qs.join("&"))
    };
    let mut head = format!("{} {target} HTTP/1.1\r\n", req.method);
    for (k, v) in &req.headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let head = head.into_bytes();
    // One frame dir per viewer connection (never one per process): two
    // concurrent viewers of the same session must not share `frame.png`
    // - they would read each other's screenshots - and the disconnect
    // cleanup in `serve_stream` must not nuke a live viewer's directory.
    let frame_dir = frame_dir_for(&session, &pantheon_runtime::new_run_id());
    Response::Takeover {
        run: Box::new(move |stream| serve_stream(stream, head, backend, session, fps, frame_dir)),
    }
}

/// Per-connection screenshot frame directory. The nonce (a fresh id per
/// `stream()` call) makes every connection's dir unique even for the same
/// session; the session name is already sanitized by [`session_name`].
fn frame_dir_for(session: &str, nonce: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "pantheon-browser-stream-{}-{}-{}",
        std::process::id(),
        session,
        nonce
    ))
}

pub(crate) fn ws_text(ws: &mut tungstenite::WebSocket<HeadReplay>, text: &str) {
    let _ = ws.send(tungstenite::Message::Text(text.to_string().into()));
}

/// The stream loop: capture → PNG → binary frame, at `fps`. Runs on the
/// connection thread until the client disconnects. Capture failures are
/// reported as text frames; the loop survives them (the page may still
/// be loading or the browser launching).
fn serve_stream(
    stream: TcpStream,
    head: Vec<u8>,
    backend: Arc<dyn BrowserBackend>,
    session: String,
    fps: u32,
    frame_dir: std::path::PathBuf,
) {
    let replay = HeadReplay {
        head,
        pos: 0,
        stream,
    };
    let mut ws = match tungstenite::accept(replay) {
        Ok(ws) => ws,
        Err(e) => {
            eprintln!("browser stream: websocket handshake failed: {e}");
            return;
        }
    };
    if std::fs::create_dir_all(&frame_dir).is_err() {
        ws_text(
            &mut ws,
            r#"{"type":"error","message":"cannot create frame dir"}"#,
        );
        let _ = ws.close(None);
        return;
    }
    let frame_path = frame_dir.join("frame.png");
    ws_text(
        &mut ws,
        &format!(
            "{{\"type\":\"ready\",\"session\":{}}}",
            serde_json::to_string(&session).unwrap_or_default()
        ),
    );
    // Client liveness: each loop iteration ends with a read bounded by
    // the frame interval. A quiet client just yields timeouts; a Close
    // frame or a dead socket ends the loop.
    let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
    let _ = ws.get_mut().stream_mut().set_read_timeout(Some(interval));
    let shot_argv = |p: &str| {
        vec![
            "screenshot".to_string(),
            "--output".to_string(),
            p.to_string(),
            "--format".to_string(),
            "png".to_string(),
        ]
    };
    loop {
        let frame_str = frame_path.to_string_lossy().to_string();
        match backend.invoke(&shot_argv(&frame_str), &session) {
            Ok(_) => match std::fs::read(&frame_path) {
                Ok(bytes) => {
                    if ws.send(tungstenite::Message::Binary(bytes.into())).is_err() {
                        break;
                    }
                }
                Err(e) => ws_text(
                    &mut ws,
                    &format!(
                        "{{\"type\":\"error\",\"message\":\"screenshot file unreadable: {e}\"}}"
                    ),
                ),
            },
            // The argv here is just screenshot --output <tmp path>: no
            // secrets, but keep the message short anyway.
            Err(e) => ws_text(
                &mut ws,
                &format!(
                    "{{\"type\":\"error\",\"message\":{}}}",
                    serde_json::to_string(&short_err(&e.to_string()))
                        .unwrap_or_else(|_| "\"capture failed\"".into())
                ),
            ),
        }
        match ws.read() {
            Ok(tungstenite::Message::Close(_)) => break,
            Ok(_) => {} // client → server messages are ignored by design
            Err(tungstenite::Error::ConnectionClosed) => break,
            Err(tungstenite::Error::AlreadyClosed) => break,
            Err(tungstenite::Error::Io(e))
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => break,
        }
    }
    let _ = std::fs::remove_dir_all(&frame_dir);
    let _ = ws.close(None);
}

/// First line, capped - error text frames stay small.
fn short_err(e: &str) -> String {
    let line = e.lines().next().unwrap_or("capture failed");
    const MAX: usize = 300;
    if line.len() > MAX {
        format!("{}...", &line[..MAX])
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn test_app_with_config(config: Option<&str>) -> (App, std::path::PathBuf) {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "pantheon-dashboard-browser-test-{}-{}",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp data dir");
        if let Some(text) = config {
            std::fs::write(dir.join("config.toml"), text).expect("seed config");
        }
        let app = App {
            data_dir: dir.clone(),
            token: "test-token".to_string(),
            bind: "127.0.0.1".to_string(),
            bind_all: false,
            on_approval: None,
            send_locks: Default::default(),
            ui_action_locks: Default::default(),
            turn_children: Default::default(),
            swarm: crate::swarm::orchestrator_for(&dir),
            desktop: Default::default(),
        };
        (app, dir)
    }

    fn status_json(app: &App) -> (u16, serde_json::Value) {
        let req = Request {
            method: "GET".to_string(),
            path: "/api/browser/status".to_string(),
            query: HashMap::new(),
            headers: HashMap::new(),
            body: Vec::new(),
        };
        match status(app, &req) {
            Response::Buffered { status, body, .. } => {
                (status, serde_json::from_slice(&body).unwrap_or_default())
            }
            _ => panic!("expected a buffered response"),
        }
    }

    /// Sweep (dashboard/API): the old status reported `backend: gsd`
    /// with no hint that the binary was missing, and the gap only
    /// surfaced as a 500 on first input. An unresolvable gsd binary
    /// must report ready:false naming gsd-browser.
    #[test]
    fn status_gsd_missing_binary_reports_gap() {
        let (app, dir) = test_app_with_config(Some(
            "[browser]\nbinary = \"/nonexistent/pantheon-sweep-gsd-browser\"\n",
        ));
        let (code, body) = status_json(&app);
        assert_eq!(code, 200, "status never fails: {body}");
        assert_eq!(body["backend"], "gsd");
        assert_eq!(body["ready"], false);
        assert_eq!(body["binary_found"], false);
        assert!(
            body["detail"].as_str().unwrap_or("").contains("gsd-browser"),
            "detail must name the missing binary: {body}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// GSD's vault key is optional, not a blocker: a found binary with
    /// an unresolvable vault secret is still ready, and the detail
    /// says the vault absence is not a blocker (never a key demand).
    #[test]
    fn status_gsd_binary_found_vault_absence_is_not_a_blocker() {
        let (app, dir) = test_app_with_config(Some(
            "[browser]\nbinary = \"/bin/true\"\nvault_key_secret = \"PANTHEON_SWEEP_TEST_ABSENT_VAULT_XYZ\"\n",
        ));
        let (code, body) = status_json(&app);
        assert_eq!(code, 200, "status never fails: {body}");
        assert_eq!(body["backend"], "gsd");
        assert_eq!(body["ready"], true);
        assert_eq!(body["binary_found"], true);
        assert_eq!(body["secret_present"], false);
        assert!(
            body["detail"]
                .as_str()
                .unwrap_or("")
                .contains("not a blocker"),
            "detail must say the vault is optional: {body}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Steel is the hosted/keyed backend: an unresolvable steel secret
    /// (and no self-hosted base URL) reports ready:false naming the
    /// key need - the one backend where a key genuinely is the gap.
    #[test]
    fn status_steel_absent_key_reports_key_gap() {
        let (app, dir) = test_app_with_config(Some(
            "[browser]\nbackend = \"steel\"\nsteel_api_key_secret = \"PANTHEON_SWEEP_TEST_ABSENT_STEEL_XYZ\"\n",
        ));
        let (code, body) = status_json(&app);
        assert_eq!(code, 200, "status never fails: {body}");
        assert_eq!(body["backend"], "steel");
        assert_eq!(body["ready"], false);
        assert_eq!(body["secret_present"], false);
        assert!(
            body["detail"].as_str().unwrap_or("").contains("STEEL_API_KEY"),
            "detail must name the steel key need: {body}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Camofox is keyless: status must never present an API key as
    /// its requirement - only a proxy password, if a proxy is used.
    #[test]
    fn status_camofox_is_keyless() {
        let (app, dir) = test_app_with_config(Some("[browser]\nbackend = \"camofox\"\n"));
        let (code, body) = status_json(&app);
        assert_eq!(code, 200, "status never fails: {body}");
        assert_eq!(body["backend"], "camofox");
        assert_eq!(body["ready"], true);
        assert!(body["secret_present"].is_null());
        let detail = body["detail"].as_str().unwrap_or("");
        assert!(detail.contains("keyless"), "detail must say keyless: {body}");
        assert!(
            detail.contains("no API key"),
            "detail must state no API key is needed: {body}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
