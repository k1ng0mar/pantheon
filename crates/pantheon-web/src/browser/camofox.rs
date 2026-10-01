//! Camoufox backend: the patched-Firefox anti-detect browser, driven
//! through a long-lived Python shim over JSON-over-stdio.
//!
//! Camoufox (upstream spelling; the `camofox` backend id is Pantheon's
//! stable config name) is driven by its official Python launcher API,
//! which speaks the patched **Juggler** protocol — there is no CDP
//! transport, so the raw-`chromiumoxide` backends cannot talk to it.
//! Instead this backend spawns `python3` running the embedded shim
//! ([`SHIM_SOURCE`], materialized to a temp file on first use), holds
//! one shim process per Pantheon session, and exchanges one JSON
//! command object per stdin line / one JSON response per stdout line.
//!
//! The shim speaks Pantheon's canonical command vocabulary (the same
//! `navigate`/`snapshot`/`click-ref`/… argv the gsd-browser and
//! playwright-cli backends speak), so the tool layer needs no new
//! surface. `wait-for` is implemented Rust-side by polling a JS probe
//! through the shim's `eval` (sharing [`wait_plan`](super::cdp::wait_plan)
//! with the Playwright backend); `act`/`act-instruction` are
//! unsupported (no semantic-intent engine in this loop — same as
//! Playwright).
//!
//! ## Graceful degradation
//!
//! Construction is lazy (no subprocess until first `invoke`). When the
//! `camoufox` Python package — or its fetched browser binary
//! (`python -m camoufox fetch`) — is absent, the shim's handshake
//! reports `{"ready": false, "kind": "missing"}` and the backend
//! returns [`BrowserError::BinaryMissing`] carrying
//! [`CAMOFOX_INSTALL_INSTRUCTIONS`]. It never crashes and never fails
//! at registration: the backend builds keyless and reports unavailable
//! on first use, like the other optional backends.
//!
//! ## Fingerprint config (`[browser.camofox]`)
//!
//! [`CamofoxConfig`] maps 1:1 onto the launcher options: `os`
//! (`windows`|`macos`|`linux`), `humanize_secs`, `geoip`, `locale`,
//! `timezone`, `proxy_*`, `block_images`, `block_webrtc`,
//! `fingerprint_preset`. Unset properties are auto-filled from
//! BrowserForge fingerprints by the launcher itself — prefer leaving
//! them unset over inventing inconsistent values.

use super::backend::BrowserBackend;
use super::cdp::{build_extract_arrow, wait_plan, WaitPlan};
use super::error::BrowserError;
use super::proc::{snip, spawn_error};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Install instructions surfaced when the `camoufox` package or its
/// browser binary is missing. Never crash on a missing optional
/// dependency — tell the user how to get it.
pub const CAMOFOX_INSTALL_INSTRUCTIONS: &str =
    "the Camoufox browser backend needs the `camoufox` Python package and its \
fetched browser binary. Install with `pip install \"camoufox[geoip]\"` then \
run `python -m camoufox fetch` (one-time ~150MB download). See \
https://github.com/daijro/camoufox.";

/// Embedded Python shim source (see `shims/camofox_shim.py`).
const SHIM_SOURCE: &str = include_str!("shims/camofox_shim.py");

/// Poll interval for `wait-for` probes through the shim's `eval`.
const WAIT_POLL: Duration = Duration::from_millis(500);

/// Grace period for the shim's `close` handshake during `stop_daemon`.
const CLOSE_GRACE: Duration = Duration::from_secs(5);

