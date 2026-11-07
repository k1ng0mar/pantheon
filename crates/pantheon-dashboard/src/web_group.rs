//! web_group group: browser, link_preview, uploads, util, ui_actions in one file.

pub mod browser {
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
        std::env::split_paths(&path).any(|dir| dir.join(name).is_file())
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
    pub(crate) fn input_argv(
        action: &str,
        body: &serde_json::Value,
    ) -> Result<Vec<String>, String> {
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
            run: Box::new(move |stream| {
                serve_stream(stream, head, backend, session, fps, frame_dir)
            }),
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
                body["detail"]
                    .as_str()
                    .unwrap_or("")
                    .contains("gsd-browser"),
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
                body["detail"]
                    .as_str()
                    .unwrap_or("")
                    .contains("STEEL_API_KEY"),
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
            assert!(
                detail.contains("keyless"),
                "detail must say keyless: {body}"
            );
            assert!(
                detail.contains("no API key"),
                "detail must state no API key is needed: {body}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

pub mod link_preview {
    //! `GET /api/link-preview?url=<url>`: rich link cards for chat.
    //!
    //! The mobile app renders link cards (title, image, domain) under
    //! messages; this endpoint fetches the target page server-side and
    //! extracts Open Graph metadata so the client never has to.
    //!
    //! Security model: the dashboard fetches an arbitrary URL the client
    //! names, so every hop is SSRF-guarded - only `http`/`https`, the host
    //! must resolve to public IPs (no loopback, private, link-local,
    //! carrier-grade NAT, or unique-local ranges), and redirects are
    //! followed manually (max 3) with the guard re-applied per hop.
    //! Fetch is bounded: 5 s total timeout, ~1 MiB body cap.
    //!
    //! v1 notes: no caching (every card view re-fetches), and the DNS
    //! check is a pre-flight - a resolver that answers differently on the
    //! real connection (TOCTOU) is not defended against. Good enough for a
    //! localhost dashboard; revisit before exposing this remotely.

    use std::io::Read;
    use std::net::{IpAddr, ToSocketAddrs};
    use std::time::Duration;

    use pantheon_gateway::http::{Request, Response};
    use url::Url;

    use crate::{err_json, json_ok};

    const MAX_REDIRECTS: u32 = 3;
    const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
    const MAX_BODY_BYTES: u64 = 1024 * 1024;
    const USER_AGENT: &str =
        "Mozilla/5.0 (compatible; Pantheon/1.0; +https://github.com/k1ng0mar/pantheon)";

    /// Error carrying the HTTP status the endpoint should return.
    pub struct PreviewError {
        pub status: u16,
        pub code: &'static str,
        pub msg: String,
    }

    fn fail(status: u16, code: &'static str, msg: impl Into<String>) -> PreviewError {
        PreviewError {
            status,
            code,
            msg: msg.into(),
        }
    }

    /// True when the IP is publicly routable. Rejects loopback, private,
    /// link-local, unspecified, multicast, CGNAT (100.64.0.0/10), IPv6
    /// unique-local (fc00::/7), and the reserved IPv4 ranges
    /// 192.0.0.0/24, 198.18.0.0/15, and 240.0.0.0/4 - the ranges std's
    /// helpers don't cover are checked by hand.
    ///
    /// IPv4-mapped (`::ffff:a.b.c.d`) and IPv4-compatible (`::a.b.c.d`)
    /// IPv6 addresses route to the IPv4 host, so they are judged by the
    /// IPv4 rules: without the mapping, `::ffff:127.0.0.1` and
    /// `::ffff:192.168.1.1` sailed past the V6 branch untouched.
    ///
    /// Public so the behavioral suite in `eval/tests/link_preview_guard.rs` can
    /// pin the rejection set; this is the SSRF boundary, not an internal detail.
    pub fn is_public_ip(ip: &IpAddr) -> bool {
        if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
            return false;
        }
        match ip {
            IpAddr::V4(v4) => is_public_v4(*v4),
            IpAddr::V6(v6) => {
                if let Some(v4) = v6.to_ipv4() {
                    return is_public_v4(v4);
                }
                if v6.is_unicast_link_local() {
                    return false;
                }
                // Unique-local fc00::/7.
                let seg0 = v6.segments()[0];
                if seg0 & 0xfe00 == 0xfc00 {
                    return false;
                }
                true
            }
        }
    }

    /// IPv4 half of [`is_public_ip`], shared with the mapped-V6 path so
    /// both spellings of an address get the same verdict.
    fn is_public_v4(v4: std::net::Ipv4Addr) -> bool {
        // Loopback/unspecified/broadcast must be denied HERE, not just
        // by the outer `is_public_ip` check: an IPv4-mapped IPv6 address
        // (`::ffff:127.0.0.1`) is not loopback as V6, so it reaches this
        // function, and without these checks it would pass as public.
        if v4.is_private()
            || v4.is_loopback()
            || v4.is_link_local()
            || v4.is_broadcast()
            || v4.is_unspecified()
        {
            return false;
        }
        let o = v4.octets();
        // Carrier-grade NAT: 100.64.0.0/10.
        if o[0] == 100 && (64..128).contains(&o[1]) {
            return false;
        }
        // IETF protocol assignments: 192.0.0.0/24 (includes NAT64
        // well-known prefixes - not a public host).
        if o[0] == 192 && o[1] == 0 && o[2] == 0 {
            return false;
        }
        // Benchmarking: 198.18.0.0/15.
        if o[0] == 198 && (o[1] == 18 || o[1] == 19) {
            return false;
        }
        // Reserved: 240.0.0.0/4 (includes 255.255.255.255 broadcast).
        if o[0] >= 240 {
            return false;
        }
        // Everything else IPv4 is public (documentation ranges 192.0.2/24,
        // 198.51.100/24, 203.0.113/24 are harmless to allow: they route
        // nowhere real).
        true
    }

    /// SSRF guard: the URL's host must be http(s) with no credentials, and
    /// every resolved IP must be public. Returns the parsed URL.
    ///
    /// Public so the behavioral suite in `eval/tests/link_preview_guard.rs` can
    /// exercise the exact rejection set this boundary enforces.
    pub fn guard_url(raw: &str) -> Result<Url, PreviewError> {
        let url = Url::parse(raw)
            .map_err(|_| fail(422, "LINK_BAD_URL", "url is not a valid absolute URL"))?;
        match url.scheme() {
            "http" | "https" => {}
            _ => {
                return Err(fail(
                    422,
                    "LINK_BAD_URL",
                    "only http and https URLs are allowed",
                ));
            }
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(fail(
                422,
                "LINK_BAD_URL",
                "URLs with credentials are not allowed",
            ));
        }
        let host = url
            .host_str()
            .ok_or_else(|| fail(422, "LINK_BAD_URL", "URL has no host"))?;
        // Literal IP in the host: check it directly. `host_str()` brackets IPv6
        // literals (`"[::1]"`), which `IpAddr::parse` rejects, so unwrap the
        // brackets first - otherwise every IPv6 literal silently skips this
        // branch and falls through to the DNS path, where it is rejected only
        // because a bracketed host does not resolve. That made the guard's
        // literal check a no-op for the entire IPv6 space.
        let ip_candidate = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        if let Ok(ip) = ip_candidate.parse::<IpAddr>() {
            if !is_public_ip(&ip) {
                return Err(fail(422, "LINK_SSRF", "URL resolves to a non-public IP"));
            }
            return Ok(url);
        }
        // DNS name: resolve and check every answer.
        let port = url.port_or_known_default().unwrap_or(443);
        let addrs = (host, port)
            .to_socket_addrs()
            .map_err(|_| fail(422, "LINK_BAD_URL", "could not resolve URL host"))?;
        let mut any = false;
        for addr in addrs {
            any = true;
            if !is_public_ip(&addr.ip()) {
                return Err(fail(422, "LINK_SSRF", "URL resolves to a non-public IP"));
            }
        }
        if !any {
            return Err(fail(
                422,
                "LINK_BAD_URL",
                "URL host resolved to no addresses",
            ));
        }
        Ok(url)
    }

    /// Extracted card fields; `None` serializes as null.
    pub struct Card {
        pub title: Option<String>,
        pub description: Option<String>,
        pub image: Option<String>,
        pub site_name: Option<String>,
    }

    /// ASCII case-insensitive byte search. Returns the byte offset in the
    /// ORIGINAL string, so slicing stays valid for non-ASCII documents
    /// (`str::to_lowercase` can change byte length and would misalign
    /// indices).
    fn find_insensitive(haystack: &str, needle: &str) -> Option<usize> {
        let h = haystack.as_bytes();
        let n = needle.as_bytes();
        if n.is_empty() || n.len() > h.len() {
            return None;
        }
        h.windows(n.len()).position(|w| {
            w.iter()
                .zip(n.iter())
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
        })
    }

    fn find_insensitive_from(haystack: &str, from: usize, needle: &str) -> Option<usize> {
        find_insensitive(&haystack[from..], needle).map(|p| from + p)
    }

    /// Pull one attribute value out of a tag string. Handles
    /// `name="v"`, `name='v'`, and `name=v` forms, case-insensitively.
    /// The match must sit on an attribute boundary (start of tag or
    /// whitespace), so `data-content=` never matches a search for
    /// `content=`.
    fn attr(tag: &str, name: &str) -> Option<String> {
        let needle = format!("{name}=");
        let mut from = 0;
        let pos = loop {
            let rel = find_insensitive_from(tag, from, &needle)?;
            let boundary =
                rel == 0 || matches!(tag.as_bytes()[rel - 1], b' ' | b'\t' | b'\n' | b'\r' | b'<');
            if boundary {
                break rel;
            }
            from = rel + 1;
        };
        let rest = tag[pos + name.len() + 1..].trim_start();
        let mut chars = rest.chars();
        let first = chars.next()?;
        if first == '"' || first == '\'' {
            return rest[1..].split(first).next().map(|s| s.to_string());
        }
        Some(
            rest.split(|c: char| c.is_whitespace() || c == '>')
                .next()
                .unwrap_or("")
                .trim_end_matches('/')
                .to_string(),
        )
    }

    /// Scan `<meta ...>` tags for an og/property (or name) match and return
    /// its `content` value.
    fn meta_content(html: &str, key: &str) -> Option<String> {
        let mut pos = 0;
        while let Some(abs) = find_insensitive_from(html, pos, "<meta") {
            let end = find_insensitive_from(html, abs, ">")
                .map(|e| e + 1)
                .unwrap_or(html.len());
            let tag = &html[abs..end.min(html.len())];
            let prop = attr(tag, "property").or_else(|| attr(tag, "name"));
            if prop
                .as_deref()
                .map(|p| p.eq_ignore_ascii_case(key))
                .unwrap_or(false)
            {
                if let Some(c) = attr(tag, "content") {
                    let c = c.trim();
                    if !c.is_empty() {
                        return Some(c.to_string());
                    }
                }
            }
            pos = end;
        }
        None
    }

    fn clean(s: &str) -> Option<String> {
        let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
        let s = s.trim();
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    }

    /// Parse card fields out of an HTML document. `base` resolves relative
    /// image URLs. Pure function - unit-tested with fixture HTML.
    pub fn parse_preview_html(html: &str, base: &Url) -> Card {
        let title = meta_content(html, "og:title").or_else(|| {
            find_insensitive(html, "<title>").and_then(|s| {
                find_insensitive_from(html, s, "</title>").and_then(|e| clean(&html[s + 7..e]))
            })
        });
        let description =
            meta_content(html, "og:description").or_else(|| meta_content(html, "description"));
        let image = meta_content(html, "og:image")
            .and_then(|src| base.join(src.trim()).ok().map(|u| u.to_string()))
            .or_else(|| {
                // Fallback: first non-data, non-empty <img src>.
                let mut pos = 0;
                while let Some(abs) = find_insensitive_from(html, pos, "<img") {
                    let end = find_insensitive_from(html, abs, ">")
                        .map(|e| e + 1)
                        .unwrap_or(html.len());
                    let tag = &html[abs..end.min(html.len())];
                    if let Some(src) = attr(tag, "src") {
                        let src = src.trim();
                        if !src.is_empty() && !src.starts_with("data:") {
                            return base.join(src).ok().map(|u| u.to_string());
                        }
                    }
                    pos = end;
                }
                None
            });
        let site_name = meta_content(html, "og:site_name");
        Card {
            title: title.and_then(|t| clean(&t)),
            description: description.and_then(|d| clean(&d)),
            image,
            site_name: site_name.and_then(|s| clean(&s)),
        }
    }

    fn fetch_html(url: &Url) -> Result<(Url, String), PreviewError> {
        // Manual redirect loop so the SSRF guard runs on every hop.
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(FETCH_TIMEOUT))
            .max_redirects(0)
            .max_redirects_will_error(false)
            .user_agent(USER_AGENT)
            .http_status_as_error(false)
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .new_agent();
        let mut current = url.clone();
        for _ in 0..=MAX_REDIRECTS {
            guard_url(current.as_str())?;
            let resp = agent
                .get(current.as_str())
                .call()
                .map_err(|e| fail(502, "LINK_FETCH", format!("fetch failed: {e}")))?;
            let status = resp.status().as_u16();
            if (300..400).contains(&status) {
                let loc = resp
                    .headers()
                    .get("Location")
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| fail(502, "LINK_FETCH", "redirect without Location"))?;
                current = current
                    .join(loc)
                    .map_err(|_| fail(502, "LINK_FETCH", "bad redirect Location"))?;
                continue;
            }
            if !(200..300).contains(&status) {
                return Err(fail(
                    502,
                    "LINK_FETCH",
                    format!("target returned HTTP {status}"),
                ));
            }
            let mut body = String::new();
            resp.into_body()
                .into_reader()
                .take(MAX_BODY_BYTES + 1)
                .read_to_string(&mut body)
                .map_err(|e| fail(502, "LINK_FETCH", format!("read failed: {e}")))?;
            return Ok((current, body));
        }
        Err(fail(502, "LINK_FETCH", "too many redirects"))
    }

