//! Minimal real MCP client over stdio.
//!
//! Protocol subset: `initialize` (with protocol-version negotiation),
//! `tools/list`, `tools/call`. Everything else - resources, prompts,
//! sampling, roots, elicitation - is out of scope for this pass.
//!
//! # Call direction
//!
//! This client only initiates calls **to** the server. Calls **from** the
//! server to the client (`sampling/createMessage`, `roots/list`, ...) are out
//! of scope: the reader answers any server-initiated request with JSON-RPC
//! `-32601 Method not found` and moves on, so a chatty server cannot wedge
//! the request/response pairing.
//!
//! # Capability enforcement
//!
//! Every `tools/call` passes through the embedder-supplied
//! [`CapabilityGate`] **before** anything is written to the server's stdin.
//! A denial aborts the call; the server never sees it.

use crate::framed::{self, FrameError};
use crate::{classify, result_text, McpConn, Wire};
use pantheon_api::capability::{Capability, Policy};
use pantheon_api::message::ToolSchema;
use pantheon_exec::sandbox::{build_sandboxed, enforce, Enforcement};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::io::{BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Latest protocol version this client speaks; sent in `initialize`.
pub const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

/// Protocol versions the client understands. The server's answered version
/// must be one of these, otherwise the handshake fails rather than guessing
/// at compatibility.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// Default per-request timeout: every request must complete within this.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Default cap on a single framed message, both directions.
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 10 * 1024 * 1024;

/// How to spawn one MCP server.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    /// Label for this server, carried on every [`McpToolDef`].
    pub name: String,
    /// Program to spawn.
    pub command: String,
    /// Arguments for the program.
    pub args: Vec<String>,
    /// Timeout for every request (`initialize`, `tools/list`, `tools/call`).
    pub request_timeout: Duration,
    /// Max bytes for one JSON-RPC message, either direction.
    pub max_message_bytes: usize,
    /// Extra environment for the child, applied on top of the cleared
    /// environment (plus PATH). Values are already resolved - the
    /// manager turns `env:NAME` refs into values before building this,
    /// so this struct never sees a ref it cannot resolve. Never logged:
    /// only the variable names travel in errors.
    pub env: HashMap<String, String>,
}

impl McpServerConfig {
    /// Minimal config: 30s timeout, 10MiB message cap.
    pub fn new(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            args: Vec::new(),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            env: HashMap::new(),
        }
    }

    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }

    pub fn with_env(mut self, env: HashMap<String, String>) -> Self {
        self.env = env;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }
}

/// Structured errors from the client. Every failure mode is a variant, so
/// callers can match instead of parsing strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpError {
    /// The server process could not be spawned.
    Spawn(String),
    /// I/O with the child failed (write, or the pipe broke mid-request).
    Io(String),
    /// No matching response within the request timeout. The child has been
    /// killed; the client is dead.
    Timeout { method: String },
    /// The transport is dead (timeout, EOF, or framing failure killed it).
    Closed,
    /// The server is in reconnect backoff: not dead, but the manager will
    /// not attempt a new connection until the cooldown elapses.
    Unavailable { retry_in_secs: u64 },
    /// A message exceeded the configured cap. The child has been killed.
    Oversized { bytes: usize },
    /// The server sent bytes that are not a valid framed message.
    Framing(String),
    /// JSON-RPC shape violation: missing `jsonrpc: "2.0"`, unexpected
    /// message kind, or a protocol version we do not speak.
    Protocol(String),
    /// The server answered with a JSON-RPC error object.
    Rpc { code: i64, message: String },
    /// `tools/call` returned `isError: true`. Carries the server's text.
    ServerToolError(String),
    /// The capability gate denied the call. The server never saw it.
    GateDenied(String),
    /// The sandbox policy denied the spawn (or held it for approval).
    /// The server process was never started.
    PolicyDenied(String),
}

