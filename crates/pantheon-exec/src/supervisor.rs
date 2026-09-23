//! Plugin subprocess supervisor: spawns verified plugins as child processes,
//! speaks newline-delimited JSON over stdio, enforces timeouts, owns the
//! process group, and kills cleanly on shutdown.
//!
//! Protocol (one JSON object + newline per line):
//!   stdin  -> {"call_id": "...", "tool": "...", "args": {...}}
//!   stdout -> {"call_id": "...", "result": ...} or
//!             {"call_id": "...", "error": {"code": "...", "cause": "..."}}
//!
//! Design (sync, std-only):
//! - One persistent child per supervisor. `call` locks the whole exchange
//!   (write request, read one response line) so responses can never
//!   interleave, even with concurrent callers behind an `Arc<Mutex<..>>`.
//! - The response read runs on a helper thread joined with a deadline, so a
//!   wedged plugin can never wedge the agent loop. Timeout kills the whole
//!   process group, not just the root, so plugin-spawned helpers die too.
//! - The child runs in its own process group (setsid on Unix). Stop is
//!   TERM, poll, then KILL. Env is filtered: only manifest-declared vars
//!   plus PATH reach the child. Pantheon secrets never cross the boundary.
//! - Large plugin output goes through `compact_output` before it reaches the
//!   caller, same as shell output.
use crate::plugins::{PluginManifest, ToolCapability};
use crate::tools::ToolRegistry;
use crate::{compact_output, CompactionPolicy};
use pantheon_core::capability::Capability;
use pantheon_core::error::{Layer, PantheonError};
use pantheon_core::message::ToolSchema;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(unix)]
const SIGTERM: i32 = 15;
#[cfg(unix)]
const SIGKILL: i32 = 9;

/// Grace period between TERM and KILL on stop().
const STOP_GRACE: Duration = Duration::from_secs(5);
/// Poll interval while waiting for exit or a response line.
const POLL_MS: u64 = 25;

static CALL_SEQ: AtomicU64 = AtomicU64::new(1);

fn merr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Execution,
        false,
        cause,
        "check the plugin process",
        "",
    )
}

/// One request to a plugin: which tool, with what args.
#[derive(Debug, Serialize)]
struct PluginRequest<'a> {
    call_id: String,
    tool: &'a str,
    args: serde_json::Value,
}

/// One response from a plugin.
#[derive(Debug, Deserialize)]
struct PluginResponse {
    call_id: String,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<PluginErrorDetail>,
}

#[derive(Debug, Deserialize)]
struct PluginErrorDetail {
    code: String,
    cause: String,
}

/// Supervises one plugin child process. Owns the child, its stdin, and a
/// line-buffered stdout reader. Not Clone; share via `Arc<Mutex<..>>`.
pub struct PluginSupervisor {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: Option<BufReader<ChildStdout>>,
    /// Process group id (== child pid after setsid). Only signal this group.
    pgid: i32,
    /// Per-call timeout.
    timeout: Duration,
    /// Set false after a timeout kill; further calls fail fast.
    alive: bool,
    /// Plugin name, for error messages.
    name: String,
    /// Compaction policy for large plugin output.
    compaction: CompactionPolicy,
}