    /// Fetch and summarize a URL into a link card.
    pub fn fetch_preview(raw_url: &str) -> Result<serde_json::Value, PreviewError> {
        let url = guard_url(raw_url)?;
        let (final_url, html) = fetch_html(&url)?;
        let card = parse_preview_html(&html, &final_url);
        let domain = final_url.host_str().unwrap_or("").to_string();
        Ok(serde_json::json!({
            "url": final_url.to_string(),
            "title": card.title,
            "description": card.description,
            "image": card.image,
            "site_name": card.site_name,
            "domain": domain,
        }))
    }

    /// `GET /api/link-preview?url=<url>`.
    pub fn link_preview(req: &Request) -> Response {
        let raw = req.query.get("url").map(String::as_str).unwrap_or("");
        if raw.trim().is_empty() {
            return err_json(422, "LINK_BAD_URL", "missing ?url= parameter");
        }
        match fetch_preview(raw) {
            Ok(v) => json_ok(v),
            Err(e) => err_json(e.status, e.code, &e.msg),
        }
    }
}

pub mod uploads {
    //! File uploads backing chat message attachments (the mobile app's
    //! "paperclip" button).
    //!
    //! `POST /api/uploads` stores a base64 file under
    //! `<data_dir>/uploads/<id>_<sanitized-name>` plus a tiny `<id>.json`
    //! sidecar (`{name, mime, size_bytes}` - the mime is re-derived from the
    //! sidecar, never from the filename extension). `GET /api/uploads/:id`
    //! serves the raw bytes back with the stored mime as the content type,
    //! for preview/verify in the client.
    //!
    //! `POST /api/runs/:id/message` then takes an optional
    //! `"attachments": ["upl_...", ...]` array; each id is resolved here and
    //! its absolute path is appended to the `--say` text so the agent can
    //! read it with its file tools. Image attachments are passed as file
    //! paths only; they are not interpreted as pictures.

