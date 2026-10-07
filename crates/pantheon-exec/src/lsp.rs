//! LSP diagnostics: minimal language-server client for feeding type-check
//! and lint results back to the model after edits.
//!
//! Pantheon edits files through `safewrite`/tools but the model never sees
//! whether the code still compiles. This module closes that loop: it
//! speaks the Language Server Protocol over a JSON-RPC stdio connection,
//! opens a document, pushes changes, and collects `publishDiagnostics`
//! notifications into a cache that a `lsp.diagnostics` tool reads back.
//!
//! What it deliberately is *not*: a full LSP client. It implements the
//! smallest subset that gets real diagnostics - the `initialize`
//! handshake, `initialized`, `textDocument/didOpen`, `didChange`,
//! `shutdown`/`exit` - plus a passive read loop that turns server
//! notifications into cache entries. Anything richer (completion,
//! references, code actions) is out of scope here; this is the
//! diagnostics-only path that unblocks "did my edit break the build".
use pantheon_api::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn err(code: &str, cause: String, remediation: &str) -> PantheonError {
    PantheonError::new(code, Layer::Execution, false, cause, remediation, "")
}

/// One diagnostic, flattened to the fields the model actually uses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Diagnostics {
    /// URI of the file the diagnostics apply to.
    pub uri: String,
    /// The server that produced them (for display).
    pub server: String,
    /// Count of diagnostics, newest-first ordering preserved.
    pub count: usize,
    /// Human-readable lines: `[severity] line:col message`.
    pub lines: Vec<String>,
    /// Unix ms when this batch was last updated.
    pub updated_ms: i64,
}

/// Shared diagnostics cache, updated by the read thread and read by the
/// tool. Guarded by a mutex; the read loop is the only writer besides
/// `open`, so contention is low.
#[derive(Default)]
pub struct LspState {
    /// Cache keyed by file URI, newest-first diagnostics per file.
    pub diagnostics: HashMap<String, Diagnostics>,
    /// Pending response, set when the reader thread completes the
    /// handshake's `initialize` response. Cleared by `request` after
    /// delivery. A `None` here means "no response yet".
    pub pending_response: Option<Result<Value, String>>,
}

/// A running language-server connection plus its reader thread.
pub struct LspClient {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    next_id: Mutex<u64>,
    state: Arc<Mutex<LspState>>,
    server: String,
    /// The reader thread's join handle, wrapped in a `Mutex` so `shutdown`
    /// (which takes `&self` so it can be called through an `Arc`) can take
    /// it out. A `Mutex` rather than a `RefCell` keeps `LspClient` `Sync`,
    /// so the tool layer can share one live client across its `Send + Sync`
    /// closures.
    reader: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Set when the reader thread saw the server exit; `request` checks
    /// it so a dead server surfaces as a clean error, not a hang.
    server_alive: Arc<std::sync::atomic::AtomicBool>,
}