/// Camoufox backend configuration. These are the `[browser.camofox]`
/// keys; secrets (proxy password) arrive as resolved values and are
/// never logged.
#[derive(Debug, Clone)]
pub struct CamofoxConfig {
    /// Python interpreter for the shim. `None` = `python3` on PATH.
    pub python: Option<PathBuf>,
    /// Explicit shim script path (debugging). `None` = the embedded
    /// shim, materialized to a temp file on first spawn.
    pub shim_path: Option<PathBuf>,
    /// Launch headless. Default true. (`headless="virtual"` when
    /// [`CamofoxConfig::headless_virtual`] is set — needs `xvfb`.)
    pub headless: bool,
    /// Use the Xvfb "virtual" headless mode instead of true headless.
    pub headless_virtual: bool,
    /// Fingerprint OS: `windows`, `macos`, or `linux`. `None` = the
    /// launcher's BrowserForge default.
    pub os: Option<String>,
    /// Human-like cursor movement, in seconds. `None` = off.
    pub humanize_secs: Option<f64>,
    /// Derive timezone/locale/geolocation from the proxy IP (needs the
    /// `[geoip]` install extra).
    pub geoip: bool,
    /// Explicit locale override, e.g. `"en-US"`.
    pub locale: Option<String>,
    /// Explicit timezone override, e.g. `"America/New_York"`.
    pub timezone: Option<String>,
    /// Proxy server URL, e.g. `"http://proxy:8080"`.
    pub proxy_server: Option<String>,
    /// Proxy username (optional).
    pub proxy_username: Option<String>,
    /// Proxy password (secret — never logged).
    pub proxy_password: Option<String>,
    /// Block image loads (perf).
    pub block_images: bool,
    /// Block WebRTC (IP-leak prevention).
    pub block_webrtc: bool,
    /// Use BrowserForge-backed real fingerprint presets. Default true
    /// (upstream recommendation).
    pub fingerprint_preset: bool,
    /// Per-command timeout, in seconds.
    pub timeout_secs: u64,
    /// Seconds to wait for the browser to launch on session start.
    /// First launch after a fetch can take minutes.
    pub startup_timeout_secs: u64,
}

impl Default for CamofoxConfig {
    fn default() -> Self {
        Self {
            python: None,
            shim_path: None,
            headless: true,
            headless_virtual: false,
            os: None,
            humanize_secs: None,
            geoip: false,
            locale: None,
            timezone: None,
            proxy_server: None,
            proxy_username: None,
            proxy_password: None,
            block_images: false,
            block_webrtc: false,
            fingerprint_preset: true,
            timeout_secs: 120,
            startup_timeout_secs: 300,
        }
    }
}