    use crate::{bad_json, body_json, created_json, err_json, App};
    use pantheon_api::message::ImagePart;
    use pantheon_gateway::http::{Request, Response};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Hard cap on decoded upload size: 25 MiB.
    pub const MAX_UPLOAD_BYTES: usize = 25 * 1024 * 1024;

    /// Cap on the number of entries inflated from an attached zip.
    pub const MAX_ZIP_ENTRIES: usize = 1000;
    /// Cap on total inflated bytes from an attached zip (zip-bomb guard,
    /// enforced on actual bytes read, never on header claims): 100 MiB.
    pub const MAX_ZIP_INFLATED_BYTES: u64 = 100 * 1024 * 1024;
    /// Max extracted entries listed inline in the message text; beyond this
    /// the listing is summarized so one giant archive can't flood context.
    pub const MAX_ZIP_LISTED: usize = 50;

    static ID_COUNTER: AtomicU32 = AtomicU32::new(0);

    /// Resolved upload: the metadata from the `<id>.json` sidecar plus the
    /// absolute path of the stored file.
    #[derive(Debug, Clone)]
    pub struct UploadInfo {
        pub id: String,
        pub name: String,
        pub mime: String,
        pub size_bytes: u64,
        pub path: PathBuf,
    }

    fn uploads_dir(app: &App) -> PathBuf {
        app.data_dir.join("uploads")
    }

    /// `upl_<epochms>_<nnnn>`, mirroring `pantheon_runtime::new_scoped_id`
    /// (which is private to that crate).
    fn new_upload_id() -> String {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let sequence = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
        let n = sequence.wrapping_add(std::process::id()) % 10_000;
        format!("upl_{ms}_{n:04}")
    }

    /// Minimal base64 decoder (standard + URL-safe alphabets, padding
    /// optional). Copied from `pantheon_providers::voice::b64decode` to avoid
    /// a new dependency for this one use.
    fn b64decode(input: &str) -> Result<Vec<u8>, String> {
        fn val(c: u8) -> Option<u8> {
            match c {
                b'A'..=b'Z' => Some(c - b'A'),
                b'a'..=b'z' => Some(c - b'a' + 26),
                b'0'..=b'9' => Some(c - b'0' + 52),
                b'+' | b'-' => Some(62),
                b'/' | b'_' => Some(63),
                _ => None,
            }
        }
        let clean: Vec<u8> = input
            .bytes()
            .filter(|&b| b != b'=' && !b.is_ascii_whitespace())
            .collect();
        if clean.len() % 4 == 1 {
            return Err("invalid base64 length".to_string());
        }
        let mut out = Vec::with_capacity(clean.len() * 3 / 4);
        for chunk in clean.chunks(4) {
            let mut n: u32 = 0;
            for (i, &c) in chunk.iter().enumerate() {
                n |= (val(c).ok_or_else(|| format!("invalid base64 char {c:?}"))? as u32)
                    << (18 - 6 * i);
            }
            out.push((n >> 16) as u8);
            if chunk.len() > 2 {
                out.push((n >> 8) as u8);
            }
            if chunk.len() > 3 {
                out.push(n as u8);
            }
        }
        Ok(out)
    }

    /// Sanitize a client-supplied filename for storage. Path traversal is
    /// rejected outright (`/`, `\`, `..`); anything outside
    /// `[A-Za-z0-9._-]` collapses to `_`. Returns `None` for empty names,
    /// names that sanitize to nothing, or dotfiles.
    fn sanitize_name(name: &str) -> Option<String> {
        if name.is_empty() {
            return None;
        }
        if name.contains('/') || name.contains('\\') || name.contains("..") {
            return None;
        }
        let safe: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        if safe.is_empty() || safe.starts_with('.') {
            return None;
        }
        Some(safe)
    }

    /// Cheap pre-decode size check: the decoded length is at most
    /// `clean_len * 3 / 4`, so reject oversized payloads without allocating
    /// the decode buffer.
    fn decoded_len_within_cap(data: &str) -> bool {
        let clean_len = data
            .bytes()
            .filter(|&b| b != b'=' && !b.is_ascii_whitespace())
            .count();
        clean_len * 3 / 4 <= MAX_UPLOAD_BYTES
    }