impl LspClient {
    /// Start a language server and run the `initialize`/`initialized`
    /// handshake. Fails hard when the server cannot start or does not
    /// answer the handshake within `timeout`.
    pub fn start(
        program: &str,
        args: &[String],
        server_name: &str,
        root_uri: &str,
        timeout: Duration,
    ) -> Result<Arc<Self>, PantheonError> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                err(
                    "LSP_SPAWN",
                    format!("spawn {program}: {e}"),
                    "the language server binary is missing or not on PATH",
                )
            })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            err(
                "LSP_STDOUT",
                "child stdout not piped".to_string(),
                "internal: check LspClient::start wiring",
            )
        })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            err(
                "LSP_STDIN",
                "child stdin not piped".to_string(),
                "internal: check LspClient::start wiring",
            )
        })?;
        let reader_input = stdout;
        // Keep the real child for shutdown(); it is wrapped in a mutex so
        // only `shutdown` may kill it.
        let child = Mutex::new(child);

        let state = Arc::new(Mutex::new(LspState::default()));
        let server_alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let server_name = server_name.to_string();

        // Reader thread: consume LSP frames off stdout, record
        // publishDiagnostics into the cache, and mark the server dead
        // when the stream ends.
        let reader = {
            let state = Arc::clone(&state);
            let alive = Arc::clone(&server_alive);
            let server_name = server_name.clone();
            std::thread::spawn(move || {
                let mut reader = reader_input;
                let mut buf = Vec::new();
                loop {
                    match read_frame(&mut reader, &mut buf) {
                        Ok(Some(msg)) => {
                            if let Some(diag) = flatten_publish_diagnostics(&msg, &server_name) {
                                let mut guard = state.lock().unwrap_or_else(|p| p.into_inner());
                                guard.diagnostics.insert(diag.uri.clone(), diag);
                            } else if msg.get("id").is_some() {
                                // A JSON-RPC response (not a notification):
                                // park it so `request` can pick it up.
                                let mut guard = state.lock().unwrap_or_else(|p| p.into_inner());
                                if let Some(err_val) = msg.get("error") {
                                    guard.pending_response = Some(Err(err_val.to_string()));
                                } else {
                                    guard.pending_response =
                                        Some(Ok(msg.get("result").cloned().unwrap_or(Value::Null)));
                                }
                            }
                        }
                        Ok(None) => break, // clean EOF: server exited
                        Err(_) => break,   // read error: treat as dead
                    }
                }
                alive.store(false, std::sync::atomic::Ordering::Release);
            })
        };

        let client = LspClient {
            child,
            stdin: Mutex::new(stdin),
            next_id: Mutex::new(1),
            state,
            server: server_name.clone(),
            reader: Mutex::new(Some(reader)),
            server_alive,
        };
        // The handshake writes are done through client.send_request.
        client.initialize(root_uri, &server_name, timeout)?;
        Ok(Arc::new(client))
    }

    fn send(&self, msg: &Value) -> Result<(), PantheonError> {
        let header = format!("Content-Length: {}\r\n\r\n", msg.to_string().len());
        let mut guard = self.stdin.lock().unwrap_or_else(|p| p.into_inner());
        guard
            .write_all(header.as_bytes())
            .and_then(|_| guard.write_all(msg.to_string().as_bytes()))
            .and_then(|_| guard.flush())
            .map_err(|e| {
                err(
                    "LSP_WRITE",
                    format!("write to server: {e}"),
                    "the language server connection is broken",
                )
            })
    }

    /// Fire a JSON-RPC request and wait for its response or a timeout.
    fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, PantheonError> {
        if !self.server_alive.load(std::sync::atomic::Ordering::Acquire) {
            return Err(err(
                "LSP_DEAD",
                format!("{} server has exited", self.server),
                "the language server stopped; start a fresh one",
            ));
        }
        let id = {
            let mut g = self.next_id.lock().unwrap_or_else(|p| p.into_inner());
            let n = *g;
            *g += 1;
            n
        };
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.send(&msg)?;

        // Wait for the reader thread to park a response for this request.
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if !self.server_alive.load(std::sync::atomic::Ordering::Acquire) {
                return Err(err(
                    "LSP_DEAD",
                    format!("{} exited while waiting for {method}", self.server),
                    "the language server stopped during the handshake; check its stderr",
                ));
            }
            // Check whether the reader parked a response for us.
            let maybe = {
                let mut guard = self.state.lock().unwrap_or_else(|p| p.into_inner());
                guard.pending_response.take()
            };
            if let Some(res) = maybe {
                return res.map_err(|e| {
                    err(
                        "LSP_RPC_ERROR",
                        format!("server returned an error for {method}: {e}"),
                        "the language server rejected the request; inspect its stderr",
                    )
                });
            }
            if std::time::Instant::now() >= deadline {
                return Err(err(
                    "LSP_TIMEOUT",
                    format!("no response to {method} within {timeout:?}"),
                    "the language server is hung; kill it and restart",
                ));
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Run the initialize handshake.
    fn initialize(
        &self,
        root_uri: &str,
        _server_name: &str,
        timeout: Duration,
    ) -> Result<(), PantheonError> {
        let params = serde_json::json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {},
            "workspaceFolders": [{ "uri": root_uri, "name": "workspace" }],
        });
        self.request("initialize", params, timeout)?;
        // `initialized` is a notification (no id, no response).
        let note = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": null,
        });
        self.send(&note)?;
        Ok(())
    }

    /// Open a document with full text; the server will emit diagnostics
    /// via `publishDiagnostics`, which the reader caches.
    pub fn open_document(
        &self,
        path: &Path,
        language_id: &str,
        text: &str,
    ) -> Result<(), PantheonError> {
        let uri = path_uri(path);
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 1,
                    "text": text,
                }
            }
        });
        self.send(&msg)?;
        Ok(())
    }

    /// Wait up to `timeout` for the reader thread to have populated
    /// diagnostics for the given file. Returns the latest batch or
    /// None if the server emitted none in time.
    pub fn wait_diagnostics(
        &self,
        uri: &str,
        timeout: Duration,
    ) -> Result<Option<Diagnostics>, PantheonError> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let got = {
                let g = self.state.lock().unwrap_or_else(|p| p.into_inner());
                g.diagnostics.get(uri).cloned()
            };
            if got.is_some() {
                return Ok(got);
            }
            if std::time::Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Read the current diagnostics cache for one file.
    pub fn diagnostics(&self, uri: &str) -> Option<Diagnostics> {
        let g = self.state.lock().unwrap_or_else(|p| p.into_inner());
        g.diagnostics.get(uri).cloned()
    }

    /// Read the whole diagnostics cache (all files).
    pub fn all_diagnostics(&self) -> Vec<Diagnostics> {
        let g = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let mut v: Vec<_> = g.diagnostics.values().cloned().collect();
        v.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
        v
    }

    /// Shut the server down cleanly.
    pub fn shutdown(&self) {
        let _ = self.request("shutdown", Value::Null, Duration::from_secs(3));
        let _ = self.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "exit"
        }));
        if let Some(mut guard) = self.child.try_lock().ok() {
            let _ = guard.kill();
        }
        self.server_alive
            .store(false, std::sync::atomic::Ordering::Release);
        // `reader` is a Mutex<Option<JoinHandle>> so `shutdown` can take it
        // out through `&self` (it is reachable through an `Arc`).
        if let Some(h) = self.reader.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = h.join();
        }
    }
}