impl fmt::Display for McpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            McpError::Spawn(e) => write!(f, "mcp: spawn failed: {e}"),
            McpError::Io(e) => write!(f, "mcp: i/o error: {e}"),
            McpError::Timeout { method } => {
                write!(f, "mcp: request '{method}' timed out; server killed")
            }
            McpError::Closed => write!(f, "mcp: transport closed"),
            McpError::Unavailable { retry_in_secs } => write!(
                f,
                "mcp: server in reconnect backoff, retry in {retry_in_secs}s"
            ),
            McpError::Oversized { bytes } => {
                write!(
                    f,
                    "mcp: message of {bytes} bytes exceeds the cap; server killed"
                )
            }
            McpError::Framing(e) => write!(f, "mcp: bad frame from server: {e}"),
            McpError::Protocol(e) => write!(f, "mcp: protocol violation: {e}"),
            McpError::Rpc { code, message } => {
                write!(f, "mcp: server error {code}: {message}")
            }
            McpError::ServerToolError(t) => write!(f, "mcp: tool reported error: {t}"),
            McpError::GateDenied(e) => write!(f, "mcp: capability gate denied the call: {e}"),
            McpError::PolicyDenied(e) => {
                write!(f, "mcp: sandbox policy denied the spawn: {e}")
            }
        }
    }
}

impl std::error::Error for McpError {}

/// Call-time capability gate, supplied by the embedder. The client calls
/// [`CapabilityGate::check`] before every `tools/call`; `Ok(())` lets the
/// call through, `Err` aborts it before anything reaches the server.
///
/// Closures work too: any `Fn(&str, &Value) -> Result<(), McpError>` that is
/// `Send + Sync` implements this trait.
pub trait CapabilityGate: Send + Sync {
    fn check(&self, tool_name: &str, args: &Value) -> Result<(), McpError>;
}

impl<F> CapabilityGate for F
where
    F: Fn(&str, &Value) -> Result<(), McpError> + Send + Sync,
{
    fn check(&self, tool_name: &str, args: &Value) -> Result<(), McpError> {
        self(tool_name, args)
    }
}

/// One tool from `tools/list`, in a shape a later step can register with
/// `pantheon-tools`. Use [`McpToolDef::to_tool_schema`] for the schema half
/// of `ToolRegistry::register`; the capability half comes from the
/// embedder's policy, and the `run` half is a closure over
/// [`McpClient::call_tool`] plus the embedder's gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolDef {
    /// Server label from [`McpServerConfig::name`].
    pub server: String,
    pub name: String,
    pub description: String,
    /// The tool's `inputSchema`, passed through verbatim.
    pub input_schema: Value,
}

impl McpToolDef {
    pub fn to_tool_schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: self.input_schema.clone(),
        }
    }
}

/// Events from the stdout reader thread.
enum ReadEvent {
    Message(Value),
    Eof,
    Error(FrameError),
}

/// A connected MCP server: spawned child, framed stdio, handshook.
///
/// Not `Sync`: requests are issued one at a time from the owning thread.
/// `Send` so an embedder can move it between threads.
pub struct McpClient {
    server: String,
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<ReadEvent>,
    next_id: u64,
    timeout: Duration,
    max_bytes: usize,
    /// Protocol version the server answered in `initialize`.
    pub negotiated_version: String,
    /// The server's self-reported version (`serverInfo.version`), when it
    /// sends one. The manager binds operator approval to this plus the
    /// content hash.
    pub server_version: Option<String>,
    dead: bool,
}