impl CamofoxConfig {
    fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.max(1))
    }

    fn startup_timeout(&self) -> Duration {
        Duration::from_secs(self.startup_timeout_secs.max(1))
    }

    fn python(&self) -> PathBuf {
        self.python
            .clone()
            .unwrap_or_else(|| PathBuf::from("python3"))
    }

    /// The launch-config JSON handed to the shim as argv[1].
    fn launch_config_json(&self) -> serde_json::Value {
        let headless = if self.headless_virtual {
            serde_json::json!("virtual")
        } else {
            serde_json::json!(self.headless)
        };
        let proxy = match self
            .proxy_server
            .as_deref()
            .filter(|s| !s.trim().is_empty())
        {
            Some(server) => {
                let mut p = serde_json::Map::new();
                p.insert(
                    "server".into(),
                    serde_json::Value::String(server.to_string()),
                );
                if let Some(u) = self.proxy_username.as_deref().filter(|s| !s.is_empty()) {
                    p.insert("username".into(), serde_json::Value::String(u.to_string()));
                }
                // The secret value travels to the shim only; it is never
                // logged or included in an error string.
                if let Some(pw) = self.proxy_password.as_deref().filter(|s| !s.is_empty()) {
                    p.insert("password".into(), serde_json::Value::String(pw.to_string()));
                }
                serde_json::Value::Object(p)
            }
            None => serde_json::Value::Null,
        };
        let mut cfg = serde_json::Map::new();
        cfg.insert("headless".into(), headless);
        cfg.insert("os".into(), opt_str(&self.os));
        cfg.insert(
            "humanize".into(),
            self.humanize_secs
                .map(serde_json::Number::from_f64)
                .and_then(|n| n.map(serde_json::Value::Number))
                .unwrap_or(serde_json::Value::Null),
        );
        cfg.insert("geoip".into(), serde_json::Value::Bool(self.geoip));
        cfg.insert("locale".into(), opt_str(&self.locale));
        cfg.insert("timezone".into(), opt_str(&self.timezone));
        cfg.insert("proxy".into(), proxy);
        cfg.insert(
            "block_images".into(),
            serde_json::Value::Bool(self.block_images),
        );
        cfg.insert(
            "block_webrtc".into(),
            serde_json::Value::Bool(self.block_webrtc),
        );
        cfg.insert(
            "fingerprint_preset".into(),
            serde_json::Value::Bool(self.fingerprint_preset),
        );
        serde_json::Value::Object(cfg)
    }

    /// Fail fast on contradictory fingerprint settings, before any
    /// subprocess spawns. Never includes secret values.
    pub fn validate(&self) -> Result<(), BrowserError> {
        if let Some(os) = self.os.as_deref() {
            match os.trim().to_ascii_lowercase().as_str() {
                "windows" | "macos" | "linux" => {}
                other => {
                    return Err(BrowserError::Failed {
                        argv: Vec::new(),
                        exit: None,
                        stderr: format!(
                            "camofox backend: [browser.camofox] os must be windows, macos, or linux, got {other:?}"
                        ),
                    });
                }
            }
        }
        if let Some(s) = self.humanize_secs {
            if !s.is_finite() || s < 0.0 {
                return Err(BrowserError::Failed {
                    argv: Vec::new(),
                    exit: None,
                    stderr: format!(
                        "camofox backend: [browser.camofox] humanize_secs must be >= 0, got {s}"
                    ),
                });
            }
        }
        Ok(())
    }
}

fn opt_str(v: &Option<String>) -> serde_json::Value {
    match v.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => serde_json::Value::String(s.to_string()),
        None => serde_json::Value::Null,
    }
}

/// Write the embedded shim to a content-hashed temp path (once) and
/// return the path to spawn. An explicit `shim_path` override skips
/// the embedded copy (shim development).
fn materialize_shim(shim_path: Option<&Path>) -> Result<PathBuf, BrowserError> {
    if let Some(p) = shim_path {
        return Ok(p.to_path_buf());
    }
    let mut hasher = DefaultHasher::new();
    SHIM_SOURCE.hash(&mut hasher);
    let dir = std::env::temp_dir().join(format!("pantheon-camofox-shim-{:016x}", hasher.finish()));
    let path = dir.join("camofox_shim.py");
    if !path.exists() {
        std::fs::create_dir_all(&dir).map_err(|e| BrowserError::Failed {
            argv: Vec::new(),
            exit: None,
            stderr: format!(
                "camofox backend: cannot create shim dir {}: {e}",
                dir.display()
            ),
        })?;
        std::fs::write(&path, SHIM_SOURCE).map_err(|e| BrowserError::Failed {
            argv: Vec::new(),
            exit: None,
            stderr: format!("camofox backend: cannot write shim {}: {e}", path.display()),
        })?;
    }
    Ok(path)
}

/// One live shim process: stdin for commands, a reader thread feeding
/// stdout lines into `lines`, and the child handle for kills.
struct ShimSession {
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
    child: Child,
}

impl ShimSession {
    fn send(&mut self, payload: &serde_json::Value, argv: &[String]) -> Result<(), BrowserError> {
        let mut line = serde_json::to_string(payload).map_err(|e| BrowserError::Failed {
            argv: argv.to_vec(),
            exit: None,
            stderr: format!("camofox backend: cannot encode command: {e}"),
        })?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .map_err(|e| BrowserError::Failed {
                argv: argv.to_vec(),
                exit: None,
                stderr: format!("camofox backend: shim stdin write failed: {e}"),
            })?;
        self.stdin.flush().map_err(|e| BrowserError::Failed {
            argv: argv.to_vec(),
            exit: None,
            stderr: format!("camofox backend: shim stdin flush failed: {e}"),
        })
    }
}