    /// Upload ids have a fixed shape (`upl_<digits>_<digits>`); anything
    /// else is not an upload and can never address a file on disk.
    fn valid_id_shape(id: &str) -> bool {
        let rest = match id.strip_prefix("upl_") {
            Some(r) => r,
            None => return false,
        };
        let mut parts = rest.split('_');
        match (parts.next(), parts.next(), parts.next()) {
            (Some(a), Some(b), None) => {
                !a.is_empty()
                    && !b.is_empty()
                    && a.bytes().all(|c| c.is_ascii_digit())
                    && b.bytes().all(|c| c.is_ascii_digit())
            }
            _ => false,
        }
    }

    /// Mime values go out as an HTTP content-type header, so they must be
    /// plain token-ish values - no CR/LF smuggling, and a `/` is required.
    fn valid_mime(mime: &str) -> bool {
        !mime.is_empty()
            && mime.contains('/')
            && mime
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$&^_.+-*/; =".contains(&b))
    }

    /// `POST /api/uploads`: `{"name": ..., "mime": ..., "data": "<base64>"}`.
    /// 201 `{"id", "name", "mime", "size_bytes"}`.
    pub fn create(app: &App, req: &Request) -> Response {
        let body = match body_json(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let name = match body.get("name").and_then(|v| v.as_str()) {
            Some(n) if !n.trim().is_empty() => n.trim(),
            _ => return bad_json("field \"name\" is required"),
        };
        let safe_name = match sanitize_name(name) {
            Some(n) => n,
            None => return err_json(400, "UPLOAD_BAD_NAME", "file name is not acceptable"),
        };
        let mime = body
            .get("mime")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .unwrap_or("application/octet-stream");
        if !valid_mime(mime) {
            return err_json(400, "UPLOAD_BAD_MIME", "mime value is not acceptable");
        }
        let data = match body.get("data").and_then(|v| v.as_str()) {
            Some(d) => d,
            None => return bad_json("field \"data\" is required"),
        };
        if !decoded_len_within_cap(data) {
            return err_json(
                400,
                "UPLOAD_TOO_LARGE",
                &format!(
                    "upload exceeds the {} MiB limit",
                    MAX_UPLOAD_BYTES / 1024 / 1024
                ),
            );
        }
        let bytes = match b64decode(data) {
            Ok(b) => b,
            Err(e) => return err_json(400, "UPLOAD_BAD_DATA", &format!("invalid base64: {e}")),
        };
        if bytes.len() > MAX_UPLOAD_BYTES {
            return err_json(
                400,
                "UPLOAD_TOO_LARGE",
                &format!(
                    "upload exceeds the {} MiB limit",
                    MAX_UPLOAD_BYTES / 1024 / 1024
                ),
            );
        }
        let dir = uploads_dir(app);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return err_json(500, "UPLOAD", &format!("create uploads dir: {e}"));
        }
        let id = new_upload_id();
        let file_name = format!("{id}_{safe_name}");
        if let Err(e) = std::fs::write(dir.join(&file_name), &bytes) {
            return err_json(500, "UPLOAD", &format!("write file: {e}"));
        }
        let sidecar = serde_json::json!({
            "name": safe_name,
            "mime": mime,
            "size_bytes": bytes.len(),
        });
        if let Err(e) = std::fs::write(
            dir.join(format!("{id}.json")),
            serde_json::to_string(&sidecar).unwrap_or_default(),
        ) {
            let _ = std::fs::remove_file(dir.join(&file_name));
            return err_json(500, "UPLOAD", &format!("write sidecar: {e}"));
        }
        created_json(serde_json::json!({
            "id": id,
            "name": safe_name,
            "mime": mime,
            "size_bytes": bytes.len(),
        }))
    }

    /// Resolve an upload id to its metadata + absolute path. Returns `None`
    /// for malformed ids, missing sidecars, or missing data files.
    pub fn resolve(app: &App, id: &str) -> Option<UploadInfo> {
        resolve_in(&uploads_dir(app), id)
    }

    /// Same as [`resolve`], but against an explicit uploads directory instead
    /// of an `App` - for callers (like the turn child) that have a data dir
    /// but no dashboard `App`.
    pub fn resolve_in(dir: &Path, id: &str) -> Option<UploadInfo> {
        if !valid_id_shape(id) {
            return None;
        }
        let sidecar_bytes = std::fs::read(dir.join(format!("{id}.json"))).ok()?;
        let sidecar: serde_json::Value = serde_json::from_slice(&sidecar_bytes).ok()?;
        let name = sidecar.get("name")?.as_str()?.to_string();
        let mime = sidecar.get("mime")?.as_str()?.to_string();
        let size_bytes = sidecar.get("size_bytes")?.as_u64()?;
        let prefix = format!("{id}_");
        let data_file = std::fs::read_dir(dir)
            .ok()?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .filter_map(|n| n.into_string().ok())
            .find(|n| n.starts_with(&prefix))?;
        let path = std::fs::canonicalize(dir.join(&data_file)).ok()?;
        Some(UploadInfo {
            id: id.to_string(),
            name,
            mime,
            size_bytes,
            path,
        })
    }

    /// True when `bytes` begins with a zip signature (local file header,
    /// empty archive, or spanned archive). Zip detection sniffs magic bytes
    /// the stored mime is client-supplied and never trusted.
    pub fn is_zip_bytes(bytes: &[u8]) -> bool {
        bytes.len() >= 4
            && (bytes.starts_with(b"PK\x03\x04")
                || bytes.starts_with(b"PK\x05\x06")
                || bytes.starts_with(b"PK\x07\x08"))
    }

    /// True when the stored upload file looks like a zip archive (first four
    /// bytes only - the whole file is never read for detection).
    pub fn is_zip_upload(info: &UploadInfo) -> bool {
        use std::io::Read;
        let mut head = [0u8; 4];
        match std::fs::File::open(&info.path).and_then(|mut f| f.read(&mut head)) {
            Ok(4) => is_zip_bytes(&head),
            _ => false,
        }
    }

    /// Directory an attached zip is inflated into:
    /// `<uploads_dir>/<id>_unzipped/`.
    pub fn zip_extract_dir(app: &App, id: &str) -> PathBuf {
        uploads_dir(app).join(format!("{id}_unzipped"))
    }