impl McpClient {
    /// Spawn the server and run the `initialize` handshake. The child is a
    /// third-party process: its environment is cleared down to `PATH` plus
    /// the explicitly configured `env` entries, so host secrets never leak
    /// across the boundary except the ones the operator declared.
    ///
    /// `policy` gates the spawn through the sandbox enforcement bridge:
    /// `Deny` / `RequireApproval` abort before the process starts
    /// ([`McpError::PolicyDenied`]); `Allow` runs the server inside the
    /// enforcement's sandbox profile. `None` = no policy configured: the
    /// spawn proceeds un-gated, as before.
    pub fn connect(config: &McpServerConfig, policy: Option<&Policy>) -> Result<Self, McpError> {
        // Sandbox enforcement: the capability policy owns *whether* the
        // server process may run at all.
        let profile = match policy {
            Some(policy) => match enforce(policy, &Capability::McpEnable) {
                Enforcement::Run(profile) => Some(profile),
                Enforcement::Deny { reason, .. } => {
                    return Err(McpError::PolicyDenied(format!("mcp.enable: {reason}")))
                }
                Enforcement::RequireApproval { scope, .. } => {
                    return Err(McpError::PolicyDenied(format!(
                        "mcp.enable requires approval (scope: {scope})"
                    )))
                }
            },
            None => None,
        };
        let args: Vec<&str> = config.args.iter().map(|s| s.as_str()).collect();
        // The sandbox builder needs an explicit cwd; inheriting the
        // process cwd preserves the previous (unset-cwd) behavior.
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".to_string());
        let mut cmd = match &profile {
            // Policy allowed it: the enforcement's sandbox profile decides
            // how isolated the child runs (namespace wrapper + rlimits
            // where the host supports them; a direct spawn where it does
            // not - the policy gate above is what restores enforcement).
            Some(profile) => build_sandboxed(profile, &config.command, &args, &cwd),
            None => {
                let mut cmd = Command::new(&config.command);
                cmd.args(&args);
                cmd
            }
        };
        cmd.env_clear();
        if let Ok(p) = std::env::var("PATH") {
            cmd.env("PATH", p);
        }
        for (k, v) in &config.env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| McpError::Spawn(format!("{}: {e}", config.command)))?;
        let stdin = child.stdin.take().ok_or(McpError::Io("no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or(McpError::Io("no stdout".into()))?;

        let (tx, rx) = mpsc::channel();
        let max_bytes = config.max_message_bytes;
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match framed::read_message(&mut reader, max_bytes) {
                    Ok(Some(v)) => {
                        if tx.send(ReadEvent::Message(v)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        let _ = tx.send(ReadEvent::Eof);
                        break;
                    }
                    Err(e) => {
                        let _ = tx.send(ReadEvent::Error(e));
                        break;
                    }
                }
            }
        });

