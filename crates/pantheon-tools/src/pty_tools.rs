//! Persistent terminal sessions.
//!
//! A normal `shell` call is one process with a timeout. These tools keep a
//! PTY and its child alive across tool calls, so the model can drive an
//! interactive program (a REPL, a debugger, a long build) without losing
//! state between turns. The manager is session-scoped: its working root is
//! the same confined workspace root as the file tools, and it is dropped
//! with the runtime session.
//!
//! The child is a real shell, so every tool carries `ShellExecute`. Opening
//! a command also passes through the same dangerous-pattern pre-gate as
//! `shell`; writes are raw PTY input because splitting a command at an
//! arbitrary byte boundary would make the gate both bypassable and noisy.

use crate::tools::{parse_args, tool_err, ToolRegistry};
use pantheon_api::capability::Capability;
use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::message::ToolSchema;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const MAX_BUFFER_BYTES: usize = 256 * 1024;

pub struct PtyManager {
    root: Mutex<PathBuf>,
    sessions: Mutex<HashMap<String, PtySession>>,
    next_id: AtomicU64,
}

impl PtyManager {
    pub fn new(root: PathBuf) -> Self {
        let root = root.canonicalize().unwrap_or(root);
        Self {
            root: Mutex::new(root),
            sessions: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    /// Live worktree moves update the manager's root. Existing terminals
    /// keep the cwd they were opened with; new terminals start in the new
    /// root.
    pub fn set_root(&self, root: PathBuf) {
        let root = root.canonicalize().unwrap_or(root);
        if let Ok(mut guard) = self.root.lock() {
            *guard = root;
        }
    }

    pub fn root(&self) -> PathBuf {
        self.root
            .lock()
            .map(|g| g.clone())
            .unwrap_or_else(|_| PathBuf::from("."))
    }

    fn open(
        &self,
        command: Option<&str>,
        cwd: Option<&str>,
        rows: u16,
        cols: u16,
    ) -> Result<String, PantheonError> {
        if let Some(command) = command {
            pantheon_exec::danger::gate(command)?;
        }
        let workspace = self.root();
        let cwd = match cwd {
            Some(cwd) => pantheon_exec::confine::confine(Path::new(cwd), &workspace)?,
            None => workspace.clone(),
        };
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: rows.clamp(1, 500),
                cols: cols.clamp(1, 500),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| perr("PTY_OPEN", format!("open pty: {e}")))?;
        let shell = default_shell();
        let mut cmd = CommandBuilder::new(&shell);
        if let Some(command) = command {
            if cfg!(windows) {
                cmd.arg("/C");
                cmd.arg(command);
            } else {
                cmd.arg("-lc");
                cmd.arg(command);
            }
        }
        cmd.cwd(&cwd);
        cmd.env("TERM", "xterm-256color");
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| perr("PTY_SPAWN", format!("spawn {shell}: {e}")))?;
        drop(pair.slave);
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| perr("PTY_READER", format!("clone pty reader: {e}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| perr("PTY_WRITER", format!("take pty writer: {e}")))?;
        let output = Arc::new(Mutex::new(VecDeque::new()));
        let closed = Arc::new(AtomicBool::new(false));
        spawn_reader(reader, Arc::clone(&output), Arc::clone(&closed));
        let id = format!("pty-{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        let session = PtySession {
            master: pair.master,
            writer,
            child,
            output,
            closed,
            command: command.unwrap_or(&shell).to_string(),
        };
        self.sessions
            .lock()
            .map_err(|_| perr("PTY_LOCK", "terminal manager lock is poisoned".into()))?
            .insert(id.clone(), session);
        Ok(id)
    }

    fn with_session<T>(
        &self,
        id: &str,
        f: impl FnOnce(&mut PtySession) -> Result<T, PantheonError>,
    ) -> Result<T, PantheonError> {
        let mut guard = self
            .sessions
            .lock()
            .map_err(|_| perr("PTY_LOCK", "terminal manager lock is poisoned".into()))?;
        let session = guard
            .get_mut(id)
            .ok_or_else(|| perr("PTY_NOT_FOUND", format!("no terminal session '{id}'")))?;
        f(session)
    }

    fn write(&self, id: &str, data: &str) -> Result<String, PantheonError> {
        self.with_session(id, |session| {
            session
                .writer
                .write_all(data.as_bytes())
                .and_then(|_| session.writer.flush())
                .map_err(|e| perr("PTY_WRITE", format!("write terminal '{id}': {e}")))?;
            Ok(format!("sent {} byte(s) to {id}", data.len()))
        })
    }

    fn read(
        &self,
        id: &str,
        timeout: Duration,
        max_bytes: usize,
    ) -> Result<String, PantheonError> {
        let deadline = Instant::now() + timeout;
        loop {
            let drained = self.with_session(id, |session| {
                let mut out = String::new();
                if let Ok(mut buf) = session.output.lock() {
                    while out.len() < max_bytes {
                        let Some(byte) = buf.pop_front() else {
                            break;
                        };
                        out.push(byte as char);
                    }
                }
                if !out.is_empty() {
                    return Ok(Some(out));
                }
                if session.closed.load(Ordering::Relaxed) {
                    return Ok(Some(String::new()));
                }
                Ok(None)
            })?;
            if let Some(out) = drained {
                return Ok(if out.is_empty() {
                    "(no output)".to_string()
                } else {
                    out
                });
            }
            if Instant::now() >= deadline {
                return Ok(String::new());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn resize(&self, id: &str, rows: u16, cols: u16) -> Result<String, PantheonError> {
        self.with_session(id, |session| {
            session
                .master
                .resize(PtySize {
                    rows: rows.clamp(1, 500),
                    cols: cols.clamp(1, 500),
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .map_err(|e| perr("PTY_RESIZE", format!("resize terminal '{id}': {e}")))?;
            Ok(format!("resized {id} to {cols}x{rows}"))
        })
    }

    fn close(&self, id: &str) -> Result<String, PantheonError> {
        let mut guard = self
            .sessions
            .lock()
            .map_err(|_| perr("PTY_LOCK", "terminal manager lock is poisoned".into()))?;
        let mut session = guard
            .remove(id)
            .ok_or_else(|| perr("PTY_NOT_FOUND", format!("no terminal session '{id}'")))?;
        session.kill();
        Ok(format!("closed {id}"))
    }

    fn list(&self) -> Result<Vec<serde_json::Value>, PantheonError> {
        let guard = self
            .sessions
            .lock()
            .map_err(|_| perr("PTY_LOCK", "terminal manager lock is poisoned".into()))?;
        Ok(guard
            .iter()
            .map(|(id, session)| {
                serde_json::json!({
                    "id": id,
                    "command": session.command,
                    "alive": !session.closed.load(Ordering::Relaxed),
                    "buffered_bytes": session.output.lock().map(|b| b.len()).unwrap_or(0),
                })
            })
            .collect())
    }
}

impl Drop for PtyManager {
    fn drop(&mut self) {
        if let Ok(mut sessions) = self.sessions.lock() {
            for (_, mut session) in sessions.drain() {
                session.kill();
            }
        }
    }
}

struct PtySession {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    output: Arc<Mutex<VecDeque<u8>>>,
    closed: Arc<AtomicBool>,
    command: String,
}

impl PtySession {
    fn kill(&mut self) {
        self.closed.store(true, Ordering::Relaxed);
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Dropping the master closes the PTY; the reader thread sees EOF.
        let _ = self.writer.flush();
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        self.kill();
    }
}

fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    output: Arc<Mutex<VecDeque<u8>>>,
    closed: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 4096];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Ok(mut buf) = output.lock() {
                        buf.extend(&chunk[..n]);
                        while buf.len() > MAX_BUFFER_BYTES {
                            buf.pop_front();
                        }
                    }
                }
            }
        }
        closed.store(true, Ordering::Relaxed);
    });
}