/// How a canonical argv reaches the shim.
#[derive(Debug)]
enum InvokeKind {
    /// Passed to the shim as-is (it speaks the canonical vocabulary).
    Shim(Vec<String>),
    /// Rust-side `wait-for` (probe polling through the shim's `eval`).
    WaitFor,
    /// Not implemented on this backend.
    Unsupported(String),
}

fn invoke_kind(argv: &[String]) -> InvokeKind {
    let flag = |name: &str| {
        argv.iter()
            .position(|a| a == name)
            .and_then(|i| argv.get(i + 1))
            .cloned()
    };
    let cmd = argv.first().map(String::as_str).unwrap_or("");
    match cmd {
        "wait-for" => InvokeKind::WaitFor,
        "act" | "act-instruction" => InvokeKind::Unsupported(cmd.to_string()),
        // `extract` reuses the shared extraction arrow JS, evaluated in
        // the page through the shim's `eval` — same as the Playwright
        // backend's `eval` mapping, minus the CLI.
        "extract" => InvokeKind::Shim(vec![
            "eval".to_string(),
            build_extract_arrow(flag("--schema").as_deref(), flag("--selector").as_deref()),
        ]),
        "navigate" | "back" | "forward" | "reload" | "snapshot" | "click-ref" | "hover-ref"
        | "fill-ref" | "click" | "type" | "press" | "screenshot" | "page-source" | "eval" => {
            InvokeKind::Shim(argv.to_vec())
        }
        other => InvokeKind::Unsupported(other.to_string()),
    }
}

/// Camoufox subprocess backend: one long-lived Python shim per session.
pub struct CamofoxBackend {
    config: CamofoxConfig,
    sessions: Mutex<HashMap<String, Arc<Mutex<ShimSession>>>>,
}