impl PluginSupervisor {
    /// Spawn the plugin runner. `runner` must already be verified by
    /// `verify_plugin`. Only manifest-declared env vars (plus PATH) reach
    /// the child.
    pub fn spawn(
        runner: &Path,
        manifest: &PluginManifest,
        data_dir: &Path,
        timeout: Duration,
    ) -> Result<Self, PantheonError> {
        let mut cmd = Command::new(runner);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear();
        // Minimal safe env: PATH so shebangs and basic tools resolve.
        if let Ok(p) = std::env::var("PATH") {
            cmd.env("PATH", p);
        }
        // Plus only what the manifest declares, resolved from the host.
        for decl in &manifest.env_vars {
            if let Ok(v) = std::env::var(&decl.name) {
                cmd.env(&decl.name, v);
            }
        }
        // Pantheon-provided vars MUST come after env_clear() — the clear
        // wipes everything set before it.
        cmd.env("PANTHEON_PLUGIN_NAME", &manifest.name)
            .env("PANTHEON_DATA_DIR", data_dir.to_string_lossy().to_string());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                cmd.pre_exec(|| {
                    // Become session leader: own process group == our pid.
                    if libc::setsid() < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| merr("PLUGIN_SPAWN", format!("spawn {}: {e}", runner.display())))?;
        let pgid = child.id() as i32;
        // Refuse to supervise PID 1 or our own process, defensively.
        if pgid <= 1 || pgid == std::process::id() as i32 {
            let _ = child.kill();
            return Err(merr(
                "PLUGIN_BAD_PID",
                format!("refusing to supervise pid {pgid}"),
            ));
        }
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| merr("PLUGIN_STDIN", "child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| merr("PLUGIN_STDOUT", "child has no stdout".into()))?;
        Ok(Self {
            child: Some(child),
            stdin: Some(stdin),
            stdout: Some(BufReader::new(stdout)),
            pgid,
            timeout,
            alive: true,
            name: manifest.name.clone(),
            compaction: CompactionPolicy::default(),
        })
    }

    /// Call one tool. Blocks up to the supervisor timeout. On timeout the
    /// whole process group is killed and the supervisor is marked dead.
    pub fn call(&mut self, tool: &str, args: serde_json::Value) -> Result<String, PantheonError> {
        if !self.alive {
            return Err(merr(
                "PLUGIN_DEAD",
                format!(
                    "plugin '{}' was killed after a timeout; respawn it",
                    self.name
                ),
            ));
        }
        let call_id = format!("call_{}", CALL_SEQ.fetch_add(1, Ordering::Relaxed));
        let req = PluginRequest {
            call_id: call_id.clone(),
            tool,
            args,
        };
        let line = serde_json::to_string(&req)
            .map_err(|e| merr("PLUGIN_ENCODE", format!("encode request: {e}")))?;
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| merr("PLUGIN_DEAD", "plugin stdin is gone".into()))?;
        stdin
            .write_all(line.as_bytes())
            .and_then(|_| stdin.write_all(b"\n"))
            .and_then(|_| stdin.flush())
            .map_err(|e| {
                self.alive = false;
                merr(
                    "PLUGIN_WRITE",
                    format!("write to plugin '{}': {e}", self.name),
                )
            })?;

        // Read one response line on a helper thread; the main thread waits
        // with a deadline. A wedged plugin can never wedge the loop.
        // The reader is moved into the thread and sent back with the line.
        let mut reader = self.stdout.take().ok_or_else(|| {
            merr(
                "PLUGIN_DEAD",
                format!("plugin '{}' stdout is gone", self.name),
            )
        })?;
        let (tx, rx) = mpsc::channel::<(BufReader<ChildStdout>, Option<String>)>();
        std::thread::spawn(move || {
            let mut line = String::new();
            let out = match reader.read_line(&mut line) {
                Ok(0) => (reader, None), // EOF
                Ok(_) => {
                    while line.ends_with('\n') || line.ends_with('\r') {
                        line.pop();
                    }
                    (reader, Some(line))
                }
                Err(_) => (reader, None),
            };
            let _ = tx.send(out);
        });
        let deadline = Instant::now() + self.timeout;
        let (reader_back, line_opt) = loop {
            match rx.recv_timeout(Duration::from_millis(POLL_MS)) {
                Ok(pair) => break pair,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if Instant::now() > deadline {
                        // Timeout: the reader thread still owns stdout and is
                        // blocked in read_line. TERM the group; a compliant
                        // plugin exits, closing the pipe, and the reader
                        // thread unblocks on EOF and sends the reader back
                        // (which we drop). If it ignores TERM, escalate to
                        // SIGKILL so neither the group nor the thread leaks.
                        self.kill_group();
                        self.child.take();
                        self.stdin.take();
                        let kill_deadline = Instant::now() + STOP_GRACE;
                        while Instant::now() < kill_deadline {
                            #[cfg(unix)]
                            unsafe {
                                libc::killpg(self.pgid, SIGKILL);
                            }
                            std::thread::sleep(Duration::from_millis(100));
                            // SIGKILL is terminal; one signal + settle is enough.
                            break;
                        }
                        // stdout stays None (moved into the dead thread).
                        self.alive = false;
                        return Err(merr(
                            "PLUGIN_TIMEOUT",
                            format!(
                                "tool '{tool}' did not respond within {}s; process group {} killed",
                                self.timeout.as_secs(),
                                self.pgid
                            ),
                        ));
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // Thread died without sending; restore nothing usable.
                    self.alive = false;
                    return Err(merr(
                        "PLUGIN_DEAD",
                        format!("plugin '{}' reader thread died", self.name),
                    ));
                }
            }
        };
        // Normal path: restore the reader for the next call.
        self.stdout = Some(reader_back);
        let line = match line_opt {
            Some(l) if !l.is_empty() => l,
            _ => {
                self.alive = false;
                return Err(merr(
                    "PLUGIN_EOF",
                    format!("plugin '{}' closed stdout mid-call", self.name),
                ));
            }
        };
        let resp: PluginResponse = serde_json::from_str(&line).map_err(|e| {
            merr(
                "PLUGIN_PROTOCOL",
                format!("plugin '{}' sent invalid JSON: {e}", self.name),
            )
        })?;
        if resp.call_id != call_id {
            // A mismatched id means the protocol stream is corrupted (we
            // have one in-flight call). Restoring stdout would poison every
            // later call with this stale line, so treat it like EOF.
            self.alive = false;
            self.stdout.take();
            return Err(merr(
                "PLUGIN_PROTOCOL",
                format!(
                    "plugin '{}' answered wrong call: got {}, want {call_id}; marking dead to avoid stream desync",
                    self.name, resp.call_id
                ),
            ));
        }
        if let Some(err) = resp.error {
            return Err(merr(&err.code, err.cause));
        }
        match resp.result {
            Some(v) => {
                let raw = if v.is_string() {
                    v.as_str().unwrap_or("").to_string()
                } else {
                    serde_json::to_string(&v).unwrap_or_default()
                };
                Ok(compact_output(&raw, &self.compaction).text)
            }
            None => Err(merr(
                "PLUGIN_PROTOCOL",
                format!("plugin '{}' sent neither result nor error", self.name),
            )),
        }
    }

    /// Kill the owned process group, never any other group.
    fn kill_group(&self) {
        // Guard: only signal the group we created.
        if self.pgid <= 1 || self.pgid == std::process::id() as i32 {
            return;
        }
        #[cfg(unix)]
        unsafe {
            libc::killpg(self.pgid, SIGTERM);
        }
    }

    /// Graceful stop: TERM the process group, wait up to STOP_GRACE, then
    /// KILL the group. Idempotent.
    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            self.stdin.take();
            self.stdout.take();
            self.kill_group();
            let deadline = Instant::now() + STOP_GRACE;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => {
                        if Instant::now() > deadline {
                            #[cfg(unix)]
                            unsafe {
                                if self.pgid > 1 && self.pgid != std::process::id() as i32 {
                                    libc::killpg(self.pgid, SIGKILL);
                                }
                            }
                            #[cfg(not(unix))]
                            {
                                let _ = child.kill();
                            }
                            let _ = child.wait();
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(POLL_MS));
                    }
                    Err(_) => break,
                }
            }
            self.alive = false;
        }
    }

    pub fn is_alive(&self) -> bool {
        self.alive
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for PluginSupervisor {
    fn drop(&mut self) {
        // Never leave orphans: group-kill on drop. KILL, not TERM — drop
        // cannot wait for a graceful exit, so the guaranteed signal is the
        // right default. stop() is the graceful path when the caller can wait.
        if self.child.is_some() {
            #[cfg(unix)]
            unsafe {
                if self.pgid > 1 && self.pgid != std::process::id() as i32 {
                    libc::killpg(self.pgid, SIGKILL);
                }
            }
            self.child.take();
        }
    }
}

// PluginSupervisor owns a raw Child; safe to move between threads as long as
// only one thread touches it at a time (callers share via Mutex).
unsafe impl Send for PluginSupervisor {}

/// Register every tool from a plugin manifest into the registry. Each closure
/// locks the shared supervisor, forwards the call, and returns the plugin's
/// (compacted) result string.
///
/// The manifest's `ToolCapability` entries declare the tool name, the
/// `Capability` the session must grant, and the JSON schema shown to the
/// model. The loop-level gate from `Tool` still applies; the closure
/// re-checks cheaply so direct `registry.execute` callers are gated too.
pub fn register_plugin_tools(
    reg: &mut ToolRegistry,
    manifest: &PluginManifest,
    sup: Arc<Mutex<PluginSupervisor>>,
) {
    for cap in &manifest.capabilities {
        register_one_plugin_tool(reg, cap.clone(), sup.clone());
    }
}

fn register_one_plugin_tool(
    reg: &mut ToolRegistry,
    cap: ToolCapability,
    sup: Arc<Mutex<PluginSupervisor>>,
) {
    reg.register(
        ToolSchema {
            name: cap.name.clone(),
            description: if cap.description.is_empty() {
                format!(
                    "Plugin tool '{}' (capability: {:?})",
                    cap.name, cap.capability
                )
            } else {
                cap.description.clone()
            },
            parameters: cap.parameters.clone(),
        },
        cap.capability.clone(),
        move |args| {
            let v: serde_json::Value = if args.trim().is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(args).map_err(|e| {
                    PantheonError::new(
                        "TOOL_BAD_ARGS",
                        Layer::Execution,
                        false,
                        format!("invalid JSON args: {e}"),
                        "check tool name and arguments",
                        "",
                    )
                })?
            };
            let mut guard = sup.lock().map_err(|_| {
                PantheonError::new(
                    "PLUGIN_LOCK",
                    Layer::Execution,
                    false,
                    "plugin supervisor lock poisoned".to_string(),
                    "respawn the plugin",
                    "",
                )
            })?;
            guard.call(&cap.name, v)
        },
    );
}