fn default_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            if cfg!(windows) {
                std::env::var("COMSPEC").ok()
            } else {
                None
            }
        })
        .unwrap_or_else(|| {
            if cfg!(windows) {
                "cmd.exe".to_string()
            } else {
                "/bin/sh".to_string()
            }
        })
}

fn perr(code: &str, cause: String) -> PantheonError {
    tool_err(
        code,
        Layer::Execution,
        false,
        cause,
        "check terminal arguments and retry",
    )
}

fn jstr(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(str::to_string)
}

fn ju16(v: &serde_json::Value, key: &str, default: u16) -> u16 {
    v.get(key)
        .and_then(|x| x.as_u64())
        .map(|n| n.min(u16::MAX as u64) as u16)
        .unwrap_or(default)
}

/// Register `terminal_open/read/write/resize/close/list`. The caller owns
/// the manager for the session's lifetime; registering without one is a
/// programming error, so this function takes the manager explicitly.
pub fn register_pty_tools(reg: &mut ToolRegistry, manager: Arc<PtyManager>) {
    let m = manager.clone();
    reg.register(
        ToolSchema {
            name: "terminal_open".into(),
            description: "Open a persistent PTY. Returns a terminal id. With no `command`, starts an interactive shell; with `command`, runs it through the shell and keeps the PTY for follow-up input. Use terminal_write/read to drive it. Requires ShellExecute.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "Optional command to run; omit for an interactive shell."},
                    "cwd": {"type": "string", "description": "Optional working directory inside the workspace."},
                    "rows": {"type": "integer", "minimum": 1, "maximum": 500},
                    "cols": {"type": "integer", "minimum": 1, "maximum": 500}
                }
            }),
        },
        Capability::ShellExecute,
        move |args| {
            let v = parse_args(args)?;
            let id = m.open(
                jstr(&v, "command").as_deref(),
                jstr(&v, "cwd").as_deref(),
                ju16(&v, "rows", 24),
                ju16(&v, "cols", 80),
            )?;
            Ok(format!(
                "{id}\nUse terminal_read {id} to collect output and terminal_write {id} to send input."
            ))
        },
    );

    let m = manager.clone();
    reg.register(
        ToolSchema {
            name: "terminal_write".into(),
            description: "Write raw input to a persistent PTY. Include a trailing newline to submit a shell command.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "data": {"type": "string"}
                },
                "required": ["session_id", "data"]
            }),
        },
        Capability::ShellExecute,
        move |args| {
            let v = parse_args(args)?;
            let id = jstr(&v, "session_id")
                .ok_or_else(|| perr("PTY_BAD_ARGS", "missing 'session_id'".into()))?;
            let data = jstr(&v, "data")
                .ok_or_else(|| perr("PTY_BAD_ARGS", "missing 'data'".into()))?;
            m.write(&id, &data)
        },
    );

    let m = manager.clone();
    reg.register(
        ToolSchema {
            name: "terminal_read".into(),
            description: "Read buffered output from a persistent PTY. `timeout_ms` waits for output (default 0 = immediate). Output is drained; it is not repeated on later reads.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "timeout_ms": {"type": "integer", "minimum": 0, "maximum": 30000},
                    "max_bytes": {"type": "integer", "minimum": 1, "maximum": 262144}
                },
                "required": ["session_id"]
            }),
        },
        Capability::ShellExecute,
        move |args| {
            let v = parse_args(args)?;
            let id = jstr(&v, "session_id")
                .ok_or_else(|| perr("PTY_BAD_ARGS", "missing 'session_id'".into()))?;
            let timeout = Duration::from_millis(
                v.get("timeout_ms")
                    .and_then(|x| x.as_u64())
                    .unwrap_or(0)
                    .min(30_000),
            );
            let max = v
                .get("max_bytes")
                .and_then(|x| x.as_u64())
                .unwrap_or(64 * 1024)
                .clamp(1, MAX_BUFFER_BYTES as u64) as usize;
            m.read(&id, timeout, max)
        },
    );

    let m = manager.clone();
    reg.register(
        ToolSchema {
            name: "terminal_resize".into(),
            description: "Resize a persistent PTY.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "session_id": {"type": "string"},
                    "rows": {"type": "integer", "minimum": 1, "maximum": 500},
                    "cols": {"type": "integer", "minimum": 1, "maximum": 500}
                },
                "required": ["session_id", "rows", "cols"]
            }),
        },
        Capability::ShellExecute,
        move |args| {
            let v = parse_args(args)?;
            let id = jstr(&v, "session_id")
                .ok_or_else(|| perr("PTY_BAD_ARGS", "missing 'session_id'".into()))?;
            m.resize(&id, ju16(&v, "rows", 24), ju16(&v, "cols", 80))
        },
    );

    let m = manager.clone();
    reg.register(
        ToolSchema {
            name: "terminal_close".into(),
            description: "Kill and remove a persistent PTY.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"session_id": {"type": "string"}},
                "required": ["session_id"]
            }),
        },
        Capability::ShellExecute,
        move |args| {
            let v = parse_args(args)?;
            let id = jstr(&v, "session_id")
                .ok_or_else(|| perr("PTY_BAD_ARGS", "missing 'session_id'".into()))?;
            m.close(&id)
        },
    );

    let m = manager;
    reg.register(
        ToolSchema {
            name: "terminal_list".into(),
            description: "List live persistent PTYs and their buffered output sizes.".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        },
        Capability::ShellExecute,
        move |_args| {
            serde_json::to_string_pretty(&m.list()?)
                .map_err(|e| perr("PTY_JSON", e.to_string()))
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-pty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn opens_writes_reads_and_closes_a_persistent_shell() {
        if cfg!(windows) {
            return;
        }
        let root = temp_root();
        let manager = PtyManager::new(root.clone());
        let id = manager.open(None, None, 24, 80).unwrap();
        assert!(manager.list().unwrap().iter().any(|v| v["id"] == id));
        manager.write(&id, "printf pty-ok\\n").unwrap();
        let mut text = String::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            text.push_str(&manager.read(&id, Duration::from_millis(100), 64 * 1024).unwrap());
            if text.contains("pty-ok") {
                break;
            }
        }
        assert!(text.contains("pty-ok"), "{text:?}");
        manager.resize(&id, 30, 100).unwrap();
        manager.close(&id).unwrap();
        assert!(manager.list().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unknown_session_is_a_structured_error() {
        let manager = PtyManager::new(temp_root());
        let err = manager.read("missing", Duration::ZERO, 1024).unwrap_err();
        assert!(err.to_string().contains("PTY_NOT_FOUND"));
    }

    #[test]
    fn cwd_is_confined_to_the_workspace() {
        let root = temp_root();
        let manager = PtyManager::new(root.clone());
        let err = manager.open(None, Some("/etc"), 24, 80).unwrap_err();
        assert!(err.to_string().contains("CONFINE"));
        let _ = std::fs::remove_dir_all(root);
    }
}