impl CamofoxBackend {
    pub fn new(config: CamofoxConfig) -> Self {
        Self {
            config,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Get the live shim for `session`, spawning it on first use.
    fn session(&self, name: &str) -> Result<Arc<Mutex<ShimSession>>, BrowserError> {
        let mut map = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(h) = map.get(name) {
            return Ok(h.clone());
        }
        // The map lock is held across the spawn so two threads cannot
        // launch duplicate shims for one session; the per-session lock
        // (not the map lock) serializes commands afterwards.
        let sess = self.spawn_session(name)?;
        let handle = Arc::new(Mutex::new(sess));
        map.insert(name.to_string(), handle.clone());
        Ok(handle)
    }

    /// Remove `name` from the map and kill its child. Idempotent.
    fn kill_session(&self, name: &str) {
        let handle = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
        if let Some(h) = handle {
            if let Ok(mut sess) = h.lock() {
                let _ = sess.child.kill();
                let _ = sess.child.wait();
            }
        }
    }

    fn spawn_error(&self, python: &Path, e: std::io::Error) -> BrowserError {
        match spawn_error(python, &[], e) {
            BrowserError::BinaryMissing { binary, detail } => BrowserError::BinaryMissing {
                binary,
                detail: format!("{detail}\n{CAMOFOX_INSTALL_INSTRUCTIONS}"),
            },
            other => other,
        }
    }

    /// Spawn the shim, run the launch handshake, and return the live
    /// session. A missing `camoufox` package / unfetched binary becomes
    /// [`BrowserError::BinaryMissing`] with install instructions —
    /// never a panic.
    fn spawn_session(&self, _session: &str) -> Result<ShimSession, BrowserError> {
        let shim = materialize_shim(self.config.shim_path.as_deref())?;
        let python = self.config.python();
        let cfg_json = serde_json::to_string(&self.config.launch_config_json())
            .unwrap_or_else(|_| "{}".into());
        let mut child = Command::new(&python)
            .arg(&shim)
            .arg(&cfg_json)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| self.spawn_error(&python, e))?;
        let stdin = child.stdin.take().ok_or_else(|| BrowserError::Failed {
            argv: Vec::new(),
            exit: None,
            stderr: "camofox backend: could not pipe shim stdin".into(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| BrowserError::Failed {
            argv: Vec::new(),
            exit: None,
            stderr: "camofox backend: could not pipe shim stdout".into(),
        })?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let mut sess = ShimSession {
            stdin,
            lines: rx,
            child,
        };
        // Launch handshake: the shim prints one JSON line once the
        // browser is up (or reports the package/binary as missing).
        let fail = |sess: &mut ShimSession, err: BrowserError| -> BrowserError {
            let _ = sess.child.kill();
            let _ = sess.child.wait();
            err
        };
        let line = match sess.lines.recv_timeout(self.config.startup_timeout()) {
            Ok(l) => l,
            Err(_) => {
                return Err(fail(
                    &mut sess,
                    BrowserError::Failed {
                        argv: Vec::new(),
                        exit: None,
                        stderr: format!(
                            "camofox backend: shim produced no handshake in {}s \
                             (browser launch hung or python3 failed silently; \
                             check that `python3 -c \"import camoufox\"` works)",
                            self.config.startup_timeout_secs.max(1)
                        ),
                    },
                ));
            }
        };
        let v: serde_json::Value = serde_json::from_str(line.trim()).map_err(|e| {
            fail(
                &mut sess,
                BrowserError::BadOutput {
                    argv: Vec::new(),
                    detail: format!(
                        "camofox backend: shim handshake was not JSON: {e}; \
                             line began with {:?}",
                        snip(&line, 300)
                    ),
                },
            )
        })?;
        if v.get("ready").and_then(|b| b.as_bool()) == Some(true) {
            return Ok(sess);
        }
        let shim_err = v
            .get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("shim failed to start")
            .to_string();
        if v.get("kind").and_then(|k| k.as_str()) == Some("missing") {
            return Err(fail(
                &mut sess,
                BrowserError::BinaryMissing {
                    binary: python.display().to_string(),
                    detail: format!("{shim_err}\n{CAMOFOX_INSTALL_INSTRUCTIONS}"),
                },
            ));
        }
        Err(fail(
            &mut sess,
            BrowserError::Failed {
                argv: Vec::new(),
                exit: None,
                stderr: format!(
                    "camofox backend: shim failed to start: {}",
                    snip(&shim_err, 2000)
                ),
            },
        ))
    }

    /// One command round-trip against a live session. Transport death
    /// (timeout / shim exit) kills the session so the next call starts
    /// clean; command-level failures keep the session alive.
    fn invoke_shim(
        &self,
        name: &str,
        cmd: &[String],
        argv: &[String],
    ) -> Result<serde_json::Value, BrowserError> {
        let handle = self.session(name)?;
        // The transport round-trip runs under the per-session lock; on
        // transport death the session is killed (outside the lock) so
        // the next call starts clean. Command-level failures keep the
        // session alive.
        let line: String = {
            let mut sess = handle.lock().unwrap_or_else(|e| e.into_inner());
            let payload = serde_json::json!({"argv": cmd});
            let roundtrip: Result<String, BrowserError> = (|| {
                sess.send(&payload, argv)?;
                match sess.lines.recv_timeout(self.config.timeout()) {
                    Ok(line) => Ok(line),
                    Err(mpsc::RecvTimeoutError::Timeout) => Err(BrowserError::Timeout {
                        argv: argv.to_vec(),
                        secs: self.config.timeout_secs.max(1),
                    }),
                    Err(mpsc::RecvTimeoutError::Disconnected) => Err(BrowserError::Failed {
                        argv: argv.to_vec(),
                        exit: None,
                        stderr: "camofox backend: shim exited unexpectedly".into(),
                    }),
                }
            })();
            match roundtrip {
                Ok(line) => line,
                Err(e) => {
                    drop(sess);
                    self.kill_session(name);
                    return Err(e);
                }
            }
        };
        let v: serde_json::Value =
            serde_json::from_str(line.trim()).map_err(|e| BrowserError::BadOutput {
                argv: argv.to_vec(),
                detail: format!(
                    "camofox backend: shim returned non-JSON: {e}; line began with {:?}",
                    snip(&line, 300)
                ),
            })?;
        if v.get("ok").and_then(|b| b.as_bool()) == Some(true) {
            Ok(v.get("result").cloned().unwrap_or(serde_json::Value::Null))
        } else if v.get("stale").and_then(|b| b.as_bool()) == Some(true) {
            Err(BrowserError::StaleRef {
                message: v
                    .get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("stale browser ref: take a fresh snapshot")
                    .to_string(),
            })
        } else {
            Err(BrowserError::Failed {
                argv: argv.to_vec(),
                exit: None,
                stderr: snip(
                    v.get("error")
                        .and_then(|e| e.as_str())
                        .unwrap_or("shim command failed"),
                    2000,
                ),
            })
        }
    }

    /// `wait-for`: poll a condition probe through the shim's `eval`
    /// until true or the command timeout elapses. `delay` sleeps once.
    /// Shares [`wait_plan`](super::cdp::wait_plan) with the Playwright
    /// backend, so condition semantics are identical.
    fn wait_for(&self, session: &str, argv: &[String]) -> Result<serde_json::Value, BrowserError> {
        match wait_plan(argv, "camofox")? {
            WaitPlan::Delay(ms) => {
                let capped = ms.min(self.config.timeout().as_millis() as u64);
                std::thread::sleep(Duration::from_millis(capped));
                Ok(serde_json::json!({"ok": true, "slept_ms": capped}))
            }
            WaitPlan::Probe(probe) => {
                let probe_js = format!("()=>{{return {probe};}}");
                let deadline = std::time::Instant::now() + self.config.timeout();
                loop {
                    let v =
                        self.invoke_shim(session, &["eval".to_string(), probe_js.clone()], argv)?;
                    if v.as_bool().unwrap_or(false) {
                        return Ok(serde_json::json!({"ok": true}));
                    }
                    if std::time::Instant::now() >= deadline {
                        return Err(BrowserError::Failed {
                            argv: argv.to_vec(),
                            exit: None,
                            stderr: "wait-for condition not met in time".into(),
                        });
                    }
                    std::thread::sleep(WAIT_POLL);
                }
            }
        }
    }
}

impl BrowserBackend for CamofoxBackend {
    fn invoke(&self, argv: &[String], session: &str) -> Result<serde_json::Value, BrowserError> {
        match invoke_kind(argv) {
            InvokeKind::Shim(cmd) => self.invoke_shim(session, &cmd, argv),
            InvokeKind::WaitFor => self.wait_for(session, argv),
            InvokeKind::Unsupported(cmd) => Err(BrowserError::UnsupportedCommand {
                command: cmd,
                backend: "camofox".to_string(),
            }),
        }
    }

    fn stop_daemon(&self, session: &str) -> Result<(), BrowserError> {
        let handle = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session);
        let Some(h) = handle else {
            return Ok(());
        };
        // Best-effort: the GC path must never fail on a dead shim.
        if let Ok(mut sess) = h.lock() {
            let _ = sess.send(&serde_json::json!({"argv": ["close"]}), &[]);
            let _ = sess.lines.recv_timeout(CLOSE_GRACE);
            let _ = sess.child.kill();
            let _ = sess.child.wait();
        }
        Ok(())
    }
}