        let mut client = Self {
            server: config.name.clone(),
            child,
            stdin,
            rx,
            next_id: 1,
            timeout: config.request_timeout,
            max_bytes: config.max_message_bytes,
            negotiated_version: String::new(),
            server_version: None,
            dead: false,
        };
        client.initialize()?;
        Ok(client)
    }

    fn send(&mut self, body: &Value) -> Result<(), McpError> {
        let bytes = framed::encode(body, self.max_bytes).map_err(|e| match e {
            FrameError::Oversized(n) => McpError::Oversized { bytes: n },
            other => McpError::Framing(format!("encode: {other:?}")),
        })?;
        self.stdin
            .write_all(&bytes)
            .map_err(|e| McpError::Io(format!("write: {e}")))?;
        self.stdin
            .flush()
            .map_err(|e| McpError::Io(format!("flush: {e}")))?;
        Ok(())
    }

    /// `initialize` handshake: offer our protocol version, require the
    /// server's answer to be one we know, then send the mandatory
    /// `notifications/initialized` notification.
    fn initialize(&mut self) -> Result<(), McpError> {
        let result = self.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "pantheon",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        )?;
        let version = result
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                McpError::Protocol("initialize response has no protocolVersion".into())
            })?;
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&version) {
            return Err(McpError::Protocol(format!(
                "server protocol version {version:?} not in supported {:?}",
                SUPPORTED_PROTOCOL_VERSIONS
            )));
        }
        self.negotiated_version = version.to_string();
        self.server_version = result
            .get("serverInfo")
            .and_then(|i| i.get("version"))
            .and_then(|v| v.as_str())
            .map(str::to_string);
        // Notification: no id, no response expected.
        self.send(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
        }))
        .inspect_err(|_| {
            self.kill();
        })
    }

    /// Send one request and wait for the response with the matching id.
    /// Notifications and unrelated responses are skipped; server-initiated
    /// requests get a `-32601` reply (out of scope) so the pairing cannot
    /// wedge. On timeout the child is killed and the client is dead.
    fn request(&mut self, method: &str, params: Value) -> Result<Value, McpError> {
        if self.dead {
            return Err(McpError::Closed);
        }
        let id = self.next_id;
        self.next_id += 1;
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if let Err(e) = self.send(&body) {
            self.kill();
            return Err(e);
        }
        let deadline = Instant::now() + self.timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(remaining) {
                Err(RecvTimeoutError::Timeout) => {
                    self.kill();
                    return Err(McpError::Timeout {
                        method: method.to_string(),
                    });
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.dead = true;
                    return Err(McpError::Closed);
                }
                Ok(ReadEvent::Eof) => {
                    self.dead = true;
                    return Err(McpError::Closed);
                }
                Ok(ReadEvent::Error(e)) => {
                    self.kill();
                    return Err(match e {
                        FrameError::Oversized(bytes) => McpError::Oversized { bytes },
                        FrameError::BadJson(msg) => McpError::Framing(format!("bad JSON: {msg}")),
                        FrameError::BadHeader(msg) => {
                            McpError::Framing(format!("bad header: {msg}"))
                        }
                        FrameError::Io(msg) => McpError::Io(msg),
                    });
                }
                Ok(ReadEvent::Message(v)) => match classify(&v) {
                    Wire::Response {
                        id: rid,
                        result,
                        error,
                    } if rid == id => {
                        if let Some((code, message)) = error {
                            return Err(McpError::Rpc { code, message });
                        }
                        return Ok(result.unwrap_or(Value::Null));
                    }
                    Wire::Request { id: rid, .. } => {
                        // Server-to-client calls (sampling, roots, ...) are
                        // out of scope: answer "method not found" and carry
                        // on waiting for our response.
                        let _ = self.send(&serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": rid,
                            "error": {"code": -32601, "message": "method not found"},
                        }));
                    }
                    _ => {}
                },
            }
        }
    }

    /// `tools/list`: the tool definitions a later step can register.
    pub fn list_tools(&mut self) -> Result<Vec<McpToolDef>, McpError> {
        let result = self.request("tools/list", serde_json::json!({}))?;
        let tools = result
            .get("tools")
            .and_then(|t| t.as_array())
            .ok_or_else(|| McpError::Protocol("tools/list response has no tools array".into()))?;
        tools
            .iter()
            .map(|t| {
                let name = t
                    .get("name")
                    .and_then(|n| n.as_str())
                    .ok_or_else(|| McpError::Protocol("tool without a name".into()))?;
                Ok(McpToolDef {
                    server: self.server.clone(),
                    name: name.to_string(),
                    description: t
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("")
                        .to_string(),
                    input_schema: t.get("inputSchema").cloned().unwrap_or(Value::Null),
                })
            })
            .collect()
    }

    /// `tools/call`: the gate runs **first** - a denial aborts the call
    /// before anything is written to the server. On success returns the raw
    /// `result` object; when the server reports `isError: true` this is
    /// [`McpError::ServerToolError`] carrying the server's text.
    pub fn call_tool(
        &mut self,
        name: &str,
        args: &Value,
        gate: &dyn CapabilityGate,
    ) -> Result<Value, McpError> {
        gate.check(name, args)?;
        let result = self.request(
            "tools/call",
            serde_json::json!({"name": name, "arguments": args}),
        )?;
        if result.get("isError").and_then(|b| b.as_bool()) == Some(true) {
            return Err(McpError::ServerToolError(result_text(&result)));
        }
        Ok(result)
    }

    /// Whether the child process is still running.
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Kill the child and mark the client dead. Idempotent.
    fn kill(&mut self) {
        self.dead = true;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        self.kill();
    }
}

impl McpConn for McpClient {
    fn list_tools(&mut self) -> Result<Vec<McpToolDef>, McpError> {
        McpClient::list_tools(self)
    }