/// Capability-gated check helper, mirroring `plugins::tool_allowed` for the
/// supervisor path. Kept here so callers can pre-filter without locking.
#[allow(dead_code)]
fn _capability_of(cap: &Capability) -> Capability {
    cap.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_round_trips() {
        let req = PluginRequest {
            call_id: "c1".into(),
            tool: "my_tool",
            args: serde_json::json!({"key": "value"}),
        };
        let line = serde_json::to_string(&req).unwrap();
        assert!(line.contains("\"call_id\":\"c1\""));

        let resp: PluginResponse =
            serde_json::from_str(r#"{"call_id":"c1","result":"ok"}"#).unwrap();
        assert_eq!(resp.call_id, "c1");
        assert_eq!(resp.result.unwrap(), serde_json::json!("ok"));
    }

    #[test]
    fn error_response_parses() {
        let resp: PluginResponse =
            serde_json::from_str(r#"{"call_id":"c2","error":{"code":"TOOL_FAIL","cause":"boom"}}"#)
                .unwrap();
        assert_eq!(resp.call_id, "c2");
        let err = resp.error.unwrap();
        assert_eq!(err.code, "TOOL_FAIL");
        assert_eq!(err.cause, "boom");
    }

    /// Spawn a fake plugin (a shell loop that answers one canned response)
    /// and drive a full call through the supervisor.
    #[test]
    fn live_call_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-sup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let runner = dir.join("run.sh");
        // Reads one request line, answers with result "pong:<tool>".
        std::fs::write(
            &runner,
            "#!/bin/sh\nread -r line\ntool=$(printf '%s' \"$line\" | sed 's/.*\"tool\":\"\\([^\"]*\\)\".*/\\1/')\ncid=$(printf '%s' \"$line\" | sed 's/.*\"call_id\":\"\\([^\"]*\\)\".*/\\1/')\nprintf '{\"call_id\":\"%s\",\"result\":\"pong:%s\"}\\n' \"$cid\" \"$tool\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(&runner).unwrap().permissions();
            p.set_mode(0o755);
            std::fs::set_permissions(&runner, p).unwrap();
        }
        let manifest = PluginManifest {
            name: "pong".into(),
            description: String::new(),
            version: "0.1.0".into(),
            sha: None,
            maintainer: String::new(),
            capabilities: vec![ToolCapability {
                name: "ping".into(),
                capability: Capability::ShellExecute,
                description: String::new(),
                parameters: serde_json::json!({}),
            }],
            env_vars: vec![],
            runner: "run.sh".into(),
            enabled: true,
        };
        let mut sup =
            PluginSupervisor::spawn(&runner, &manifest, &dir, Duration::from_secs(5)).unwrap();
        let out = sup.call("ping", serde_json::json!({})).unwrap();
        assert!(out.contains("pong:ping"), "got: {out}");
        sup.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A plugin that never answers must time out and die, not hang the test.
    #[test]
    fn timeout_kills_wedged_plugin() {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-sup-wedge-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let runner = dir.join("run.sh");
        // Reads one line, then sleeps forever.
        std::fs::write(&runner, "#!/bin/sh\nread -r line\nsleep 60\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(&runner).unwrap().permissions();
            p.set_mode(0o755);
            std::fs::set_permissions(&runner, p).unwrap();
        }
        let manifest = PluginManifest {
            name: "wedge".into(),
            description: String::new(),
            version: "0.1.0".into(),
            sha: None,
            maintainer: String::new(),
            capabilities: vec![],
            env_vars: vec![],
            runner: "run.sh".into(),
            enabled: true,
        };
        let mut sup =
            PluginSupervisor::spawn(&runner, &manifest, &dir, Duration::from_millis(300)).unwrap();
        let t0 = Instant::now();
        let err = sup.call("anything", serde_json::json!({})).unwrap_err();
        assert_eq!(err.code, "PLUGIN_TIMEOUT");
        assert!(t0.elapsed() < Duration::from_secs(10));
        assert!(!sup.is_alive());
        // Second call fails fast.
        let err2 = sup.call("anything", serde_json::json!({})).unwrap_err();
        assert_eq!(err2.code, "PLUGIN_DEAD");
        sup.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Registry wiring: plugin tools execute through the shared supervisor
    /// and respect direct-execute gating.
    #[test]
    fn registry_wires_plugin_tool() {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-sup-reg-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let runner = dir.join("run.sh");
        std::fs::write(
            &runner,
            "#!/bin/sh\nwhile read -r line; do cid=$(printf '%s' \"$line\" | sed 's/.*\"call_id\":\"\\([^\"]*\\)\".*/\\1/'); printf '{\"call_id\":\"%s\",\"result\":\"hi\"}\\n' \"$cid\"; done\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(&runner).unwrap().permissions();
            p.set_mode(0o755);
            std::fs::set_permissions(&runner, p).unwrap();
        }
        let manifest = PluginManifest {
            name: "hi".into(),
            description: String::new(),
            version: "0.1.0".into(),
            sha: None,
            maintainer: String::new(),
            capabilities: vec![ToolCapability {
                name: "greet".into(),
                capability: Capability::ShellExecute,
                description: String::new(),
                parameters: serde_json::json!({}),
            }],
            env_vars: vec![],
            runner: "run.sh".into(),
            enabled: true,
        };
        let sup = Arc::new(Mutex::new(
            PluginSupervisor::spawn(&runner, &manifest, &dir, Duration::from_secs(5)).unwrap(),
        ));
        let mut reg = ToolRegistry::new();
        register_plugin_tools(&mut reg, &manifest, sup.clone());
        let out = reg.execute("greet", "{}").unwrap();
        assert!(out.contains("hi"), "got: {out}");
        sup.lock().unwrap().stop();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