    /// One inflated zip entry: path relative to the extraction root.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    pub struct ExtractedEntry {
        pub rel_path: PathBuf,
        pub size_bytes: u64,
    }

    /// Inflate an attached zip upload into [`zip_extract_dir`], returning the
    /// extracted entries. Idempotent: a completed extraction is recorded in
    /// `.extracted.json` and reused on repeat attaches.
    ///
    /// Fail-closed on zip-slip paths (absolute or containing `..`), more
    /// than [`MAX_ZIP_ENTRIES`] entries, and total inflated bytes past
    /// [`MAX_ZIP_INFLATED_BYTES`] (enforced on actual bytes read, never on
    /// header claims). Directory and symlink entries are skipped, and every
    /// entry is written as a regular file, so a symlink entry can never
    /// materialize as a link on disk.
    pub fn extract_zip_upload(app: &App, info: &UploadInfo) -> Result<Vec<ExtractedEntry>, String> {
        let dest_root = zip_extract_dir(app, &info.id);
        let marker = dest_root.join(".extracted.json");
        if marker.is_file() {
            if let Ok(entries) = serde_json::from_str::<Vec<ExtractedEntry>>(
                &std::fs::read_to_string(&marker).unwrap_or_default(),
            ) {
                return Ok(entries);
            }
            // Corrupt marker: fall through and re-extract.
            let _ = std::fs::remove_dir_all(&dest_root);
        }
        let bytes =
            std::fs::read(&info.path).map_err(|e| format!("read upload \"{}\": {e}", info.name))?;
        if !is_zip_bytes(&bytes) {
            return Err(format!("\"{}\" is not a zip archive", info.name));
        }
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))
            .map_err(|e| format!("open zip: {e}"))?;
        if zip.len() > MAX_ZIP_ENTRIES {
            return Err(format!(
                "zip has {} entries (max {MAX_ZIP_ENTRIES})",
                zip.len()
            ));
        }
        // Extract into a temp dir first, then rename into place, so a failed
        // or interrupted extraction never leaves a half-written tree behind.
        let tmp_root = uploads_dir(app).join(format!("{}_unzipped.tmp", info.id));
        let _ = std::fs::remove_dir_all(&tmp_root);
        std::fs::create_dir_all(&tmp_root).map_err(|e| format!("create extract dir: {e}"))?;
        let mut total: u64 = 0;
        let mut entries: Vec<ExtractedEntry> = Vec::new();
        let result: Result<(), String> = (|| {
            use std::io::Read as _;
            for i in 0..zip.len() {
                let mut f = zip.by_index(i).map_err(|e| format!("entry {i}: {e}"))?;
                let name = f
                    .name()
                    .map_err(|e| format!("entry {i} name: {e}"))?
                    .into_owned();
                if f.is_dir() || f.is_symlink() {
                    continue;
                }
                let rel = PathBuf::from(&name);
                if rel.is_absolute()
                    || rel
                        .components()
                        .any(|c| matches!(c, std::path::Component::ParentDir))
                {
                    return Err(format!("unsafe entry path: {name}"));
                }
                let remaining = MAX_ZIP_INFLATED_BYTES.saturating_sub(total);
                let mut buf = Vec::new();
                let n = std::io::Read::take(&mut f, remaining + 1)
                    .read_to_end(&mut buf)
                    .map_err(|e| format!("read {name}: {e}"))?;
                total = total.saturating_add(n as u64);
                if total > MAX_ZIP_INFLATED_BYTES {
                    return Err(format!(
                        "zip inflates past {} MiB cap",
                        MAX_ZIP_INFLATED_BYTES / (1024 * 1024)
                    ));
                }
                let dest = tmp_root.join(&rel);
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent).map_err(|e| format!("create dir: {e}"))?;
                }
                std::fs::write(&dest, &buf).map_err(|e| format!("write {name}: {e}"))?;
                entries.push(ExtractedEntry {
                    rel_path: rel,
                    size_bytes: buf.len() as u64,
                });
            }
            Ok(())
        })();
        if let Err(e) = result {
            let _ = std::fs::remove_dir_all(&tmp_root);
            return Err(e);
        }
        let _ = std::fs::remove_dir_all(&dest_root);
        std::fs::rename(&tmp_root, &dest_root).map_err(|e| format!("commit extraction: {e}"))?;
        let listing = serde_json::to_string(&entries).unwrap_or_else(|_| "[]".to_string());
        std::fs::write(&marker, listing).map_err(|e| format!("write marker: {e}"))?;
        Ok(entries)
    }

    /// Turn a resolved upload into a vision [`ImagePart`]: reads the stored
    /// bytes, sniffs the mime from magic bytes (the stored mime is
    /// client-supplied and never trusted), enforces the size cap, and
    /// base64-encodes. This is the bridge from "file on disk" to "image part
    /// on the outgoing provider request". Loud `Err` on anything that is not
    /// actually a sendable image.
    pub fn image_part(info: &UploadInfo) -> Result<ImagePart, String> {
        let bytes = std::fs::read(&info.path)
            .map_err(|e| format!("cannot read stored upload \"{}\": {e}", info.name))?;
        ImagePart::from_bytes(&info.name, &bytes).map_err(|e| format!("\"{}\": {e}", info.name))
    }

    /// `GET /api/uploads/:id`: raw bytes with the stored mime as the
    /// content type. 404 `UPLOAD_NOT_FOUND` for unknown ids.
    pub fn download(app: &App, id: &str) -> Response {
        let info = match resolve(app, id) {
            Some(i) => i,
            None => {
                return err_json(
                    404,
                    "UPLOAD_NOT_FOUND",
                    &format!("unknown upload id \"{id}\""),
                )
            }
        };
        let bytes = match std::fs::read(&info.path) {
            Ok(b) => b,
            Err(_) => {
                return err_json(
                    404,
                    "UPLOAD_NOT_FOUND",
                    &format!("unknown upload id \"{id}\""),
                )
            }
        };
        // `Response` needs a `&'static str` content type; the mime was
        // validated at upload time and re-checked here, so leaking the tiny
        // string is safe and header-injection-proof. Served as an attachment
        // (never inline): a stored HTML/SVG must not execute in the
        // dashboard's origin.
        let content_type: &'static str = if valid_mime(&info.mime) {
            Box::leak(info.mime.into_boxed_str())
        } else {
            "application/octet-stream"
        };
        let filename = sanitize_name(&info.name).unwrap_or_else(|| "download".to_string());
        Response::download(&filename, content_type, bytes)
    }

    /// Human-readable byte size: `B`, one-decimal `KB`, one-decimal `MB`.
    pub fn human_size(bytes: u64) -> String {
        const KB: f64 = 1024.0;
        const MB: f64 = 1024.0 * 1024.0;
        let b = bytes as f64;
        if b < KB {
            format!("{bytes} B")
        } else if b < MB {
            format!("{:.1} KB", b / KB)
        } else {
            format!("{:.1} MB", b / MB)
        }
    }
}

pub mod util {
    //! Shared small helpers. `now_ms`, `project_root`, `atomic_write`, and
    //! `parent_of` were each copied byte-identical into several route
    //! modules; they live here once.

    use std::path::{Path, PathBuf};

    /// Wall-clock milliseconds since the Unix epoch. Never panics: a clock
    /// before the epoch (or an unreadable one) reads as 0.
    ///
    /// Thin re-export of the canonical `pantheon_api::logging::now_ms` rather
    /// than a ninth copy of the same five lines.
    pub(crate) use pantheon_api::logging::now_ms;

    /// The project working directory: the process's current dir, or `.` when
    /// it cannot be read.
    pub(crate) fn project_root() -> PathBuf {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }

    /// Atomic config write: unique temp file in the same directory +
    /// fsync + rename, so a crash mid-write never leaves a half-written
    /// file, and the existing file's permissions survive the rename.
    pub(crate) fn atomic_write(path: &Path, text: &str) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        // Unique temp name (pid + nanos): the old fixed `*.toml.tmp` name
        // let concurrent writers clobber each other's temp file. Mode:
        // preserve the existing file's mode instead of loosening it to the
        // umask default on rename (a 0600 config.toml used to come back
        // 0644); a brand-new file keeps the umask default, as before.
        // fsync file + dir so the rename survives a crash.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp = path.with_file_name(format!(
            "{}.{}-{}.tmp",
            path.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "config".to_string()),
            std::process::id(),
            nanos
        ));
        #[cfg(unix)]
        let existing_mode = std::fs::metadata(path).ok().map(|m| {
            use std::os::unix::fs::MetadataExt;
            m.mode() & 0o777
        });
        let write = (|| -> Result<(), String> {
            std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
            #[cfg(unix)]
            if let Some(mode) = existing_mode {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
                    .map_err(|e| e.to_string())?;
            }
            std::fs::File::open(&tmp)
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string())?;
            std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
            if let Some(parent) = path.parent() {
                if let Ok(dir) = std::fs::File::open(parent) {
                    let _ = dir.sync_all();
                }
            }
            Ok(())
        })();
        if write.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        write
    }

    /// Navigate (and create) intermediate tables for a dotted path. Returns
    /// the parent table and the leaf key.
    pub(crate) fn parent_of<'a>(
        doc: &'a mut toml::Value,
        path: &str,
    ) -> Result<(&'a mut toml::map::Map<String, toml::Value>, String), String> {
        let mut parts: Vec<&str> = path.split('.').collect();
        let leaf = parts.pop().ok_or_else(|| "empty path".to_string())?;
        if leaf.is_empty() {
            return Err("empty path segment".to_string());
        }
        let mut cur = doc;
        for part in parts {
            if part.is_empty() {
                return Err("empty path segment".to_string());
            }
            cur = cur
                .as_table_mut()
                .ok_or_else(|| format!("'{path}' walks through a non-table"))?
                .entry(part.to_string())
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
            if !cur.is_table() {
                return Err(format!("'{path}' walks through a non-table value"));
            }
        }
        let table = cur
            .as_table_mut()
            .ok_or_else(|| format!("'{path}' has no parent table"))?;
        Ok((table, leaf.to_string()))
    }

    /// Round a USD cost to 4 decimal places for JSON output. Four, not two:
    /// single model calls routinely cost fractions of a cent, and a frontend
    /// `toFixed(2)` display formats identically either way.
    pub(crate) fn round_cost_usd(v: f64) -> f64 {
        (v * 10000.0).round() / 10000.0
    }

    /// Default agent/namespace name when none is configured.
    pub(crate) const DEFAULT_AGENT_NAME: &str = "nyx";
}

pub mod ui_actions {
    //! Dashboard UI-action submission: `POST /api/runs/:id/ui-action`.
    //!
    //! This is the dashboard's interactive client for the agent-native UI
    //! protocol (`pantheon_api::ui`, see `docs/ui-protocol.md`). Forms,
    //! buttons, and approval cards rendered by `assets/app.js` submit
    //! through this route.
    //!
    //! Security posture (deliberate):
    //! - The route lives under `/api/*`, so the gateway's `check_auth`
    //!   enforces the dashboard token before this handler runs. There is
    //!   no unauthenticated path.
    //! - The run must exist in THIS dashboard's ledger (`known_run`): a
    //!   caller cannot reach another user's session by guessing a run id.
    //! - The handler does NO validation of its own beyond JSON shape. Every
    //!   security decision — action registry, revision pin, nonce replay,
    //!   capability gate, secret redaction — lives in
    //!   `Session::submit_ui_action`, which this handler delegates to
    //!   verbatim. The dashboard never constructs tool calls.
    //! - Client params are never echoed back in responses (secret safety).
    //! - Approval decisions resume the parked turn exactly like
    //!   `runs::answer_input` does: under the run's send guard, spawning a
    //!   turn child with `--resume`.

    use crate::session_factory::open_maintenance_session;
    use crate::{bad_json, conflict, err_json, json_ok, spawn_turn_child, App};
    use pantheon_api::ui::UiActionSubmission;
    use pantheon_gateway::http::{Request, Response};
    use pantheon_runtime::session::UiActionOutcome;