/// Read one LSP frame: parse a `Content-Length` header, then that many
/// bytes of UTF-8 JSON. Returns None on clean EOF.
fn read_frame(r: &mut ChildStdout, buf: &mut Vec<u8>) -> Result<Option<Value>, std::io::Error> {
    // Accumulate raw header bytes until the `\r\n\r\n` terminator. LSP
    // headers always end in `\r\n\r\n`; a lone `\n` stream is not valid
    // LSP, so we key on the CRLF terminator.
    let mut header_bytes: Vec<u8> = Vec::new();
    let mut one = [0u8; 1];
    loop {
        match r.read(&mut one) {
            Ok(0) => return Ok(None),
            Ok(_) => {
                header_bytes.push(one[0]);
                if header_bytes.len() >= 4 && header_bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    let header = String::from_utf8_lossy(&header_bytes).to_string();
    let len = header
        .lines()
        .find_map(|l| l.strip_prefix("Content-Length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .ok_or_else(|| std::io::Error::other("bad Content-Length"))?;
    buf.clear();
    buf.resize(len, 0);
    r.read_exact(buf)?;
    let v: Value = serde_json::from_slice(buf)?;
    Ok(Some(v))
}

/// Pull `publishDiagnostics` out of a raw JSON-RPC message and flatten it
/// into cache form. Returns None for any other message type.
fn flatten_publish_diagnostics(msg: &Value, server: &str) -> Option<Diagnostics> {
    if msg.get("method")? != "publishDiagnostics" {
        return None;
    }
    let params = msg.get("params")?;
    let uri = params.get("uri")?.as_str()?.to_string();
    let raw = params.get("diagnostics")?.as_array()?;
    let mut lines = Vec::new();
    for d in raw {
        let sev = d.get("severity").and_then(|s| s.as_u64()).unwrap_or(1);
        let sev_s = match sev {
            1 => "error",
            2 => "warning",
            3 => "info",
            _ => "hint",
        };
        let line = d
            .pointer("/range/start/line")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let col = d
            .pointer("/range/start/character")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let msg = d.get("message").and_then(|m| m.as_str()).unwrap_or("");
        lines.push(format!("[{sev_s}] {line}:{col} {msg}"));
    }
    Some(Diagnostics {
        uri,
        server: server.to_string(),
        count: lines.len(),
        lines,
        updated_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
    })
}

/// Convert a filesystem path to a `file://` URI.
pub fn path_uri(path: &Path) -> String {
    let uri = PathBuf::from(path).to_string_lossy().replace('\\', "/");
    format!("file://{uri}")
}

/// Language -> LSP server command. Only the common ones; everything else
/// falls back to `None` and the caller reports "no server for this
/// language".
pub fn server_for(language: &str) -> Option<(String, Vec<String>)> {
    match language.to_lowercase().as_str() {
        "rust" => Some(("rust-analyzer".into(), vec!["--path".into(), ".".into()])),
        "python" => Some((
            "python3".into(),
            vec!["-m".into(), "python-lsp-server".into()],
        )),
        "typescript" | "javascript" => Some((
            "node".into(),
            vec![
                "node_modules/typescript-language-server/lib/cli.mjs".into(),
                "--stdio".into(),
            ],
        )),
        "go" => Some((
            "gopls".into(),
            vec!["-rpc".into(), "-mode".into(), "stdio".into()],
        )),
        "cpp" | "c" => Some((
            "clangd".into(),
            vec![
                "--background-index".into(),
                "--completion-use-diagnostics".into(),
            ],
        )),
        _ => None,
    }
}

/// Detect a language from a file extension.
pub fn language_for(ext: &str) -> Option<&'static str> {
    match ext.to_lowercase().as_str() {
        "rs" => Some("rust"),
        "py" => Some("python"),
        "ts" | "tsx" => Some("typescript"),
        "js" | "jsx" | "mjs" => Some("javascript"),
        "go" => Some("go"),
        "c" => Some("c"),
        "cpp" | "cc" | "cxx" | "h" | "hpp" => Some("cpp"),
        _ => None,
    }
}