    /// The registry boundary (`ToolRegistry::execute_gated`) already
    /// enforced policy before the projected tool's closure runs, so the
    /// client-level gate is allow-all here. Direct `McpClient` users
    /// still pass their own gate to [`McpClient::call_tool`].
    fn call_tool(&mut self, name: &str, args: &Value) -> Result<Value, McpError> {
        McpClient::call_tool(self, name, args, &|_: &str, _: &Value| Ok(()))
    }

    fn alive(&mut self) -> bool {
        McpClient::alive(self)
    }

    fn shutdown(&mut self) {
        self.kill();
    }

    fn negotiated_version(&self) -> &str {
        &self.negotiated_version
    }

    fn server_version(&self) -> Option<&str> {
        self.server_version.as_deref()
    }
}

#[cfg(test)]
mod client_policy_tests {
    use super::*;
    use pantheon_api::capability::{Capability, Policy};

    /// Item 2: a policy that denies `mcp.enable` refuses the spawn
    /// the server process is never started (the command below does not
    /// exist; a spawn attempt would surface as `Spawn`, never
    /// `PolicyDenied`).
    #[test]
    fn connect_denied_when_policy_denies_mcp_enable() {
        // Policy::default() is default-deny: no rule grants mcp.enable.
        let cfg = McpServerConfig::new("denied-server", "definitely-not-a-real-binary");
        match McpClient::connect(&cfg, Some(&Policy::default())) {
            Err(McpError::PolicyDenied(_)) => {}
            other => panic!("expected PolicyDenied, got: {}", other.is_ok()),
        }
    }

    /// Item 2: a policy that allows `mcp.enable` lets the spawn proceed
    /// through the sandbox profile - the gate was consulted and passed,
    /// and the server completes the `initialize` handshake.
    ///
    /// `McpEnable` maps to `VeryHigh`, so the spawn needs a working
    /// bwrap user namespace. Hosts that refuse the uid map fail closed by
    /// design; that is not what this test covers, so skip and say so.
    #[test]
    fn connect_proceeds_when_policy_allows_mcp_enable() {
        if !pantheon_exec::sandbox::boundary_available(
            pantheon_exec::sandbox::ExecutionBoundary::StrictNamespaces,
        ) {
            eprintln!("SKIP: host cannot build the StrictNamespaces boundary (bwrap userns)");
            return;
        }
        // Minimal fake MCP server: answers `initialize`, then idles.
        // Lines are joined explicitly so Python keeps its indentation
        // (Rust `\`-continuations would strip it).
        let script = [
            "import sys, json",
            "inp = sys.stdin.buffer",
            "out = sys.stdout.buffer",
            "def read_msg():",
            "    line = inp.readline()",
            "    if not line:",
            "        return None",
            "    text = line.decode().strip()",
            "    if text.lower().startswith('content-length'):",
            "        n = int(text.split(':')[1])",
            "        inp.readline()",
            "        return json.loads(inp.read(n).decode())",
            "    return json.loads(text)",
            "def write_msg(obj):",
            "    out.write(json.dumps(obj).encode() + b'\\n')",
            "    out.flush()",
            "while True:",
            "    msg = read_msg()",
            "    if msg is None:",
            "        break",
            "    if isinstance(msg, dict) and msg.get('method') == 'initialize':",
            "        write_msg({'jsonrpc': '2.0', 'id': msg.get('id'), 'result': {",
            "            'protocolVersion': '2025-06-18',",
            "            'serverInfo': {'version': 'fake-1'}}})",
        ]
        .join("\n");
        let policy = Policy::default().allow(Capability::McpEnable);
        let cfg = McpServerConfig::new("fake-server", "python3")
            .with_args(vec!["-c".to_string(), script]);
        let client = McpClient::connect(&cfg, Some(&policy))
            .expect("allowed policy should let the spawn proceed");
        assert_eq!(client.negotiated_version, "2025-06-18");
        assert_eq!(client.server_version.as_deref(), Some("fake-1"));
    }
}