    /// `POST /api/runs/:id/ui-action`: submit a UI action from a dashboard-
    /// rendered document. Body: `{ui_id, revision, node_id, action_id,
    /// params, nonce}`.
    pub fn submit_ui_action(app: &App, run_id: &str, req: &Request) -> Response {
        let body = match crate::body_json(req) {
            Ok(b) => b,
            Err(r) => return r,
        };
        let get_str = |k: &str| {
            body.get(k)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .unwrap_or("")
                .to_string()
        };
        let ui_id = get_str("ui_id");
        let node_id = get_str("node_id");
        let action_id = get_str("action_id");
        let nonce = get_str("nonce");
        let revision = body.get("revision").and_then(|v| v.as_u64());
        let params = body
            .get("params")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();
        if ui_id.is_empty() || node_id.is_empty() || action_id.is_empty() || nonce.is_empty() {
            return bad_json(
                "fields \"ui_id\", \"node_id\", \"action_id\" and \"nonce\" are required and must be non-empty",
            );
        }
        let Some(revision) = revision else {
            return bad_json("field \"revision\" is required and must be a non-negative integer");
        };
        if let Err(r) = crate::runs::known_run(app, run_id) {
            return r;
        }
        let session = match open_maintenance_session(&app.data_dir) {
            Ok(s) => s,
            Err(e) => return err_json(500, "SESSION", &format!("open session: {e}")),
        };
        let sub = UiActionSubmission {
            ui_id,
            revision,
            node_id,
            action_id,
            params,
            nonce,
        };
        // Serialize with other dashboard submissions for this run. The
        // runtime's per-Session ui_action_lock does not span the fresh
        // Session opened per request above; without this, two concurrent
        // POSTs could both pass the nonce replay check and both execute.
        let ui_lock = app.ui_action_guard(run_id);
        let _ui_guard = ui_lock.lock().unwrap_or_else(|e| e.into_inner());
        match session.submit_ui_action(run_id, &sub) {
            Ok(UiActionOutcome::Executed { result }) => json_ok(serde_json::json!({
                "ok": true,
                "outcome": "executed",
                "result": result,
            })),
            Ok(UiActionOutcome::Decided { approved }) => {
                // The parked turn resumes exactly like an answered input:
                // serialize with other turn-starting requests, then spawn
                // the resume child. The decision itself is already settled
                // in the ledger by submit_ui_action.
                let send_lock = app.send_guard(run_id);
                let _send_guard = send_lock.lock().unwrap_or_else(|e| e.into_inner());
                let mode = session
                    .supervisor
                    .ledger_run_mode(run_id)
                    .unwrap_or_else(|_| crate::runs::DEFAULT_RUN_MODE.to_string());
                drop(session);
                match spawn_turn_child(&[
                    "run",
                    "--taskID",
                    run_id,
                    "--resume",
                    "--deliver",
                    "session",
                    "--mode",
                    mode.as_str(),
                ]) {
                    Ok(pid) => app.register_turn_child(run_id, pid),
                    Err(e) => {
                        return err_json(500, "SPAWN", &format!("failed to resume turn: {e}"));
                    }
                }
                json_ok(serde_json::json!({
                    "ok": true,
                    "outcome": "decided",
                    "approved": approved,
                }))
            }
            Err(e) => match e.code.as_str() {
                // Client-timing problems: the document moved under the
                // submission. The frontend should re-fetch and re-render.
                "UI_STALE" | "UI_REPLAY" => conflict(e.code.as_str(), &format!("{e}")),
                // Unknown document/action or malformed submission.
                "UI_UNKNOWN" | "UI_BAD_ACTION" => conflict(e.code.as_str(), &format!("{e}")),
                // The action was valid but the tool refused: surfaced so the
                // UI can show the actionable denial (e.g. "use an approval
                // card"). Not a server error.
                "UI_TOOL_DENIED" => err_json(422, e.code.as_str(), &format!("{e}")),
                _ => err_json(500, "RUNTIME", &format!("submit UI action: {e}")),
            },
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use pantheon_api::events::Event;
        use pantheon_api::ui::{
            ActionKind, ActionRef, ApprovalCard, ApprovalDecision, Component, FieldType, Form,
            FormField, UiDocument, UiNode, UI_PROTOCOL_VERSION,
        };
        use pantheon_gateway::http::Request;
        use pantheon_runtime::Supervisor;
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);

        fn test_app() -> (App, std::path::PathBuf) {
            let dir = std::env::temp_dir().join(format!(
                "pantheon-dash-ui-test-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("temp data dir");
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

        fn post_req(body: serde_json::Value) -> Request {
            Request {
                method: "POST".to_string(),
                path: "/api/runs/r1/ui-action".to_string(),
                query: HashMap::new(),
                headers: HashMap::new(),
                body: serde_json::to_vec(&body).unwrap(),
            }
        }

        fn resp_json(resp: &Response) -> serde_json::Value {
            match resp {
                Response::Buffered { body, .. } => {
                    serde_json::from_slice(body).expect("response is json")
                }
                _ => panic!("expected buffered response, got other response type"),
            }
        }

        fn resp_status(resp: &Response) -> u16 {
            match resp {
                Response::Buffered { status, .. } => *status,
                _ => panic!("expected buffered response, got other response type"),
            }
        }

        fn form_doc() -> UiDocument {
            UiDocument {
                version: UI_PROTOCOL_VERSION,
                id: "turn1-ui_0".to_string(),
                root: UiNode {
                    id: "root".to_string(),
                    component: Component::Form(Form {
                        title: Some("Deploy".to_string()),
                        description: None,
                        fields: vec![FormField {
                            name: "target".to_string(),
                            label: "Target".to_string(),
                            field_type: FieldType::Select {
                                options: vec!["staging".to_string(), "prod".to_string()],
                            },
                            required: true,
                            default: None,
                            placeholder: None,
                        }],
                        submit: ActionRef {
                            action_id: "deploy".to_string(),
                            label: "Deploy".to_string(),
                            kind: ActionKind::Tool {
                                tool: "list_dir".to_string(),
                                fixed_params: serde_json::json!({"path": "."})
                                    .as_object()
                                    .unwrap()
                                    .clone(),
                            },
                        },
                    }),
                },
            }
        }

        fn approval_doc() -> UiDocument {
            UiDocument {
                version: UI_PROTOCOL_VERSION,
                id: "turn1-ui_0".to_string(),
                root: UiNode {
                    id: "root".to_string(),
                    component: Component::Approval(ApprovalCard {
                        title: "Delete?".to_string(),
                        detail: "Delete 3 files.".to_string(),
                        approve: ActionRef {
                            action_id: "yes".to_string(),
                            label: "Delete".to_string(),
                            kind: ActionKind::Approval {
                                decision: ApprovalDecision::Grant,
                            },
                        },
                        deny: ActionRef {
                            action_id: "no".to_string(),
                            label: "Keep".to_string(),
                            kind: ActionKind::Approval {
                                decision: ApprovalDecision::Deny,
                            },
                        },
                        scope_hint: "delete 3 files".to_string(),
                    }),
                },
            }
        }

        /// Seed a run with a published UI document at revision 0.
        fn seed_run(app: &App, run_id: &str, doc: UiDocument) {
            let sup = Supervisor::open(app.data_dir.clone()).expect("supervisor");
            sup.emit(Event::RunStarted {
                run_id: run_id.to_string(),
            })
            .expect("run started");
            sup.emit(Event::UiContent {
                run_id: run_id.to_string(),
                turn_id: "turn1".to_string(),
                ui_id: doc.id.clone(),
                document: doc,
            })
            .expect("ui content");
        }

        fn valid_body(ui_id: &str) -> serde_json::Value {
            serde_json::json!({
                "ui_id": ui_id,
                "revision": 0,
                "node_id": "root",
                "action_id": "deploy",
                "params": {"target": "staging"},
                "nonce": "dash-n1",
            })
        }

        #[test]
        fn valid_form_action_executes() {
            let (app, _dir) = test_app();
            seed_run(&app, "r1", form_doc());
            let resp = submit_ui_action(&app, "r1", &post_req(valid_body("turn1-ui_0")));
            assert_eq!(resp_status(&resp), 200, "{:?}", resp_json(&resp));
            let j = resp_json(&resp);
            assert_eq!(j["ok"], true);
            assert_eq!(j["outcome"], "executed");
            assert!(!j["result"].as_str().unwrap_or("").is_empty());
        }

        #[test]
        fn unknown_run_is_404() {
            let (app, _dir) = test_app();
            // No run seeded: session isolation — a guessed run id 404s.
            let resp = submit_ui_action(&app, "nope", &post_req(valid_body("turn1-ui_0")));
            assert_eq!(resp_status(&resp), 404);
        }

        #[test]
        fn unknown_action_is_rejected() {
            let (app, _dir) = test_app();
            seed_run(&app, "r1", form_doc());
            let mut body = valid_body("turn1-ui_0");
            body["action_id"] = serde_json::json!("bogus");
            let resp = submit_ui_action(&app, "r1", &post_req(body));
            assert_eq!(resp_status(&resp), 409);
            assert_eq!(resp_json(&resp)["error"]["code"], "UI_BAD_ACTION");
        }

        #[test]
        fn invalid_params_are_rejected() {
            let (app, _dir) = test_app();
            seed_run(&app, "r1", form_doc());
            let mut body = valid_body("turn1-ui_0");
            body["params"] = serde_json::json!({"target": "mars"});
            let resp = submit_ui_action(&app, "r1", &post_req(body));
            assert_eq!(resp_status(&resp), 409);
            assert_eq!(resp_json(&resp)["error"]["code"], "UI_BAD_ACTION");
        }

        #[test]
        fn stale_revision_fails_closed() {
            let (app, _dir) = test_app();
            seed_run(&app, "r1", form_doc());
            // Publish revision 1.
            let sup = Supervisor::open(app.data_dir.clone()).expect("sup");
            let mut doc2 = form_doc();
            if let Component::Form(f) = &mut doc2.root.component {
                f.title = Some("Deploy v2".to_string());
            }
            sup.emit(Event::UiUpdated {
                run_id: "r1".to_string(),
                ui_id: "turn1-ui_0".to_string(),
                revision: 1,
                document: doc2,
            })
            .expect("revise");
            // Submit against rev 0: must fail closed, not execute.
            let resp = submit_ui_action(&app, "r1", &post_req(valid_body("turn1-ui_0")));
            assert_eq!(resp_status(&resp), 409);
            assert_eq!(resp_json(&resp)["error"]["code"], "UI_STALE");
        }

        #[test]
        fn replay_is_rejected() {
            let (app, _dir) = test_app();
            seed_run(&app, "r1", form_doc());
            let body = valid_body("turn1-ui_0");
            let r1 = submit_ui_action(&app, "r1", &post_req(body.clone()));
            assert_eq!(resp_status(&r1), 200);
            let r2 = submit_ui_action(&app, "r1", &post_req(body));
            assert_eq!(resp_status(&r2), 409);
            assert_eq!(resp_json(&r2)["error"]["code"], "UI_REPLAY");
        }

        #[test]
        fn concurrent_duplicate_submissions_execute_once() {
            use std::sync::Arc;
            let (app, _dir) = test_app();
            seed_run(&app, "r1", form_doc());
            let app = Arc::new(app);
            let mut handles = vec![];
            for _ in 0..8 {
                let app = Arc::clone(&app);
                handles.push(std::thread::spawn(move || {
                    let resp = submit_ui_action(&app, "r1", &post_req(valid_body("turn1-ui_0")));
                    resp_status(&resp)
                }));
            }
            let mut ok = 0;
            let mut conflicted = 0;
            for h in handles {
                match h.join().expect("thread") {
                    200 => ok += 1,
                    409 => conflicted += 1,
                    s => panic!("unexpected status {s}, expected 200 or 409"),
                }
            }
            assert_eq!(ok, 1, "exactly one executes");
            assert_eq!(conflicted, 7, "the rest are replays");
        }

        #[test]
        fn approval_decision_parks_and_resumes() {
            let (app, _dir) = test_app();
            seed_run(&app, "r1", approval_doc());
            // Park the turn like publish_ui_batch would.
            let sup = Supervisor::open(app.data_dir.clone()).expect("sup");
            sup.emit(Event::UiActionRequested {
                run_id: "r1".to_string(),
                ui_id: "turn1-ui_0".to_string(),
                call_id: "call-dash-1".to_string(),
            })
            .expect("request");
            let body = serde_json::json!({
                "ui_id": "turn1-ui_0",
                "revision": 0,
                "node_id": "root",
                "action_id": "yes",
                "params": {},
                "nonce": "dash-n2",
            });
            let resp = submit_ui_action(&app, "r1", &post_req(body));
            assert_eq!(resp_status(&resp), 200, "{:?}", resp_json(&resp));
            let j = resp_json(&resp);
            assert_eq!(j["outcome"], "decided");
            assert_eq!(j["approved"], true);
            // The parked call is settled exactly once.
            let entries = sup.replay("r1").expect("replay");
            let completions = entries
                .iter()
                .filter(|e| {
                    matches!(&e.event, Event::ToolCompleted { call_id, .. } if call_id == "call-dash-1")
                })
                .count();
            assert_eq!(completions, 1);
        }

        #[test]
        fn denied_tool_is_surfaced_not_executed() {
            let (app, _dir) = test_app();
            // Bind to a tool absent from the registry.
            let mut doc = form_doc();
            if let Component::Form(f) = &mut doc.root.component {
                f.submit.kind = ActionKind::Tool {
                    tool: "no_such_tool_xyz".to_string(),
                    fixed_params: Default::default(),
                };
            }
            seed_run(&app, "r1", doc);
            let resp = submit_ui_action(&app, "r1", &post_req(valid_body("turn1-ui_0")));
            assert_eq!(resp_status(&resp), 422);
            let j = resp_json(&resp);
            assert_eq!(j["error"]["code"], "UI_TOOL_DENIED");
        }

        #[test]
        fn malformed_body_is_rejected() {
            let (app, _dir) = test_app();
            seed_run(&app, "r1", form_doc());
            // Missing nonce.
            let body = serde_json::json!({
                "ui_id": "turn1-ui_0",
                "revision": 0,
                "node_id": "root",
                "action_id": "deploy",
                "params": {},
            });
            let resp = submit_ui_action(&app, "r1", &post_req(body));
            assert_eq!(resp_status(&resp), 400);
            // Not JSON at all.
            let bad = Request {
                method: "POST".to_string(),
                path: "/api/runs/r1/ui-action".to_string(),
                query: HashMap::new(),
                headers: HashMap::new(),
                body: b"not json".to_vec(),
            };
            let resp = submit_ui_action(&app, "r1", &bad);
            assert_eq!(resp_status(&resp), 400);
        }

        #[test]
        fn secret_params_are_not_echoed() {
            let (app, _dir) = test_app();
            let mut doc = form_doc();
            if let Component::Form(f) = &mut doc.root.component {
                f.fields = vec![FormField {
                    name: "token".to_string(),
                    label: "Token".to_string(),
                    field_type: FieldType::Secret,
                    required: true,
                    default: None,
                    placeholder: None,
                }];
            }
            seed_run(&app, "r1", doc);
            let body = serde_json::json!({
                "ui_id": "turn1-ui_0",
                "revision": 0,
                "node_id": "root",
                "action_id": "deploy",
                "params": {"token": "super-secret-value"},
                "nonce": "dash-n3",
            });
            let resp = submit_ui_action(&app, "r1", &post_req(body));
            let raw = match &resp {
                Response::Buffered { body, .. } => String::from_utf8(body.clone()).unwrap(),
                _ => panic!("expected buffered response"),
            };
            assert!(
                !raw.contains("super-secret-value"),
                "secret must not appear in the response: {raw}"
            );
        }
    }
}
