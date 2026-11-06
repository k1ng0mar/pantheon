//! MCP merged into one file. External paths keep working through the
//! crate-root shims in lib.rs (same pattern as the providers merge).

pub mod bundled {
    //! Thin facade over the canonical bundled MCP catalog.
    //!
    //! The ONE recipe definition lives in
    //! [`pantheon_api::mcp_catalog`] (the leaf both the launcher and the
    //! agent's `enable_mcp` tool depend on - putting it here would force a
    //! `tools -> mcp` dependency cycle). This module re-exports the
    //! canonical type and adds the launcher-side pure helpers: membership
    //! checks and the in-memory enable/disable helpers the tests and the
    //! setup wizard drive. There is no second copy of the recipe data
    //! the old "two copies + drift test" arrangement is gone by
    //! construction. Every recipe is inert until the operator enables it in
    //! `[mcp.servers.<name>]`; nothing here launches anything.
    //!
    //! MCPs vs plugins: a bundled MCP server is an out-of-process
    //! integration - Pantheon spawns or connects to it and speaks the MCP
    //! protocol, never executing the server's code. Bundled MCPs are
    //! first-party: they skip the third-party consent store
    //! ([`pantheon_api::approval`]) but never skip enablement; the
    //! `enabled` flag is their only gate.

    use pantheon_api::config::McpSection;
    pub use pantheon_api::mcp_catalog::BundledMcpServer;
    use std::fmt;

    /// All bundled server recipes, sorted by name. Delegates to the
    /// canonical catalog - the same rows the dashboard and the agent's
    /// `enable_mcp` tool see.
    pub fn bundled_catalog() -> Vec<BundledMcpServer> {
        pantheon_api::mcp_catalog::bundled_catalog()
    }

    /// Look up one bundled recipe by its config table name.
    pub fn find_bundled(name: &str) -> Option<BundledMcpServer> {
        pantheon_api::mcp_catalog::find(name)
    }

    /// True when `name` is one of the bundled recipes.
    pub fn is_bundled(name: &str) -> bool {
        find_bundled(name).is_some()
    }

    /// Error for the bundled enable/disable helpers.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum BundledError {
        /// `name` is not one of the bundled recipes.
        UnknownServer(String),
    }

    impl fmt::Display for BundledError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                BundledError::UnknownServer(name) => {
                    write!(f, "not a bundled MCP server: {name}")
                }
            }
        }
    }

    impl std::error::Error for BundledError {}

    /// Enable or disable a bundled server in the resolved config state.
    /// Pure and total: if the server has no `[mcp.servers.<name>]` entry
    /// yet, the canonical recipe is materialized first (disabled by
    /// default), then the `enabled` flag is flipped to the requested value.
    /// Returns [`BundledError::UnknownServer`] for names outside the
    /// catalog - only catalog names are toggleable, which is what keeps
    /// arbitrary commands off the command line.
    ///
    /// Note: this only mutates the in-memory [`McpSection`]; persisting the
    /// config file is the surfaces' (wizard/dashboard/app) job.
    pub fn set_bundled_enabled(
        section: &mut McpSection,
        name: &str,
        enabled: bool,
    ) -> Result<(), BundledError> {
        let recipe =
            find_bundled(name).ok_or_else(|| BundledError::UnknownServer(name.to_string()))?;
        // Materialize the canonical recipe only when the entry is absent;
        // an existing entry keeps the operator's keys - this flips just the
        // flag.
        let entry = section
            .servers
            .entry(name.to_string())
            .or_insert_with(|| recipe.to_config_entry());
        entry.enabled = enabled;
        Ok(())
    }

    /// Current enablement of a bundled server from the resolved config.
    /// `None` when `name` is not a bundled recipe; a bundled recipe with no
    /// config table is reported as disabled (the catalog default).
    pub fn is_bundled_enabled(section: &McpSection, name: &str) -> Option<bool> {
        find_bundled(name)?;
        Some(section.servers.get(name).is_some_and(|e| e.enabled))
    }
}

pub mod client {
    //! Minimal real MCP client over stdio.
    //!
    //! Protocol subset: `initialize` (with protocol-version negotiation),
    //! `tools/list`, `tools/call`, `resources/list`, `resources/read`,
    //! `resources/templates/list`, `prompts/list`, `prompts/get`. Sampling,
    //! roots, and elicitation remain out of scope.
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
    use crate::{
        classify, result_text, McpConn, McpPrompt, McpPromptResult, McpResource,
        McpResourceContent, McpResourceTemplate, Wire,
    };
    use pantheon_api::capability::{Capability, Policy};
    use pantheon_api::message::ToolSchema;
    use pantheon_exec::sandbox::{build_sandboxed_checked, enforce, Enforcement};
    use serde_json::Value;
    use std::collections::HashMap;
    use std::fmt;
    use std::io::{BufReader, Read, Write};
    use std::process::{Child, ChildStdin, Command, Stdio};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::sync::{Arc, Mutex};
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
        /// Remote authentication is missing, expired, or failed. Carries the
        /// remediation (`pantheon mcp login <server>`); never token material.
        Unauthorized(String),
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
                McpError::Unauthorized(e) => write!(f, "mcp: authentication required: {e}"),
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

    /// Server-initiated elicitation callback. The handler receives the server
    /// name and the raw `elicitation/create` params and returns the JSON-RPC
    /// result (normally `{"action":"accept","content":{...}}`). Returning an
    /// error is reported to the server as an MCP error; the request never
    /// wedges the client's own response pairing.
    pub type ElicitationHandler =
        std::sync::Arc<dyn Fn(&str, &Value) -> Result<Value, McpError> + Send + Sync>;

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
        elicitation: Option<ElicitationHandler>,
        /// Bounded tail of the child's stderr (last 4 KiB), collected on
        /// a reader thread. A crash-on-start server used to surface only
        /// as "transport closed" because stderr went to null; the tail
        /// is attached to transport-closed errors instead.
        stderr_tail: Arc<Mutex<String>>,
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
        pub fn connect(
            config: &McpServerConfig,
            policy: Option<&Policy>,
        ) -> Result<Self, McpError> {
            Self::connect_with(config, policy, None)
        }

        /// [`McpClient::connect`] plus an optional server-initiated
        /// elicitation handler.
        pub fn connect_with(
            config: &McpServerConfig,
            policy: Option<&Policy>,
            elicitation: Option<ElicitationHandler>,
        ) -> Result<Self, McpError> {
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
                // how isolated the child runs. `build_sandboxed_checked` applies
                // the same fail-closed gate as `run_sandboxed`: a boundary that
                // needs a wrapper which cannot initialize refuses to spawn
                // (`SANDBOX_UNAVAILABLE`) unless the direct fallback is opted
                // in - the policy gate above is what decides *whether*, this
                // is what guarantees *how isolated*.
                Some(profile) => build_sandboxed_checked(profile, &config.command, &args, &cwd)
                    .map(|(cmd, _sandboxed)| cmd)
                    .map_err(|e| McpError::Spawn(e.cause))?,
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
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| McpError::Spawn(format!("{}: {e}", config.command)))?;
            let stdin = child.stdin.take().ok_or(McpError::Io("no stdin".into()))?;
            let stdout = child
                .stdout
                .take()
                .ok_or(McpError::Io("no stdout".into()))?;
            let stderr = child
                .stderr
                .take()
                .ok_or(McpError::Io("no stderr".into()))?;
            let stderr_tail: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
            {
                let tail = Arc::clone(&stderr_tail);
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stderr);
                    let mut buf = [0u8; 1024];
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let chunk = String::from_utf8_lossy(&buf[..n]).into_owned();
                                if let Ok(mut t) = tail.lock() {
                                    t.push_str(&chunk);
                                    if t.len() > 4096 {
                                        let cut = t.len() - 4096;
                                        t.drain(..cut);
                                    }
                                }
                            }
                        }
                    }
                });
            }

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
                elicitation,
                stderr_tail,
            };
            client.initialize()?;
            Ok(client)
        }

        /// Current stderr tail, trimmed. Empty when the server said
        /// nothing (or has not flushed yet).
        fn stderr_text(&self) -> String {
            self.stderr_tail
                .lock()
                .map(|t| t.trim().to_string())
                .unwrap_or_default()
        }

        /// A transport-closed error carrying the server's stderr tail
        /// when there is one, so a crash-on-start failure names its
        /// cause instead of only "transport closed". Still a transport
        /// error (`Io`), so the manager's reconnect/retry path treats
        /// it like `Closed`.
        fn closed_err(&self) -> McpError {
            // Error path only: give the stderr thread a moment to
            // drain the pipe the child closed on exit.
            std::thread::sleep(Duration::from_millis(100));
            let tail = self.stderr_text();
            if tail.is_empty() {
                McpError::Closed
            } else {
                McpError::Io(format!("transport closed; server stderr: {tail}"))
            }
        }

        /// Attach the stderr tail to an existing error, when there is one.
        fn with_stderr(&self, e: McpError) -> McpError {
            let tail = self.stderr_text();
            if tail.is_empty() {
                e
            } else {
                McpError::Io(format!("{e}; server stderr: {tail}"))
            }
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
                return Err(self.with_stderr(e));
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
                        return Err(self.closed_err());
                    }
                    Ok(ReadEvent::Eof) => {
                        self.dead = true;
                        return Err(self.closed_err());
                    }
                    Ok(ReadEvent::Error(e)) => {
                        self.kill();
                        return Err(match e {
                            FrameError::Oversized(bytes) => McpError::Oversized { bytes },
                            FrameError::BadJson(msg) => {
                                McpError::Framing(format!("bad JSON: {msg}"))
                            }
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
                        Wire::Request {
                            id: rid,
                            method,
                            params,
                        } => {
                            // MCP elicitation is the one server-to-client call
                            // this host answers. Sampling/roots stay out of
                            // scope and get a clean method-not-found.
                            let response = if method == "elicitation/create" {
                                match self.elicitation.as_ref() {
                                    Some(handler) => match handler(&self.server, &params) {
                                        Ok(result) => serde_json::json!({
                                            "jsonrpc": "2.0", "id": rid, "result": result,
                                        }),
                                        Err(e) => serde_json::json!({
                                            "jsonrpc": "2.0", "id": rid,
                                            "error": {"code": -32603, "message": e.to_string()},
                                        }),
                                    },
                                    None => serde_json::json!({
                                        "jsonrpc": "2.0", "id": rid,
                                        "error": {
                                            "code": -32601,
                                            "message": "elicitation is not supported by this host",
                                        },
                                    }),
                                }
                            } else {
                                serde_json::json!({
                                    "jsonrpc": "2.0", "id": rid,
                                    "error": {"code": -32601, "message": "method not found"},
                                })
                            };
                            let _ = self.send(&response);
                        }
                        _ => {}
                    },
                }
            }
        }

        /// `tools/list`: the tool definitions a later step can register,
        /// following `nextCursor` pages like `list_resources` (a repeated
        /// cursor or 1,000 pages ends the walk defensively).
        pub fn list_tools(&mut self) -> Result<Vec<McpToolDef>, McpError> {
            let mut out = Vec::new();
            let mut cursor: Option<String> = None;
            for _ in 0..1000 {
                let params = match &cursor {
                    Some(c) => serde_json::json!({"cursor": c}),
                    None => serde_json::json!({}),
                };
                let result = self.request("tools/list", params)?;
                let tools = result
                    .get("tools")
                    .and_then(|t| t.as_array())
                    .ok_or_else(|| {
                        McpError::Protocol("tools/list response has no tools array".into())
                    })?;
                for t in tools {
                    let name = t
                        .get("name")
                        .and_then(|n| n.as_str())
                        .ok_or_else(|| McpError::Protocol("tool without a name".into()))?;
                    out.push(McpToolDef {
                        server: self.server.clone(),
                        name: name.to_string(),
                        description: t
                            .get("description")
                            .and_then(|d| d.as_str())
                            .unwrap_or("")
                            .to_string(),
                        input_schema: t.get("inputSchema").cloned().unwrap_or(Value::Null),
                    });
                }
                match crate::next_cursor(&result) {
                    Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
                    _ => return Ok(out),
                }
            }
            Err(McpError::Protocol(
                "tools/list exceeded 1,000 pages".to_string(),
            ))
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

        /// `resources/list`: the server's static resources, following
        /// `nextCursor` pages. A repeated cursor or 1,000 pages ends the walk
        /// defensively.
        pub fn list_resources(&mut self) -> Result<Vec<McpResource>, McpError> {
            let mut out = Vec::new();
            let mut cursor: Option<String> = None;
            for _ in 0..1000 {
                let params = match &cursor {
                    Some(c) => serde_json::json!({"cursor": c}),
                    None => serde_json::json!({}),
                };
                let result = self.request("resources/list", params)?;
                out.extend(crate::resources_from(&result)?);
                match crate::next_cursor(&result) {
                    Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
                    _ => return Ok(out),
                }
            }
            Err(McpError::Protocol(
                "resources/list exceeded 1,000 pages".to_string(),
            ))
        }

        /// `resources/templates/list`: parameterized resource templates,
        /// following `nextCursor` pages.
        pub fn list_resource_templates(&mut self) -> Result<Vec<McpResourceTemplate>, McpError> {
            let mut out = Vec::new();
            let mut cursor: Option<String> = None;
            for _ in 0..1000 {
                let params = match &cursor {
                    Some(c) => serde_json::json!({"cursor": c}),
                    None => serde_json::json!({}),
                };
                let result = self.request("resources/templates/list", params)?;
                out.extend(crate::resource_templates_from(&result)?);
                match crate::next_cursor(&result) {
                    Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
                    _ => return Ok(out),
                }
            }
            Err(McpError::Protocol(
                "resources/templates/list exceeded 1,000 pages".to_string(),
            ))
        }

        /// `resources/read`: fetch one resource by URI. MCP permits a single
        /// read to return multiple content blocks, so this returns a vector.
        pub fn read_resource(&mut self, uri: &str) -> Result<Vec<McpResourceContent>, McpError> {
            let result = self.request("resources/read", serde_json::json!({"uri": uri}))?;
            crate::resource_contents_from(&result)
        }

        /// `prompts/list`: the server's reusable prompt templates, following
        /// `nextCursor` pages.
        pub fn list_prompts(&mut self) -> Result<Vec<McpPrompt>, McpError> {
            let mut out = Vec::new();
            let mut cursor: Option<String> = None;
            for _ in 0..1000 {
                let params = match &cursor {
                    Some(c) => serde_json::json!({"cursor": c}),
                    None => serde_json::json!({}),
                };
                let result = self.request("prompts/list", params)?;
                out.extend(crate::prompts_from(&result)?);
                match crate::next_cursor(&result) {
                    Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
                    _ => return Ok(out),
                }
            }
            Err(McpError::Protocol(
                "prompts/list exceeded 1,000 pages".to_string(),
            ))
        }

        /// `prompts/get`: expand one prompt with the caller's arguments.
        pub fn get_prompt(
            &mut self,
            name: &str,
            args: &Value,
        ) -> Result<McpPromptResult, McpError> {
            let result = self.request(
                "prompts/get",
                serde_json::json!({"name": name, "arguments": args}),
            )?;
            crate::prompt_result_from(&result)
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

        fn list_resources(&mut self) -> Result<Vec<McpResource>, McpError> {
            McpClient::list_resources(self)
        }

        fn list_resource_templates(&mut self) -> Result<Vec<McpResourceTemplate>, McpError> {
            McpClient::list_resource_templates(self)
        }

        fn read_resource(&mut self, uri: &str) -> Result<Vec<McpResourceContent>, McpError> {
            McpClient::read_resource(self, uri)
        }

        fn list_prompts(&mut self) -> Result<Vec<McpPrompt>, McpError> {
            McpClient::list_prompts(self)
        }

        fn get_prompt(&mut self, name: &str, args: &Value) -> Result<McpPromptResult, McpError> {
            McpClient::get_prompt(self, name, args)
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

        /// End-to-end resource/prompt request pairing over stdio: the server
        /// answers each method, and the client parses the wire shapes into
        /// the manager-facing types.
        #[test]
        fn resources_and_prompts_round_trip() {
            // No policy = direct spawn, like an explicitly un-gated embedder;
            // this test is about request pairing, not the sandbox.
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
            "    if not isinstance(msg, dict):",
            "        continue",
            "    method = msg.get('method')",
            "    result = None",
            "    if method == 'initialize':",
            "        result = {'protocolVersion': '2025-06-18', 'serverInfo': {'version': 'res-1'}}",
            "    elif method == 'resources/list':",
            "        if msg.get('params', {}).get('cursor') == 'p2':",
            "            result = {'resources': [{'uri': 'file:///tmp/y', 'name': 'y'}]}",
            "        else:",
            "            result = {'resources': [{'uri': 'file:///tmp/x', 'name': 'x', 'mimeType': 'text/plain'}], 'nextCursor': 'p2'}",
            "    elif method == 'resources/templates/list':",
            "        result = {'resourceTemplates': [{'uriTemplate': 'file:///{path}', 'name': 'files'}]}",
            "    elif method == 'resources/read':",
            "        result = {'contents': [{'uri': msg['params']['uri'], 'mimeType': 'text/plain', 'text': 'hello'}]}",
            "    elif method == 'prompts/list':",
            "        result = {'prompts': [{'name': 'review', 'arguments': [{'name': 'diff', 'required': True}]}]}",
            "    elif method == 'prompts/get':",
            "        result = {'description': 'review', 'messages': [{'role': 'user', 'content': {'type': 'text', 'text': msg['params']['arguments']['diff']}}]}",
            "    if result is not None and 'id' in msg:",
            "        write_msg({'jsonrpc': '2.0', 'id': msg['id'], 'result': result})",
        ]
        .join("\n");
            let cfg = McpServerConfig::new("fake-resources", "python3")
                .with_args(vec!["-c".to_string(), script]);
            let mut client = McpClient::connect(&cfg, None).expect("connect fake server");

            let resources = client.list_resources().expect("resources/list");
            assert_eq!(resources.len(), 2);
            assert_eq!(resources[0].uri, "file:///tmp/x");
            assert_eq!(resources[0].mime_type.as_deref(), Some("text/plain"));
            assert_eq!(resources[1].uri, "file:///tmp/y");

            let templates = client.list_resource_templates().expect("templates/list");
            assert_eq!(templates[0].uri_template, "file:///{path}");

            let contents = client
                .read_resource("file:///tmp/x")
                .expect("resources/read");
            assert_eq!(contents[0].text.as_deref(), Some("hello"));

            let prompts = client.list_prompts().expect("prompts/list");
            assert!(prompts[0].arguments[0].required);

            let prompt = client
                .get_prompt("review", &serde_json::json!({"diff": "a b"}))
                .expect("prompts/get");
            assert_eq!(prompt.messages.len(), 1);
            assert_eq!(
                prompt.messages[0]
                    .content
                    .get("text")
                    .and_then(|v| v.as_str()),
                Some("a b")
            );
        }

        /// A crash-on-start server surfaces its stderr, not just
        /// "transport closed".
        #[test]
        fn crash_on_start_surfaces_stderr() {
            let script =
            "import sys; sys.stderr.write('boom: missing lib\n'); sys.stderr.flush(); sys.exit(1)";
            let cfg = McpServerConfig::new("fake-crash", "python3")
                .with_args(vec!["-c".to_string(), script.to_string()]);
            let err = match McpClient::connect(&cfg, None) {
                Ok(_) => panic!("crashing server must fail connect"),
                Err(e) => e,
            };
            assert!(
                err.to_string().contains("boom: missing lib"),
                "stderr tail missing in error: {err}"
            );
        }

        /// `tools/list` follows `nextCursor`: tools past page 1 must surface.
        #[test]
        fn tools_list_follows_next_cursor() {
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
            "    if not isinstance(msg, dict):",
            "        continue",
            "    method = msg.get('method')",
            "    result = None",
            "    if method == 'initialize':",
            "        result = {'protocolVersion': '2025-06-18', 'serverInfo': {'version': 'tools-1'}}",
            "    elif method == 'tools/list':",
            "        if msg.get('params', {}).get('cursor') == 'p2':",
            "            result = {'tools': [{'name': 'second', 'inputSchema': {'type': 'object'}}]}",
            "        else:",
            "            result = {'tools': [{'name': 'first', 'inputSchema': {'type': 'object'}}], 'nextCursor': 'p2'}",
            "    if result is not None and 'id' in msg:",
            "        write_msg({'jsonrpc': '2.0', 'id': msg['id'], 'result': result})",
        ]
        .join("\n");
            let cfg = McpServerConfig::new("fake-tools", "python3")
                .with_args(vec!["-c".to_string(), script]);
            let mut client = McpClient::connect(&cfg, None).expect("connect fake server");
            let tools = client.list_tools().expect("tools/list");
            assert_eq!(
                tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
                vec!["first", "second"]
            );
        }
    }
}

pub mod framed {
    //! Stdio framing for JSON-RPC 2.0 messages.
    //!
    //! The MCP stdio transport delimits messages with newlines; some servers
    //! instead send `Content-Length` headers (LSP style). The reader accepts
    //! both, like the ACP backend does. The writer always sends newline-
    //! delimited JSON, which is what the MCP spec describes for stdio.

    use serde_json::Value;
    use std::io::BufRead;

    /// Cap on the header block of a `Content-Length`-framed message.
    const MAX_HEADER_BYTES: usize = 64 * 1024;

    /// What went wrong reading one framed message.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum FrameError {
        /// `Content-Length` header missing, unparseable, or (with `usize`) over
        /// the configured message cap.
        BadHeader(String),
        /// The framed body is not valid JSON.
        BadJson(String),
        /// A newline-delimited line (or declared body) exceeds the cap.
        Oversized(usize),
        /// Underlying I/O failure.
        Io(String),
    }

    /// Read one message from `reader`: `Content-Length`-framed, or a bare JSON
    /// line. Returns `Ok(None)` on clean EOF (no bytes before close).
    /// Every byte counted against `max_bytes`; anything over is rejected.
    pub fn read_message<R: BufRead>(
        reader: &mut R,
        max_bytes: usize,
    ) -> Result<Option<Value>, FrameError> {
        // Skip blank lines: servers emit them, and a blank line is not a message.
        // Iterative, so a flood of blank lines cannot grow the stack.
        let first = loop {
            let line = match read_line_capped(reader, max_bytes)? {
                None => return Ok(None),
                Some(l) => l,
            };
            let t = line.trim().to_string();
            if !t.is_empty() {
                break t;
            }
        };
        // Header names are case-insensitive (a server sending
        // `content-length:` is speaking the same framing).
        let lower = first.to_ascii_lowercase();
        if lower.starts_with("content-length:") {
            let rest = &first["Content-Length:".len()..];
            let len: usize = rest
                .trim()
                .parse()
                .map_err(|_| FrameError::BadHeader(format!("bad Content-Length {rest:?}")))?;
            if len > max_bytes {
                return Err(FrameError::Oversized(len));
            }
            // Consume the blank line separating headers from the body,
            // and verify it actually is blank: a non-blank line here is
            // a second header (or garbage), not the separator, and
            // reading the body from the wrong offset corrupts the stream.
            let mut blank = String::new();
            read_line_into(reader, &mut blank, MAX_HEADER_BYTES)?;
            if !blank.trim().is_empty() {
                return Err(FrameError::BadHeader(format!(
                    "expected blank line after Content-Length header, got {blank:?}"
                )));
            }
            let mut buf = vec![0u8; len];
            reader
                .read_exact(&mut buf)
                .map_err(|e| FrameError::Io(format!("short body: {e}")))?;
            let v: Value =
                serde_json::from_slice(&buf).map_err(|e| FrameError::BadJson(e.to_string()))?;
            return Ok(Some(v));
        }
        // Bare JSON line.
        let v: Value =
            serde_json::from_str(&first).map_err(|e| FrameError::BadJson(e.to_string()))?;
        Ok(Some(v))
    }

    /// Read one `\n`-terminated line, consuming exactly up to and including the
    /// newline - never over-reading into the next frame. Rejects anything over
    /// `cap` bytes. Returns `Ok(None)` only on EOF with no bytes read.
    fn read_line_capped<R: BufRead>(
        reader: &mut R,
        cap: usize,
    ) -> Result<Option<String>, FrameError> {
        let mut buf = Vec::new();
        loop {
            let chunk = reader
                .fill_buf()
                .map_err(|e| FrameError::Io(e.to_string()))?;
            if chunk.is_empty() {
                if buf.is_empty() {
                    return Ok(None);
                }
                break;
            }
            match chunk.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    let take = i + 1;
                    if buf.len() + take > cap {
                        return Err(FrameError::Oversized(buf.len() + take));
                    }
                    buf.extend_from_slice(&chunk[..take]);
                    reader.consume(take);
                    break;
                }
                None => {
                    if buf.len() + chunk.len() > cap {
                        return Err(FrameError::Oversized(buf.len() + chunk.len()));
                    }
                    let n = chunk.len();
                    buf.extend_from_slice(chunk);
                    reader.consume(n);
                }
            }
        }
        let s =
            String::from_utf8(buf).map_err(|e| FrameError::BadJson(format!("not UTF-8: {e}")))?;
        Ok(Some(s))
    }

    /// Read one line into `out` (used for the short header block).
    fn read_line_into<R: BufRead>(
        reader: &mut R,
        out: &mut String,
        cap: usize,
    ) -> Result<(), FrameError> {
        match read_line_capped(reader, cap)? {
            Some(l) => {
                *out = l;
                Ok(())
            }
            None => Err(FrameError::Io("EOF inside header block".into())),
        }
    }

    /// Encode one message as newline-delimited JSON for the wire.
    pub fn encode(body: &Value, max_bytes: usize) -> Result<Vec<u8>, FrameError> {
        let mut bytes = serde_json::to_vec(body).map_err(|e| FrameError::BadJson(e.to_string()))?;
        if bytes.len() + 1 > max_bytes {
            return Err(FrameError::Oversized(bytes.len() + 1));
        }
        bytes.push(b'\n');
        Ok(bytes)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::json;
        use std::io::{BufReader, Cursor};

        /// A `BufRead` adapter that exposes at most `chunk` bytes through
        /// `fill_buf`, simulating a slow pipe that splits a frame across many
        /// reads.
        struct Chunked<R: BufRead> {
            inner: R,
            chunk: usize,
        }

        impl<R: BufRead> std::io::Read for Chunked<R> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.inner.read(buf)
            }
        }

        impl<R: BufRead> BufRead for Chunked<R> {
            fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
                let buf = self.inner.fill_buf()?;
                let n = buf.len().min(self.chunk);
                Ok(&buf[..n])
            }

            fn consume(&mut self, amt: usize) {
                self.inner.consume(amt);
            }
        }

        #[test]
        fn encode_decode_round_trip() {
            let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
            let bytes = encode(&msg, 1024).unwrap();
            assert!(bytes.ends_with(b"\n"));
            let mut r = BufReader::new(Cursor::new(bytes));
            assert_eq!(read_message(&mut r, 1024).unwrap(), Some(msg));
            // Clean EOF after the frame: no bytes before close -> Ok(None).
            assert_eq!(read_message(&mut r, 1024).unwrap(), None);
        }

        #[test]
        fn blank_lines_are_skipped_not_messages() {
            let mut r = BufReader::new(Cursor::new(b"\n\n  \n{\"id\": 9}\n".to_vec()));
            assert_eq!(read_message(&mut r, 1024).unwrap(), Some(json!({"id": 9})));
        }

        #[test]
        fn clean_eof_is_not_an_error() {
            let mut r = BufReader::new(Cursor::new(Vec::new()));
            assert_eq!(read_message(&mut r, 1024).unwrap(), None);
        }

        #[test]
        fn partial_reads_reassemble_a_split_frame() {
            let msg = json!({"jsonrpc": "2.0", "id": 7, "result": {"ok": true}});
            let bytes = encode(&msg, 4096).unwrap();
            // Three bytes per fill_buf: the frame arrives in many fragments.
            let mut r = Chunked {
                inner: BufReader::new(Cursor::new(bytes)),
                chunk: 3,
            };
            assert_eq!(read_message(&mut r, 4096).unwrap(), Some(msg));
        }

        #[test]
        fn multiple_frames_in_one_buffer_decode_in_order() {
            let a = json!({"id": 1});
            let b = json!({"id": 2});
            let mut bytes = encode(&a, 1024).unwrap();
            bytes.extend(encode(&b, 1024).unwrap());
            let mut r = BufReader::new(Cursor::new(bytes));
            assert_eq!(read_message(&mut r, 1024).unwrap(), Some(a));
            assert_eq!(read_message(&mut r, 1024).unwrap(), Some(b));
            assert_eq!(read_message(&mut r, 1024).unwrap(), None);
        }

        #[test]
        fn content_length_framed_message_round_trips() {
            let msg = json!({"jsonrpc": "2.0", "id": 3, "result": "pong"});
            let body = serde_json::to_vec(&msg).unwrap();
            let mut wire = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
            wire.extend(body);
            let mut r = BufReader::new(Cursor::new(wire));
            assert_eq!(read_message(&mut r, 4096).unwrap(), Some(msg));
        }

        #[test]
        fn malformed_json_line_is_bad_json() {
            let mut r = BufReader::new(Cursor::new(b"{not json}\n".to_vec()));
            assert!(matches!(
                read_message(&mut r, 1024),
                Err(FrameError::BadJson(_))
            ));
        }

        #[test]
        fn non_numeric_content_length_is_bad_header() {
            let mut r = BufReader::new(Cursor::new(b"Content-Length: abc\n\n{}\n".to_vec()));
            assert!(matches!(
                read_message(&mut r, 1024),
                Err(FrameError::BadHeader(_))
            ));
        }

        #[test]
        fn truncated_body_is_an_io_error() {
            // Declares 100 bytes, delivers 10.
            let mut wire = b"Content-Length: 100\n\n".to_vec();
            wire.extend(b"{\"id\":1}");
            let mut r = BufReader::new(Cursor::new(wire));
            assert!(matches!(read_message(&mut r, 1024), Err(FrameError::Io(_))));
        }

        #[test]
        fn oversized_line_is_rejected() {
            let big = "x".repeat(200);
            let mut r = BufReader::new(Cursor::new(format!("{big}\n").into_bytes()));
            assert!(matches!(
                read_message(&mut r, 64),
                Err(FrameError::Oversized(_))
            ));
        }

        #[test]
        fn content_length_over_the_cap_is_rejected() {
            let mut r = BufReader::new(Cursor::new(b"Content-Length: 99999\n\n".to_vec()));
            assert!(matches!(
                read_message(&mut r, 64),
                Err(FrameError::Oversized(_))
            ));
        }

        #[test]
        fn lowercase_content_length_header_is_accepted() {
            let msg = json!({"jsonrpc": "2.0", "id": 4, "result": "pong"});
            let body = serde_json::to_vec(&msg).unwrap();
            let mut wire = format!("content-length: {}\r\n\r\n", body.len()).into_bytes();
            wire.extend(body);
            let mut r = BufReader::new(Cursor::new(wire));
            assert_eq!(read_message(&mut r, 4096).unwrap(), Some(msg));
        }

        #[test]
        fn non_blank_separator_line_is_bad_header() {
            let mut wire = b"Content-Length: 7\nContent-Type: application/json\n\n".to_vec();
            wire.extend(b"{\"id\":1}");
            let mut r = BufReader::new(Cursor::new(wire));
            assert!(matches!(
                read_message(&mut r, 4096),
                Err(FrameError::BadHeader(_))
            ));
        }

        #[test]
        fn encode_over_the_cap_is_rejected() {
            let msg = json!({"data": "x".repeat(200)});
            assert!(matches!(encode(&msg, 64), Err(FrameError::Oversized(_))));
        }
    }
}

pub mod http {
    //! MCP over HTTP: the two remote transports, sharing the [`McpConn`]
    //! pairing rules with the stdio client.
    //!
    //! * `http` - streamable HTTP (2025-03-26): plain JSON-RPC POSTs to one
    //!   endpoint. A response is a single JSON object, or an SSE stream when
    //!   the server batches. A `Mcp-Session-Id` response header, when present,
    //!   is echoed back on later requests.
    //! * `sse` - legacy SSE (2024-11-05): one long-lived GET event stream,
    //!   plus a per-session POST endpoint the server announces in an
    //!   `endpoint` event. Responses to POSTs arrive as `message` events on
    //!   the stream.
    //!
    //! Sync, via ureq (the same crate the provider layer uses). For SSE the
    //! stream is read on a background thread and fanned into an mpsc channel,
    //! mirroring the stdio client's reader thread; `request` pairs by id with
    //! a deadline. Server-initiated requests on the SSE stream are ignored
    //! they cannot wedge the pairing because responses are matched by id.
    //!
    //! Shutdown note: the SSE reader thread blocks on the socket, so after
    //! `shutdown` it can linger until the stream agent's overall timeout (one
    //! hour) or process exit. Request pairing is unaffected.

    use crate::client::ElicitationHandler;
    use crate::{
        classify, McpConn, McpError, McpPrompt, McpPromptResult, McpResource, McpResourceContent,
        McpResourceTemplate, McpToolDef, Wire, MCP_PROTOCOL_VERSION, SUPPORTED_PROTOCOL_VERSIONS,
    };
    use serde_json::Value;
    use std::io::BufRead;
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::{Duration, Instant};

    /// Which remote transport to speak.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum HttpTransport {
        /// Legacy SSE (2024-11-05).
        Sse,
        /// Streamable HTTP (2025-03-26 and later).
        Streamable,
    }

    /// Overall ceiling for the long-lived SSE GET stream: one budget covering
    /// connect through every stream read (ureq's global-timeout scope). When
    /// it elapses the reader thread reports a stream error and the manager
    /// reconnects transparently.
    const SSE_STREAM_TIMEOUT: Duration = Duration::from_secs(3600);

    /// One parsed SSE event.
    struct SseEvent {
        event: String,
        data: String,
    }

    /// Read one SSE event from a buffered stream. `Ok(None)` is clean EOF.
    fn next_sse_event<R: BufRead>(r: &mut R) -> Result<Option<SseEvent>, String> {
        let mut event = String::new();
        let mut data: Vec<String> = Vec::new();
        loop {
            let mut line = String::new();
            match r.read_line(&mut line) {
                Ok(0) => {
                    if event.is_empty() && data.is_empty() {
                        return Ok(None);
                    }
                    break;
                }
                Ok(_) => {}
                Err(e) => return Err(e.to_string()),
            }
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                break; // blank line dispatches the event
            }
            if line.starts_with(':') {
                continue; // comment / heartbeat
            }
            let (field, value) = match line.find(':') {
                Some(i) => (&line[..i], line[i + 1..].trim_start()),
                None => (line, ""),
            };
            match field {
                "event" => event = value.to_string(),
                "data" => data.push(value.to_string()),
                _ => {}
            }
        }
        if event.is_empty() && data.is_empty() {
            return Ok(None);
        }
        Ok(Some(SseEvent {
            event,
            data: data.join("\n"),
        }))
    }

    /// Resolve the `endpoint` event payload against the SSE base URL. The
    /// spec allows a relative path; absolutize against the base's origin.
    fn resolve_endpoint(base: &str, endpoint: &str) -> String {
        if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
            return endpoint.to_string();
        }
        let origin = base.split('/').take(3).collect::<Vec<_>>().join("/");
        if let Some(path) = endpoint.strip_prefix('/') {
            format!("{origin}/{path}")
        } else {
            format!("{origin}/{endpoint}")
        }
    }

    /// ureq agent with one overall timeout (connect through body read, the
    /// scope of ureq's `timeout_global`, matching the ureq 2 `.timeout()`
    /// budget). HTTP statuses surface as responses, not errors, so call sites
    /// read headers/bodies and map statuses themselves.
    fn http_agent(timeout: Duration) -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .max_redirects(5)
            .http_status_as_error(false)
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .new_agent()
    }

    fn ureq_err(e: ureq::Error, method: &str) -> McpError {
        if is_timeout_error(&e) {
            McpError::Timeout {
                method: method.to_string(),
            }
        } else {
            McpError::Io(format!("http: {e}"))
        }
    }

    /// ureq 3 reports timeouts as `Error::Timeout`. Errors funneled through
    /// an `io::Error` keep the `TimedOut` kind, so cover both.
    fn is_timeout_error(e: &ureq::Error) -> bool {
        match e {
            ureq::Error::Timeout(_) => true,
            ureq::Error::Io(ioe) => ioe.kind() == std::io::ErrorKind::TimedOut,
            _ => false,
        }
    }

    /// Map an HTTP status >= 400 to the RPC error the ureq 2 status-error
    /// path produced. The reason phrase stands in for ureq 2's status-line
    /// text; unknown codes render an empty reason.
    fn status_err(resp: &ureq::http::Response<ureq::Body>) -> McpError {
        let code = resp.status().as_u16();
        McpError::Rpc {
            code: i64::from(code),
            message: format!(
                "http {code}: {}",
                resp.status().canonical_reason().unwrap_or("")
            ),
        }
    }

    /// `Body::read_json` fails with `ureq::Error`.
    fn json_err(e: ureq::Error, method: &str) -> McpError {
        if is_timeout_error(&e) {
            McpError::Timeout {
                method: method.to_string(),
            }
        } else {
            McpError::Framing(format!("bad json response: {e}"))
        }
    }

    /// Messages from the SSE reader thread.
    enum StreamMsg {
        Endpoint(String),
        Message(Value),
        Eof,
        Error(String),
    }

    /// Build the JSON-RPC response to one server-initiated request. Only
    /// `elicitation/create` is implemented; everything else is a clean
    /// method-not-found so a misbehaving server cannot wedge a request.
    fn server_request_response(
        handler: Option<&ElicitationHandler>,
        server: &str,
        id: &Value,
        method: &str,
        params: &Value,
    ) -> Value {
        if method != "elicitation/create" {
            return serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": "method not found"},
            });
        }
        match handler {
            Some(handler) => match handler(server, params) {
                Ok(result) => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}),
                Err(e) => serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32603, "message": e.to_string()},
                }),
            },
            None => serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "error": {"code": -32601, "message": "elicitation is not supported by this host"},
            }),
        }
    }

    /// An MCP client over SSE or streamable HTTP. Not `Sync`: requests are
    /// issued one at a time from the owning thread. `Send` so the manager can
    /// hold it behind a mutex.
    pub struct HttpMcpClient {
        server: String,
        transport: HttpTransport,
        agent: ureq::Agent,
        post_url: String,
        session_id: Option<String>,
        sse_rx: Option<mpsc::Receiver<StreamMsg>>,
        next_id: u64,
        timeout: Duration,
        /// Extra headers sent with every POST (auth for remote endpoints).
        headers: Vec<(String, String)>,
        /// Protocol version the server answered in `initialize`.
        pub negotiated_version: String,
        /// The server's self-reported version, when it sends one.
        pub server_version: Option<String>,
        dead: bool,
        elicitation: Option<ElicitationHandler>,
    }

    impl HttpMcpClient {
        /// Connect and run the `initialize` handshake.
        ///
        /// `url` is the SSE stream URL for [`HttpTransport::Sse`] and the
        /// JSON-RPC endpoint for [`HttpTransport::Streamable`].
        pub fn connect(
            server: &str,
            transport: HttpTransport,
            url: &str,
            timeout: Duration,
            headers: &[(String, String)],
        ) -> Result<Self, McpError> {
            Self::connect_with(server, transport, url, timeout, headers, None)
        }

        /// [`HttpMcpClient::connect`] plus an optional server-initiated
        /// elicitation handler.
        pub fn connect_with(
            server: &str,
            transport: HttpTransport,
            url: &str,
            timeout: Duration,
            headers: &[(String, String)],
            elicitation: Option<ElicitationHandler>,
        ) -> Result<Self, McpError> {
            let agent = http_agent(timeout);
            let (post_url, sse_rx) = match transport {
                HttpTransport::Streamable => (url.to_string(), None),
                HttpTransport::Sse => {
                    let stream_agent = http_agent(SSE_STREAM_TIMEOUT);
                    let resp = stream_agent
                        .get(url)
                        .header("Accept", "text/event-stream")
                        .call()
                        .map_err(|e| ureq_err(e, "sse connect"))?;
                    if resp.status().as_u16() >= 400 {
                        return Err(status_err(&resp));
                    }
                    let (tx, rx) = mpsc::channel();
                    std::thread::spawn(move || {
                        let mut reader = std::io::BufReader::new(resp.into_body().into_reader());
                        loop {
                            match next_sse_event(&mut reader) {
                                Ok(None) => {
                                    let _ = tx.send(StreamMsg::Eof);
                                    break;
                                }
                                Ok(Some(ev)) => {
                                    let msg = match ev.event.as_str() {
                                        "endpoint" => Some(StreamMsg::Endpoint(ev.data)),
                                        "message" => {
                                            match serde_json::from_str::<Value>(&ev.data) {
                                                Ok(v) => Some(StreamMsg::Message(v)),
                                                Err(e) => Some(StreamMsg::Error(format!(
                                                    "bad sse message json: {e}"
                                                ))),
                                            }
                                        }
                                        _ => None,
                                    };
                                    if let Some(m) = msg {
                                        if tx.send(m).is_err() {
                                            break;
                                        }
                                    }
                                }
                                Err(e) => {
                                    let _ = tx.send(StreamMsg::Error(e));
                                    break;
                                }
                            }
                        }
                    });
                    // The first thing a well-behaved server sends is the
                    // endpoint event. Wait for it with the request timeout.
                    let deadline = Instant::now() + timeout;
                    let endpoint = loop {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        match rx.recv_timeout(remaining) {
                            Ok(StreamMsg::Endpoint(u)) => break u,
                            Ok(StreamMsg::Message(_)) => continue,
                            Ok(StreamMsg::Eof) => {
                                return Err(McpError::Closed);
                            }
                            Ok(StreamMsg::Error(e)) => {
                                return Err(McpError::Framing(format!("sse stream: {e}")));
                            }
                            Err(RecvTimeoutError::Timeout) => {
                                return Err(McpError::Timeout {
                                    method: "sse endpoint".to_string(),
                                });
                            }
                            Err(RecvTimeoutError::Disconnected) => return Err(McpError::Closed),
                        }
                    };
                    (resolve_endpoint(url, &endpoint), Some(rx))
                }
            };
            let mut client = Self {
                server: server.to_string(),
                transport,
                agent,
                post_url,
                session_id: None,
                sse_rx,
                next_id: 1,
                timeout,
                headers: headers.to_vec(),
                negotiated_version: String::new(),
                server_version: None,
                dead: false,
                elicitation,
            };
            client.initialize()?;
            Ok(client)
        }

        /// POST one JSON-RPC message. Returns the raw response for callers
        /// that parse it themselves (streamable responses).
        fn post(&self, body: &Value) -> Result<ureq::http::Response<ureq::Body>, McpError> {
            let body_str = serde_json::to_string(body)
                .map_err(|e| McpError::Framing(format!("encode: {e}")))?;
            let mut req = self
                .agent
                .post(&self.post_url)
                .header("Content-Type", "application/json")
                .header("Accept", "application/json, text/event-stream");
            for (k, v) in &self.headers {
                req = req.header(k.as_str(), v.as_str());
            }
            if let Some(sid) = &self.session_id {
                req = req.header("Mcp-Session-Id", sid);
            }
            let resp = req
                .send(body_str.as_str())
                .map_err(|e| ureq_err(e, "post"))?;
            if resp.status().as_u16() >= 400 {
                return Err(status_err(&resp));
            }
            Ok(resp)
        }

        /// Fire-and-forget notification (no id, no response expected).
        fn notify(&mut self, method: &str, params: Value) -> Result<(), McpError> {
            if self.dead {
                return Err(McpError::Closed);
            }
            let body = serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params});
            // A 202 with an empty body is the happy path; any 2xx is fine.
            // Drain the body so the connection can be reused.
            match self.post(&body) {
                Ok(resp) => {
                    let _ = resp.into_body().read_to_string();
                    Ok(())
                }
                Err(e) => {
                    self.dead = true;
                    Err(e)
                }
            }
        }

        fn initialize(&mut self) -> Result<(), McpError> {
            let result = self.request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "pantheon", "version": env!("CARGO_PKG_VERSION")},
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
            // Mandatory initialized notification; a failure here kills the
            // client like the stdio path does.
            if let Err(e) = self.notify("notifications/initialized", serde_json::json!({})) {
                self.dead = true;
                return Err(e);
            }
            Ok(())
        }

        /// Send one request and wait for the response with the matching id.
        fn request(&mut self, method: &str, params: Value) -> Result<Value, McpError> {
            if self.dead {
                return Err(McpError::Closed);
            }
            let id = self.next_id;
            self.next_id += 1;
            let want = Value::from(id);
            let body = serde_json::json!({
                "jsonrpc": "2.0", "id": id, "method": method, "params": params,
            });
            match self.transport {
                HttpTransport::Streamable => self.request_streamable(&body, &want, method),
                HttpTransport::Sse => self.request_sse(&body, &want, method),
            }
        }

        fn request_streamable(
            &mut self,
            body: &Value,
            want: &Value,
            method: &str,
        ) -> Result<Value, McpError> {
            let resp = self.post(body).inspect_err(|_| self.dead = true)?;
            if self.session_id.is_none() {
                if let Some(sid) = resp
                    .headers()
                    .get("Mcp-Session-Id")
                    .and_then(|v| v.to_str().ok())
                {
                    self.session_id = Some(sid.to_string());
                }
            }
            let is_sse = resp
                .headers()
                .get("Content-Type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|ct| ct.contains("text/event-stream"));
            if !is_sse {
                let v: Value = resp.into_body().read_json().map_err(|e| {
                    self.dead = true;
                    json_err(e, method)
                })?;
                return match_response(&v, want);
            }
            // SSE-batched response: scan events for our id. The agent timeout
            // bounds the whole read; a timeout surfaces as a read error.
            let mut reader = std::io::BufReader::new(resp.into_body().into_reader());
            loop {
                match next_sse_event(&mut reader) {
                    Ok(None) => {
                        self.dead = true;
                        return Err(McpError::Closed);
                    }
                    Ok(Some(ev)) => {
                        if ev.event != "message" {
                            continue;
                        }
                        let v: Value = serde_json::from_str(&ev.data)
                            .map_err(|e| McpError::Framing(format!("bad sse message json: {e}")))?;
                        match classify(&v) {
                            Wire::Response {
                                id: rid,
                                result,
                                error,
                            } if rid == *want => {
                                if let Some((code, message)) = error {
                                    return Err(McpError::Rpc { code, message });
                                }
                                return Ok(result.unwrap_or(Value::Null));
                            }
                            Wire::Request {
                                id: rid,
                                method: smethod,
                                params,
                            } => {
                                let response = server_request_response(
                                    self.elicitation.as_ref(),
                                    &self.server,
                                    &rid,
                                    &smethod,
                                    &params,
                                );
                                if let Err(e) = self.post(&response) {
                                    self.dead = true;
                                    return Err(e);
                                }
                                continue;
                            }
                            _ => continue,
                        }
                    }
                    Err(e) => {
                        self.dead = true;
                        return Err(match_stream_err(&e, method));
                    }
                }
            }
        }

        fn request_sse(
            &mut self,
            body: &Value,
            want: &Value,
            method: &str,
        ) -> Result<Value, McpError> {
            // The POST itself carries no response body on this transport; the
            // answer arrives on the GET stream.
            match self.post(body) {
                Ok(resp) => {
                    let _ = resp.into_body().read_to_string();
                }
                Err(e) => {
                    self.dead = true;
                    return Err(e);
                }
            }
            let rx = self.sse_rx.as_ref().ok_or(McpError::Closed)?;
            let deadline = Instant::now() + self.timeout;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match rx.recv_timeout(remaining) {
                    Err(RecvTimeoutError::Timeout) => {
                        self.dead = true;
                        return Err(McpError::Timeout {
                            method: method.to_string(),
                        });
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        self.dead = true;
                        return Err(McpError::Closed);
                    }
                    Ok(StreamMsg::Eof) => {
                        self.dead = true;
                        return Err(McpError::Closed);
                    }
                    Ok(StreamMsg::Error(e)) => {
                        self.dead = true;
                        return Err(McpError::Framing(format!("sse stream: {e}")));
                    }
                    Ok(StreamMsg::Endpoint(_)) => continue,
                    Ok(StreamMsg::Message(v)) => match classify(&v) {
                        Wire::Response {
                            id: rid,
                            result,
                            error,
                        } if rid == *want => {
                            if let Some((code, message)) = error {
                                return Err(McpError::Rpc { code, message });
                            }
                            return Ok(result.unwrap_or(Value::Null));
                        }
                        Wire::Request {
                            id: rid,
                            method: smethod,
                            params,
                        } => {
                            let response = server_request_response(
                                self.elicitation.as_ref(),
                                &self.server,
                                &rid,
                                &smethod,
                                &params,
                            );
                            self.post(&response)?;
                            continue;
                        }
                        // Anything else is not our answer.
                        _ => continue,
                    },
                }
            }
        }

        /// `tools/list`, following `nextCursor` pages. A repeated cursor or
        /// 1,000 pages ends the walk defensively.
        pub fn list_tools(&mut self) -> Result<Vec<McpToolDef>, McpError> {
            let mut out = Vec::new();
            let mut cursor: Option<String> = None;
            for _ in 0..1000 {
                let params = match &cursor {
                    Some(c) => serde_json::json!({"cursor": c}),
                    None => serde_json::json!({}),
                };
                let result = self.request("tools/list", params)?;
                out.extend(tools_from(&result, &self.server)?);
                match crate::next_cursor(&result) {
                    Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
                    _ => return Ok(out),
                }
            }
            Err(McpError::Protocol(
                "tools/list exceeded 1,000 pages".to_string(),
            ))
        }

        /// `tools/call`. No capability gate on this type: the manager holds
        /// the single gate at the `ToolRegistry` boundary, like the stdio
        /// client's manager path.
        pub fn call_tool(&mut self, name: &str, args: &Value) -> Result<Value, McpError> {
            let result = self.request(
                "tools/call",
                serde_json::json!({"name": name, "arguments": args}),
            )?;
            if result.get("isError").and_then(|b| b.as_bool()) == Some(true) {
                return Err(McpError::ServerToolError(crate::result_text(&result)));
            }
            Ok(result)
        }

        /// `resources/list`, following `nextCursor` pages. A repeated cursor or
        /// 1,000 pages ends the walk defensively.
        pub fn list_resources(&mut self) -> Result<Vec<McpResource>, McpError> {
            let mut out = Vec::new();
            let mut cursor: Option<String> = None;
            for _ in 0..1000 {
                let params = match &cursor {
                    Some(c) => serde_json::json!({"cursor": c}),
                    None => serde_json::json!({}),
                };
                let result = self.request("resources/list", params)?;
                out.extend(crate::resources_from(&result)?);
                match crate::next_cursor(&result) {
                    Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
                    _ => return Ok(out),
                }
            }
            Err(McpError::Protocol(
                "resources/list exceeded 1,000 pages".to_string(),
            ))
        }

        pub fn list_resource_templates(&mut self) -> Result<Vec<McpResourceTemplate>, McpError> {
            let mut out = Vec::new();
            let mut cursor: Option<String> = None;
            for _ in 0..1000 {
                let params = match &cursor {
                    Some(c) => serde_json::json!({"cursor": c}),
                    None => serde_json::json!({}),
                };
                let result = self.request("resources/templates/list", params)?;
                out.extend(crate::resource_templates_from(&result)?);
                match crate::next_cursor(&result) {
                    Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
                    _ => return Ok(out),
                }
            }
            Err(McpError::Protocol(
                "resources/templates/list exceeded 1,000 pages".to_string(),
            ))
        }

        pub fn read_resource(&mut self, uri: &str) -> Result<Vec<McpResourceContent>, McpError> {
            let result = self.request("resources/read", serde_json::json!({"uri": uri}))?;
            crate::resource_contents_from(&result)
        }

        pub fn list_prompts(&mut self) -> Result<Vec<McpPrompt>, McpError> {
            let mut out = Vec::new();
            let mut cursor: Option<String> = None;
            for _ in 0..1000 {
                let params = match &cursor {
                    Some(c) => serde_json::json!({"cursor": c}),
                    None => serde_json::json!({}),
                };
                let result = self.request("prompts/list", params)?;
                out.extend(crate::prompts_from(&result)?);
                match crate::next_cursor(&result) {
                    Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
                    _ => return Ok(out),
                }
            }
            Err(McpError::Protocol(
                "prompts/list exceeded 1,000 pages".to_string(),
            ))
        }

        pub fn get_prompt(
            &mut self,
            name: &str,
            args: &Value,
        ) -> Result<McpPromptResult, McpError> {
            let result = self.request(
                "prompts/get",
                serde_json::json!({"name": name, "arguments": args}),
            )?;
            crate::prompt_result_from(&result)
        }

        /// Cheap liveness: no traffic. A dead stream is noticed on the next
        /// request or health probe.
        pub fn alive(&mut self) -> bool {
            !self.dead
        }

        /// Mark dead and drop the stream handle. Idempotent. (The SSE reader
        /// thread may linger until the stream timeout; see the module docs.)
        fn kill(&mut self) {
            self.dead = true;
            self.sse_rx = None;
        }
    }

    impl Drop for HttpMcpClient {
        fn drop(&mut self) {
            self.kill();
        }
    }

    impl McpConn for HttpMcpClient {
        fn list_tools(&mut self) -> Result<Vec<McpToolDef>, McpError> {
            HttpMcpClient::list_tools(self)
        }

        fn call_tool(&mut self, name: &str, args: &Value) -> Result<Value, McpError> {
            HttpMcpClient::call_tool(self, name, args)
        }

        fn list_resources(&mut self) -> Result<Vec<McpResource>, McpError> {
            HttpMcpClient::list_resources(self)
        }

        fn list_resource_templates(&mut self) -> Result<Vec<McpResourceTemplate>, McpError> {
            HttpMcpClient::list_resource_templates(self)
        }

        fn read_resource(&mut self, uri: &str) -> Result<Vec<McpResourceContent>, McpError> {
            HttpMcpClient::read_resource(self, uri)
        }

        fn list_prompts(&mut self) -> Result<Vec<McpPrompt>, McpError> {
            HttpMcpClient::list_prompts(self)
        }

        fn get_prompt(&mut self, name: &str, args: &Value) -> Result<McpPromptResult, McpError> {
            HttpMcpClient::get_prompt(self, name, args)
        }

        fn alive(&mut self) -> bool {
            HttpMcpClient::alive(self)
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

    /// Match one decoded response value against our request id.
    fn match_response(v: &Value, want: &Value) -> Result<Value, McpError> {
        match classify(v) {
            Wire::Response {
                id: rid,
                result,
                error,
            } if rid == *want => {
                if let Some((code, message)) = error {
                    return Err(McpError::Rpc { code, message });
                }
                Ok(result.unwrap_or(Value::Null))
            }
            _ => Err(McpError::Protocol(
                "streamable response did not carry our request id".into(),
            )),
        }
    }

    fn tools_from(result: &Value, server: &str) -> Result<Vec<McpToolDef>, McpError> {
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
                    server: server.to_string(),
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

    /// Map a stream read failure to timeout vs framing. ureq surfaces its own
    /// timeouts on the response reader as an `io::Error` wrapping
    /// `Error::Timeout`, which displays as `timeout: <phase>`; a raw socket
    /// timeout keeps the `timed out` message.
    fn match_stream_err(e: &str, method: &str) -> McpError {
        if e.contains("timed out") || e.contains("TimedOut") || e.contains("timeout: ") {
            McpError::Timeout {
                method: method.to_string(),
            }
        } else {
            McpError::Framing(format!("sse stream: {e}"))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::{Read, Write};
        use std::net::TcpListener;

        #[test]
        fn tools_list_follows_next_cursor_over_streamable_http() {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
            let addr = listener.local_addr().expect("addr");
            let handle = std::thread::spawn(move || {
                for stream in listener.incoming().take(4) {
                    let Ok(mut s) = stream else { continue };
                    let mut buf = Vec::new();
                    let mut byte = [0u8; 1];
                    while !buf.ends_with(b"\r\n\r\n") {
                        match s.read(&mut byte) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => buf.push(byte[0]),
                        }
                    }
                    let text = String::from_utf8_lossy(&buf).into_owned();
                    let len = text
                        .lines()
                        .find_map(|l| {
                            l.strip_prefix("Content-Length:")
                                .or_else(|| l.strip_prefix("content-length:"))
                        })
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    let mut body = vec![0u8; len];
                    if len > 0 {
                        let _ = s.read_exact(&mut body);
                    }
                    let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    let method = req
                        .get("method")
                        .and_then(|m| m.as_str())
                        .unwrap_or("")
                        .to_string();
                    let (status, payload): (u16, String) = match method.as_str() {
                    "initialize" => (
                        200,
                        serde_json::json!({"jsonrpc": "2.0", "id": req.get("id"), "result": {"protocolVersion": crate::MCP_PROTOCOL_VERSION, "serverInfo": {"version": "http-1"}}}).to_string(),
                    ),
                    "tools/list" => {
                        let cursor = req.get("params").and_then(|p| p.get("cursor")).and_then(|c| c.as_str());
                        let result = if cursor == Some("p2") {
                            serde_json::json!({"tools": [{"name": "second", "inputSchema": {"type": "object"}}]})
                        } else {
                            serde_json::json!({"tools": [{"name": "first", "inputSchema": {"type": "object"}}], "nextCursor": "p2"})
                        };
                        (200, serde_json::json!({"jsonrpc": "2.0", "id": req.get("id"), "result": result}).to_string())
                    }
                    _ => (202, String::new()),
                };
                    let resp = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                    let _ = s.write_all(resp.as_bytes());
                }
            });
            let mut client = HttpMcpClient::connect(
                "fake-http",
                HttpTransport::Streamable,
                &format!("http://{addr}/mcp"),
                Duration::from_secs(10),
                &[],
            )
            .expect("connect fake http server");
            let tools = client.list_tools().expect("tools/list");
            assert_eq!(
                tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
                vec!["first", "second"]
            );
            drop(client);
            let _ = handle.join();
        }
    }
}

pub mod manager {
    //! MCP server lifecycle manager: the production wiring for MCP servers.
    //!
    //! Reads server specs (from `[mcp.servers.<name>]` config plus migration
    //! declaration files), spawns/connects them behind the operator-approval
    //! gate, keeps them alive with reconnect backoff, projects their tools
    //! into the [`ToolRegistry`](pantheon_tools::tools::ToolRegistry) as
    //! `mcp_<server>_<tool>`, and shuts everything down cleanly.
    //!
    //! # Approval
    //!
    //! MCP servers are third-party code: a server is never spawned or
    //! connected until the operator has approved its identity - name,
    //! self-reported version, and content hash - through the unified
    //! [`pantheon_api::approval`] store (the same store and privilege warning
    //! the plugin kinds use). The hash is the server binary (plus any arg
    //! that resolves to a file, so `python3 server.py` binds `server.py`)
    //! for stdio, and the endpoint URL for remote transports. Any change
    //! lapses the approval and the server goes back to pending.
    //!
    //! Approval is enforced on every path that can execute server code, not
    //! just at registration: before connecting (a hash-matching record must
    //! exist, so unapproved code is never executed just to fingerprint it),
    //! after the handshake (the exact name + version + hash must match, so a
    //! version bump lapses consent), on every (re)connect inside
    //! [`McpManager::ensure_connected_inner`] and [`McpManager::retry_now`]
    //! (the content hash is re-verified from the current spec, so a content
    //! change lapses the approval even if the cached hash is stale), and on
    //! every [`McpManager::call_tool`] even when the connection is already
    //! alive (the store is consulted, so a revoked approval blocks execution
    //! instead of riding a stale connection).
    //!
    //! Bundled catalog servers are first-party, not third-party - but only
    //! when the spec matches the canonical recipe exactly (transport,
    //! command, args, env, url). For those the `enabled` flag is the only
    //! gate: the server launches iff it is enabled, and it never appears in
    //! [`McpManager::pending_approval`]. A config entry, migration
    //! declaration, or wizard entry that borrows a bundled recipe name but
    //! carries a different command/args is a name collision, not a bundled
    //! server: it goes through the full consent store like any custom
    //! server, and refusal messages say so. This mirrors the dashboard's
    //! model ("enabled AND (bundled OR approved)"); the dashboard's approve
    //! endpoint 409s for bundled names because there is no consent to
    //! record.
    //!
    //! # Concurrency
    //!
    //! The manager is meant to live in an `Arc`: projected tool closures
    //! hold one and call back into [`McpManager::call_tool`]. All interior
    //! state sits behind one mutex, and blocking server I/O happens while it
    //! is held - one in-flight call per manager, which matches the agent
    //! loop's sequential tool execution.

    use crate::{
        bundled::{find_bundled, is_bundled},
        http::{HttpMcpClient, HttpTransport},
        McpClient, McpConn, McpError, McpPrompt, McpPromptResult, McpResource, McpResourceContent,
        McpResourceTemplate, McpServerConfig, McpToolDef,
    };
    use pantheon_api::approval::{self, ApprovalRecord, ApprovalStore};
    use pantheon_api::capability::{Capability, Policy};
    use pantheon_api::config::McpServerEntry;
    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::logging::now_ms;
    use pantheon_api::message::ToolSchema;
    use pantheon_secrets::SecretsBroker;
    use pantheon_tools::tools::{parse_args, ToolRegistry};
    use serde_json::Value;
    use std::collections::{HashMap, HashSet};
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// Resolver for `env:NAME` secret refs in a server's `env` map.
    /// The runtime installs one over its secrets broker; without it only the
    /// process environment is consulted.
    pub type EnvResolver = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

    /// How a remote server authenticates.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum McpAuth {
        /// Static headers (if any) or no auth.
        #[default]
        None,
        /// MCP OAuth: protected-resource metadata, dynamic client
        /// registration when needed, and PKCE over a loopback redirect.
        OAuth,
    }

    impl McpAuth {
        pub fn parse(s: Option<&str>) -> Option<Self> {
            match s.map(str::trim) {
                None | Some("") | Some("none") => Some(Self::None),
                Some("oauth") => Some(Self::OAuth),
                _ => None,
            }
        }
    }

    /// Which transport a managed server speaks.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum McpTransport {
        Stdio,
        Sse,
        Http,
    }

    impl McpTransport {
        pub fn parse(s: &str) -> Option<Self> {
            match s {
                "stdio" => Some(Self::Stdio),
                "sse" => Some(Self::Sse),
                "http" => Some(Self::Http),
                _ => None,
            }
        }

        pub fn as_str(self) -> &'static str {
            match self {
                Self::Stdio => "stdio",
                Self::Sse => "sse",
                Self::Http => "http",
            }
        }
    }

    /// One managed server, resolved from config or a declaration file.
    /// `env` values are raw: a value starting with `env:` is a secret ref
    /// resolved at connect time, anything else is a literal.
    ///
    /// `Debug` prints env variable *names* only - values (which may be
    /// secrets) never appear in logs.
    #[derive(Clone, PartialEq, Eq)]
    pub struct McpServerSpec {
        pub name: String,
        pub transport: McpTransport,
        pub command: Option<String>,
        pub args: Vec<String>,
        pub env: HashMap<String, String>,
        pub url: Option<String>,
        pub enabled: bool,
        pub timeout: Duration,
        /// Extra HTTP headers for sse/http transports. Values were already
        /// resolved (`env:` names looked up) by the config loader.
        pub headers: HashMap<String, String>,
        /// Remote auth mode.
        pub auth: McpAuth,
        /// OAuth scopes to request; empty uses discovered defaults.
        pub scopes: Vec<String>,
        /// Pre-registered OAuth client id; `None` permits dynamic registration.
        pub client_id: Option<String>,
    }

    impl std::fmt::Debug for McpServerSpec {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            let env_names: Vec<&String> = self.env.keys().collect();
            f.debug_struct("McpServerSpec")
                .field("name", &self.name)
                .field("transport", &self.transport)
                .field("command", &self.command)
                .field("args", &self.args)
                .field("env", &env_names)
                .field("url", &self.url)
                .field("enabled", &self.enabled)
                .field("timeout", &self.timeout)
                .field("auth", &self.auth)
                .field("scopes", &self.scopes)
                .field("client_id", &self.client_id.as_deref().map(|_| "<set>"))
                .finish()
        }
    }

    impl McpServerSpec {
        /// Build from a `[mcp.servers.<name>]` config entry. `None` on an
        /// unknown transport (the config validator reports it; the manager
        /// skips it).
        pub fn from_entry(name: &str, e: &McpServerEntry) -> Option<Self> {
            Some(Self {
                name: name.to_string(),
                transport: McpTransport::parse(e.transport.as_str())?,
                command: e.command.clone(),
                args: e.args.clone(),
                env: e.env.clone(),
                url: e.url.clone(),
                enabled: e.enabled,
                timeout: Duration::from_secs(e.timeout_secs.unwrap_or(30).max(1)),
                // Headers ride with the spec; `env:NAME` refs inside
                // them resolve at connect time (see `connect_new`), so
                // dropping them here would silently lose remote auth.
                headers: e.headers.clone(),
                auth: McpAuth::parse(e.auth.as_deref())?,
                scopes: e.scopes.clone(),
                client_id: e.client_id.clone(),
            })
        }

        /// Short human target for status lines: command or URL. Never env.
        pub fn target(&self) -> String {
            self.command
                .clone()
                .or_else(|| self.url.clone())
                .unwrap_or_else(|| "-".to_string())
        }
    }

    /// Bundled exemption bound to the RECIPE, not the name.
    ///
    /// A spec is exempt from the consent store only when its name is a
    /// bundled catalog name AND the spec matches the canonical recipe
    /// exactly (transport, command, args, env, url) - i.e. it is what
    /// [`BundledMcpServer`](crate::bundled::BundledMcpServer)::to_config_entry
    /// materializes via the single canonical writer. `enabled` and the
    /// timeout are operator-controlled and excluded from the comparison;
    /// `pinned_version` is informational and not part of the parsed entry.
    ///
    /// A name-colliding spec with any difference is a custom server: it
    /// goes through full approval like any other third-party server, and
    /// refusal messages flag the collision.
    ///
    /// Public for the gateway boot probe: a bundled name alone is NOT
    /// consent. A hand-written `[mcp.servers.<bundled-name>]` with an
    /// attacker command is a custom server and needs an approval record;
    /// only the canonical recipe probes freely at boot.
    pub fn is_canonical_bundled(spec: &McpServerSpec) -> bool {
        let Some(recipe) = find_bundled(&spec.name) else {
            return false;
        };
        let canonical = recipe.to_config_entry();
        spec.transport.as_str() == canonical.transport
            && spec.command == canonical.command
            && spec.args == canonical.args
            && spec.env == canonical.env
            && spec.url == canonical.url
    }

    /// The name-collision attack shape: `name` borrows a bundled recipe
    /// name but the spec is not the canonical recipe.
    fn is_bundled_name_collision(spec: &McpServerSpec) -> bool {
        is_bundled(&spec.name) && !is_canonical_bundled(spec)
    }

    /// Lifecycle state of one managed server.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ServerStatus {
        Disabled,
        Unapproved,
        Connecting,
        Ready,
        /// Between reconnect attempts; the cooldown is still running.
        Backoff,
        Failed,
    }

    impl ServerStatus {
        pub fn as_str(self) -> &'static str {
            match self {
                Self::Disabled => "disabled",
                Self::Unapproved => "unapproved",
                Self::Connecting => "connecting",
                Self::Ready => "ready",
                Self::Backoff => "backoff",
                Self::Failed => "failed",
            }
        }
    }

    /// Point-in-time health of one server, for the TUI and dashboard.
    #[derive(Debug, Clone)]
    pub struct ServerHealth {
        pub name: String,
        pub transport: &'static str,
        pub target: String,
        pub status: ServerStatus,
        pub tools: usize,
        pub connects: u32,
        pub failures: u32,
        pub last_error: Option<String>,
    }

    /// A configured server waiting on operator approval.
    #[derive(Debug, Clone)]
    pub struct PendingMcpServer {
        pub name: String,
        pub transport: &'static str,
        pub target: String,
        /// Content hash the approval would bind, when computable.
        pub content_hash: Option<String>,
        pub hash_error: Option<String>,
    }

    /// Outcome of [`McpManager::register_tools`].
    #[derive(Debug, Default)]
    pub struct McpReport {
        /// Namespaced tool names registered this pass.
        pub registered: Vec<String>,
        /// `(server, reason)` for servers that produced no tools.
        pub skipped: Vec<(String, String)>,
        /// Servers waiting on operator approval.
        pub pending_approval: Vec<String>,
    }

    /// Caller-supplied capability overrides for projected MCP tools.
    ///
    /// `server` maps a server name to the capability for all of its tools
    /// (the CUA driver pattern). `tool` maps an exact `(server, tool)` pair
    /// — built from the authoritative MCP handshake names, never by
    /// splitting the namespaced `mcp_<server>_<tool>` string — to the
    /// capability for that one tool.
    ///
    /// Deterministic precedence in [`resolve_tool_capability`]:
    /// per-tool override > per-server override > built-in per-tool
    /// classification > built-in server fallback > [`Capability::McpUnclassified`].
    #[derive(Debug, Clone, Default)]
    pub struct McpCapabilityOverrides {
        /// Server name -> capability for all tools of that server.
        pub server: HashMap<String, Capability>,
        /// Exact `(server, tool)` handshake pair -> capability for one tool.
        pub tool: HashMap<(String, String), Capability>,
    }

    impl McpCapabilityOverrides {
        /// Server-level override for one server (the CUA driver pattern).
        pub fn server_level(server: &str, cap: Capability) -> Self {
            let mut o = Self::default();
            o.server.insert(server.to_string(), cap);
            o
        }
    }

    /// Resolve the capability for one projected MCP tool.
    ///
    /// Precedence is deterministic: caller per-tool override, then caller
    /// per-server override, then built-in per-tool classification (REA, only
    /// for its audited server version),
    /// then built-in server fallback (REA's restrictive fallback), then
    /// [`Capability::McpUnclassified`] for every other server's tools.
    ///
    /// Fail-closed properties:
    /// * Unknown tools (no per-tool entry anywhere) fall to the server
    ///   fallback or [`Capability::McpUnclassified`], which default policies
    ///   park for approval — never to a *more* permissive capability than the
    ///   server's own classification. The old `NetworkOutbound` default was
    ///   allowed by the coder policy, so an unclassified destructive tool ran
    ///   with server approval as the only gate.
    /// * Malformed override keys cannot match: lookup uses exact
    ///   `(server, tool)` tuple equality against the handshake pair.
    /// * A resolution failure is impossible by construction (every path
    ///   ends at the default); the function is pure and total.
    pub fn resolve_tool_capability(
        overrides: &McpCapabilityOverrides,
        server: &str,
        tool: &str,
    ) -> Capability {
        resolve_tool_capability_for_version(overrides, server, tool, None)
    }

    /// Resolve one projected tool's capability using the MCP implementation
    /// version reported by `initialize`. REA's permissive entries apply only to
    /// the exact version whose effect inventory is embedded here; an absent or
    /// different version falls back to REA's approval-required server capability.
    pub fn resolve_tool_capability_for_version(
        overrides: &McpCapabilityOverrides,
        server: &str,
        tool: &str,
        server_version: Option<&str>,
    ) -> Capability {
        use std::sync::OnceLock;
        static REA_TOOLS: OnceLock<HashMap<(String, String), Capability>> = OnceLock::new();

        // 1. Caller per-tool override (exact handshake pair).
        if let Some(cap) = overrides.tool.get(&(server.to_string(), tool.to_string())) {
            return cap.clone();
        }
        // 2. Caller per-server override.
        if let Some(cap) = overrides.server.get(server) {
            return cap.clone();
        }
        // 3. The REA effect inventory is valid only for its pinned server
        // implementation version. Unknown/missing versions fail closed.
        if server == crate::rea::REA_SERVER_NAME {
            if server_version != Some(crate::rea_audit::REA_PINNED_VERSION) {
                return crate::rea::rea_server_capability();
            }
            let rea_tools = REA_TOOLS.get_or_init(crate::rea::rea_tool_capabilities);
            if let Some(cap) = rea_tools.get(&(server.to_string(), tool.to_string())) {
                return cap.clone();
            }
            // New tools in the audited version also fail closed.
            return crate::rea::rea_server_capability();
        }
        // 4. Default for all other servers: unclassified tools park on
        // approval. A server/tool the operator has not classified (and that
        // has no built-in inventory entry) gets no benefit of the doubt: the
        // old `NetworkOutbound` default was allowed by the coder policy, so
        // a destructive third-party tool ran unless an override existed.
        Capability::McpUnclassified
    }

    /// Sanitize one name segment for the `mcp_<server>_<tool>` namespace:
    /// lowercase alphanumeric runs joined by single underscores; anything
    /// else collapses. Never empty.
    /// Header map -> ordered header list for the HTTP transport. Sorted by
    /// name so the wire shape is deterministic for tests.
    pub fn resolved_headers(headers: &HashMap<String, String>) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        out.sort();
        out
    }

    pub fn sanitize_segment(s: &str) -> String {
        let mut out = String::new();
        let mut prev_us = true; // trim leading separators
        for c in s.chars() {
            if c.is_ascii_alphanumeric() {
                out.push(c.to_ascii_lowercase());
                prev_us = false;
            } else if !prev_us {
                out.push('_');
                prev_us = true;
            }
        }
        while out.ends_with('_') {
            out.pop();
        }
        if out.is_empty() {
            out.push('x');
        }
        out
    }

    /// Namespaced registry name for one server tool: `mcp_<server>_<tool>`.
    pub fn namespaced_tool_name(server: &str, tool: &str) -> String {
        format!(
            "mcp_{}_{}",
            sanitize_segment(server),
            sanitize_segment(tool)
        )
    }

    fn mcp_err(e: McpError) -> PantheonError {
        PantheonError::new(
            "MCP_TOOL_ERROR",
            Layer::Execution,
            false,
            e.to_string(),
            "check the MCP server's health with `pantheon mcp status`",
            "",
        )
    }

    /// True for transport failures worth one reconnect + retry: the server
    /// died mid-call rather than refusing the call.
    fn is_transport_error(e: &McpError) -> bool {
        matches!(
            e,
            McpError::Closed | McpError::Timeout { .. } | McpError::Io(_)
        )
    }

    /// Backoff after `failures` consecutive failures: 2s, 4s, 8s ... capped
    /// at 5 minutes.
    fn backoff_delay(failures: u32) -> Duration {
        // 2s, 4s, 8s, ... capped at 300s. Callers increment `failures`
        // before computing the delay, so the first failure arms 2s.
        // saturating_pow never panics
        // on huge failure counts; the min(300) does the capping.
        Duration::from_secs(2u64.saturating_pow(failures).min(300))
    }

    fn resolve_command(cmd: &str) -> Option<PathBuf> {
        if cmd.trim().is_empty() {
            return None;
        }
        let p = Path::new(cmd);
        if p.components().count() > 1 {
            return p.is_file().then(|| p.to_path_buf());
        }
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join(cmd))
                .find(|p| p.is_file())
        })
    }

    /// True when `command` is a launcher shim rather than the server itself:
    /// the program on the command line fetches (npx/uvx/bunx) or runs
    /// (docker/podman) a packaged server instead of being the server code.
    fn is_launcher_shim(command: &str) -> bool {
        let base = Path::new(command)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(command);
        matches!(base, "npx" | "bunx" | "pnpx" | "uvx" | "docker" | "podman")
    }

    /// Content hash the approval binds for one spec.
    ///
    /// For stdio the hash is the server binary (plus any argument that
    /// resolves to a file, so `python3 server.py` binds `server.py` too)
    /// except when the command is a launcher shim (`npx`, `uvx`, `docker`,
    /// ...), where the shim binary says nothing about the server code that
    /// actually runs. For shims the hash covers the command plus the full
    /// argument list instead: the package spec (`name@pin`) and flags are
    /// what determine the code that runs, so a pin bump or an arg change
    /// lapses the approval. For remote transports the hash is the endpoint
    /// URL. Names only, never values, in errors.
    ///
    /// Honest limit: for launcher shims this binds the *requested* package
    /// spec, not the bytes the registry served - registry-fetched code can
    /// change under a pin (mutable tags, cache poisoning), so the approval
    /// binds intent, not bytes. A local binary's hash binds the actual bytes
    /// on disk.
    fn content_hash(spec: &McpServerSpec) -> Result<String, McpError> {
        match spec.transport {
            McpTransport::Stdio => {
                let cmd = spec.command.as_deref().unwrap_or("");
                let mut bytes = Vec::new();
                if is_launcher_shim(cmd) {
                    // Bind what determines the code, not the shim binary.
                    // Honest limit: the requested package spec is bound,
                    // not the bytes the registry actually served.
                    bytes.extend_from_slice(b"mcp-launcher-shim\0");
                    bytes.extend_from_slice(cmd.as_bytes());
                    for a in &spec.args {
                        bytes.push(0);
                        bytes.extend_from_slice(a.as_bytes());
                    }
                    return Ok(approval::bytes_hash(&bytes));
                }
                match resolve_command(cmd) {
                    Some(p) => {
                        // Bind the resolved path as well as the bytes:
                        // identical bytes at a different path are a
                        // different installation, not the same approval.
                        bytes.extend_from_slice(p.as_os_str().as_encoded_bytes());
                        bytes.push(0);
                        match std::fs::read(&p) {
                            Ok(b) => bytes.extend_from_slice(&b),
                            Err(e) => {
                                return Err(McpError::Io(format!("hash {}: {e}", p.display())))
                            }
                        }
                    }
                    None => {
                        // Fallback, honest limit: the command could not
                        // be resolved to a file (e.g. PATH differs at
                        // approval time), so only the command string is
                        // bound. A swapped binary under the same name
                        // would not lapse approval.
                        bytes.extend_from_slice(cmd.as_bytes());
                        bytes.push(0);
                    }
                }
                // Bind script files passed as arguments: the interpreter
                // binary alone would not lapse approval when the script
                // changes. An unreadable script file fails the hash
                // instead of silently contributing nothing to it.
                for a in &spec.args {
                    let p = Path::new(a);
                    if p.is_file() {
                        match std::fs::read(p) {
                            Ok(b) => {
                                bytes.push(0);
                                bytes.extend_from_slice(&b);
                            }
                            Err(e) => {
                                return Err(McpError::Io(format!("hash {}: {e}", p.display())))
                            }
                        }
                    }
                }
                Ok(approval::bytes_hash(&bytes))
            }
            McpTransport::Sse | McpTransport::Http => {
                // Honest limit: a remote transport has no bytes to bind,
                // so only the endpoint URL is hashed. Server code changing
                // at the same URL never lapses approval.
                let url = spec.url.as_deref().unwrap_or("");
                Ok(approval::bytes_hash(url.as_bytes()))
            }
        }
    }

    struct ServerState {
        spec: McpServerSpec,
        status: ServerStatus,
        conn: Option<Box<dyn McpConn>>,
        tools: Vec<McpToolDef>,
        /// `connects` value the cached `tools` were fetched on. A
        /// per-turn re-list is skipped when the connection is unchanged
        /// (see `register_names`): a transient list failure must not
        /// drop the server's tools for the turn.
        tools_connects: u32,
        /// Content hash, computed at configure time and re-verified on every
        /// (re)connect by the approval gate.
        hash: Option<String>,
        hash_error: Option<String>,
        connects: u32,
        failures: u32,
        not_before: Option<Instant>,
        last_error: Option<String>,
    }

    impl ServerState {
        fn new(spec: McpServerSpec) -> Self {
            let (hash, hash_error) = match content_hash(&spec) {
                Ok(h) => (Some(h), None),
                Err(e) => (None, Some(e.to_string())),
            };
            let status = if spec.enabled {
                ServerStatus::Unapproved
            } else {
                ServerStatus::Disabled
            };
            Self {
                spec,
                status,
                conn: None,
                tools: Vec::new(),
                tools_connects: 0,
                hash,
                hash_error,
                connects: 0,
                failures: 0,
                not_before: None,
                last_error: None,
            }
        }
    }

    struct Inner {
        data_dir: PathBuf,
        specs: Vec<McpServerSpec>,
        states: HashMap<String, ServerState>,
        env_resolver: Option<EnvResolver>,
        /// Capability policy gating stdio server spawns (see
        /// [`McpClient::connect`]). `None` = no policy configured: spawns
        /// proceed un-gated, as before.
        policy: Option<Policy>,
        /// Durable secret store for remote OAuth tokens. `None` disables OAuth
        /// connects with an actionable error instead of writing plaintext.
        secrets: Option<SecretsBroker>,
        /// Host callback for MCP `elicitation/create` requests.
        elicitation: Option<crate::client::ElicitationHandler>,
    }

    impl Inner {
        fn scope_dir(&self) -> PathBuf {
            self.data_dir.join("mcp")
        }

        fn live_path(&self) -> PathBuf {
            self.scope_dir().join("live.json")
        }
    }

    /// Manages MCP server lifecycles for one session: connect, health,
    /// reconnect backoff, approval, registry projection, shutdown.
    ///
    /// Construct with [`McpManager::new`], hand out as `Arc<McpManager>`,
    /// feed specs with [`McpManager::configure`], and project tools with
    /// [`McpManager::register_tools`].
    pub struct McpManager {
        inner: Mutex<Inner>,
    }

    impl McpManager {
        pub fn new(data_dir: PathBuf) -> Self {
            Self {
                inner: Mutex::new(Inner {
                    data_dir,
                    specs: Vec::new(),
                    states: HashMap::new(),
                    env_resolver: None,
                    policy: None,
                    secrets: None,
                    elicitation: None,
                }),
            }
        }

        /// Install the secret resolver for `env:NAME` refs. Without one,
        /// refs fall back to the process environment.
        pub fn set_env_resolver(&self, resolver: EnvResolver) {
            if let Ok(mut inner) = self.inner.lock() {
                inner.env_resolver = Some(resolver);
            }
        }

        /// Install the capability policy gating stdio server spawns.
        /// Without one, spawns proceed un-gated (the standalone/test
        /// default). The session wires its live policy here at startup.
        pub fn set_policy(&self, policy: Policy) {
            if let Ok(mut inner) = self.inner.lock() {
                inner.policy = Some(policy);
            }
        }

        /// Install the durable secret store used for remote OAuth tokens.
        /// Without one, an OAuth server fails with an actionable error rather
        /// than falling back to plaintext storage.
        pub fn set_secrets(&self, secrets: SecretsBroker) {
            if let Ok(mut inner) = self.inner.lock() {
                inner.secrets = Some(secrets);
            }
        }

        /// Install the host callback for MCP `elicitation/create`. Without one,
        /// servers receive `-32601` and tools that require elicitation fail
        /// cleanly instead of hanging.
        pub fn set_elicitation_handler(&self, handler: crate::client::ElicitationHandler) {
            if let Ok(mut inner) = self.inner.lock() {
                inner.elicitation = Some(handler);
            }
        }

        fn oauth_parts(&self, name: &str) -> Result<(McpServerSpec, SecretsBroker), McpError> {
            let inner = self
                .inner
                .lock()
                .map_err(|_| McpError::Io("mcp manager lock poisoned".into()))?;
            let spec = inner
                .specs
                .iter()
                .find(|s| s.name == name)
                .cloned()
                .ok_or_else(|| McpError::Unauthorized(format!("unknown MCP server '{name}'")))?;
            let secrets = inner.secrets.clone().ok_or_else(|| {
            McpError::Unauthorized(
                "no secret store is installed for this session; run `pantheon mcp login` from the CLI"
                    .into(),
            )
        })?;
            Ok((spec, secrets))
        }

        /// Run the interactive OAuth login for one configured server.
        pub fn login_oauth(&self, name: &str) -> Result<(), McpError> {
            let (spec, secrets) = self.oauth_parts(name)?;
            crate::oauth::login(&spec, &secrets)
        }

        /// Forget stored OAuth credentials for one configured server.
        pub fn logout_oauth(&self, name: &str) -> Result<(), McpError> {
            let (spec, secrets) = self.oauth_parts(name)?;
            crate::oauth::logout(&spec, &secrets)
        }

        /// `(connected, scopes, expires_at)` for one configured server.
        pub fn oauth_status(
            &self,
            name: &str,
        ) -> Result<(bool, Vec<String>, Option<u64>), McpError> {
            let (spec, secrets) = self.oauth_parts(name)?;
            crate::oauth::status(&spec, &secrets)
        }

        /// Replace the managed spec set. Idempotent: unchanged specs keep
        /// their connections; removed specs are shut down; changed specs
        /// reconnect lazily on next use. Specs are kept sorted by name so
        /// registration order is deterministic.
        pub fn configure(&self, mut specs: Vec<McpServerSpec>) {
            specs.sort_by(|a, b| a.name.cmp(&b.name));
            let Ok(mut inner) = self.inner.lock() else {
                return;
            };
            if inner.specs == specs {
                return;
            }
            let mut next: HashMap<String, ServerState> = HashMap::new();
            for spec in specs {
                let name = spec.name.clone();
                match inner.states.remove(&name) {
                    Some(st) if st.spec == spec => {
                        next.insert(name, st);
                    }
                    Some(mut st) => {
                        // Spec changed: drop the connection, re-fingerprint.
                        st.conn = None;
                        st.status = if spec.enabled {
                            ServerStatus::Unapproved
                        } else {
                            ServerStatus::Disabled
                        };
                        st.tools.clear();
                        st.failures = 0;
                        st.not_before = None;
                        st.last_error = None;
                        let (hash, hash_error) = match content_hash(&spec) {
                            Ok(h) => (Some(h), None),
                            Err(e) => (None, Some(e.to_string())),
                        };
                        st.hash = hash;
                        st.hash_error = hash_error;
                        st.spec = spec;
                        next.insert(name, st);
                    }
                    None => {
                        next.insert(name.clone(), ServerState::new(spec));
                    }
                }
            }
            // Removed specs: shut their connections down.
            for (_, mut st) in inner.states.drain() {
                if let Some(mut c) = st.conn.take() {
                    c.shutdown();
                }
            }
            inner.specs = next.values().map(|s| s.spec.clone()).collect();
            inner.specs.sort_by(|a, b| a.name.cmp(&b.name));
            inner.states = next;
        }

        pub fn spec_names(&self) -> Vec<String> {
            self.inner
                .lock()
                .map(|i| i.specs.iter().map(|s| s.name.clone()).collect())
                .unwrap_or_default()
        }

        /// Enabled servers with no hash-matching approval record: the set
        /// `pantheon mcp approve` should offer.
        pub fn pending_approval(&self) -> Vec<PendingMcpServer> {
            let Ok(inner) = self.inner.lock() else {
                return Vec::new();
            };
            let store = ApprovalStore::open(&inner.scope_dir());
            let mut out = Vec::new();
            for st in inner.states.values() {
                if !st.spec.enabled {
                    continue;
                }
                // Only the canonical bundled recipe is first-party: an enabled
                // spec matching the recipe exactly is never pending approval.
                // A name-colliding spec with a different command/args is a
                // custom server and stays listed, so the operator must
                // approve its exact content before it runs.
                if is_canonical_bundled(&st.spec) {
                    continue;
                }
                let approved = match (&st.hash, store.get(&st.spec.name)) {
                    (Some(h), Some(rec)) => &rec.dir_hash == h,
                    _ => false,
                };
                if !approved {
                    out.push(PendingMcpServer {
                        name: st.spec.name.clone(),
                        transport: st.spec.transport.as_str(),
                        target: st.spec.target(),
                        content_hash: st.hash.clone(),
                        hash_error: st.hash_error.clone(),
                    });
                }
            }
            out.sort_by(|a, b| a.name.cmp(&b.name));
            out
        }

        /// Fingerprint the server (connect + handshake) and record the
        /// operator's approval of its exact identity. The caller must have
        /// shown [`approval::warning_text`] and obtained explicit consent
        /// first - this only persists the record, like the plugin flow.
        pub fn approve_server(&self, name: &str) -> Result<ApprovalRecord, McpError> {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| McpError::Io("mcp manager lock poisoned".to_string()))?;
            let resolver = inner.env_resolver.clone();
            let policy = inner.policy.clone();
            let scope = inner.scope_dir();
            let spec = {
                let st = inner.states.get_mut(name).ok_or_else(|| {
                    McpError::Protocol(format!("no mcp server named '{name}' is configured"))
                })?;
                if st.hash.is_none() {
                    return Err(McpError::Io(
                        st.hash_error
                            .clone()
                            .unwrap_or_else(|| "cannot hash server".to_string()),
                    ));
                }
                st.spec.clone()
            };
            // Fingerprint: connect, then bind name + version + hash.
            let secrets = inner.secrets.clone();
            let elicitation = inner.elicitation.clone();
            let conn = Self::connect_new(
                &spec,
                &resolver,
                &policy,
                secrets.as_ref(),
                elicitation.as_ref(),
            )?;
            let version = conn
                .server_version()
                .map(str::to_string)
                .unwrap_or_else(|| conn.negotiated_version().to_string());
            let st = inner.states.get_mut(name).expect("checked above");
            let hash = st.hash.clone().unwrap_or_default();
            let rec = ApprovalRecord {
                plugin: name.to_string(),
                version,
                dir_hash: hash,
                approved_at_ms: now_ms() as u64,
            };
            ApprovalStore::open(&scope)
                .record(rec.clone())
                .map_err(|e| McpError::Io(format!("record approval: {e}")))?;
            // Keep the fingerprinted connection: register_tools reuses it.
            st.conn = Some(conn);
            st.status = ServerStatus::Ready;
            st.connects += 1;
            st.failures = 0;
            st.not_before = None;
            st.last_error = None;
            Ok(rec)
        }

        /// Project every approved, enabled server's tools into the registry
        /// as `mcp_<server>_<tool>`. Unapproved servers are never connected.
        /// Returns what registered, what was skipped and why, and what is
        /// still waiting on approval.
        pub fn register_tools(self: &Arc<Self>, reg: &mut ToolRegistry) -> McpReport {
            let names: Vec<String> = match self.inner.lock() {
                Ok(inner) => inner.specs.iter().map(|s| s.name.clone()).collect(),
                Err(_) => {
                    let mut report = McpReport::default();
                    report
                        .skipped
                        .push(("<manager>".into(), "lock poisoned".into()));
                    return report;
                }
            };
            self.register_names(reg, &names, &McpCapabilityOverrides::default())
        }

        /// Project one server's tools into the registry, with the same gates
        /// as [`McpManager::register_tools`] but restricted to `server`. Used
        /// for servers gated by their own toggle rather than the Plugins
        /// group - today, the CUA driver behind the ComputerUse group.
        ///
        /// `capability` overrides the projected tools' capability (default
        /// [`Capability::McpUnclassified`], which the default policies park
        /// for human approval). The CUA driver passes
        /// [`Capability::ComputerUse`]: desktop control is an inward
        /// capability and must park for human approval under the default
        /// policy, not ride the outward network treatment.
        pub fn register_server_tools(
            self: &Arc<Self>,
            reg: &mut ToolRegistry,
            server: &str,
            capability: Option<Capability>,
        ) -> McpReport {
            let mut overrides = McpCapabilityOverrides::default();
            if let Some(cap) = capability {
                overrides.server.insert(server.to_string(), cap);
            }
            self.register_names(reg, &[server.to_string()], &overrides)
        }

        /// Project one server's tools with per-tool capability overrides.
        /// `overrides` carries both server-level and `(server, tool)`
        /// per-tool entries; see [`McpCapabilityOverrides`] for precedence.
        /// Built-in classifications (e.g. REA) apply underneath caller
        /// overrides.
        pub fn register_server_tools_with(
            self: &Arc<Self>,
            reg: &mut ToolRegistry,
            server: &str,
            overrides: &McpCapabilityOverrides,
        ) -> McpReport {
            self.register_names(reg, &[server.to_string()], overrides)
        }

        /// Project the MCP resource/prompt surface into the registry.
        ///
        /// These are manager-level tools (`mcp.resources.list` / `.templates`
        /// / `.read`, `mcp.prompts.list` / `.get`) rather than per-server
        /// projections:
        /// resources and prompts are addressed by URI/name, not by a
        /// namespaced tool symbol. Every call still goes through
        /// [`McpManager::with_conn`], so the server's approval gate and
        /// reconnect behavior apply unchanged.
        ///
        /// Capability: `NetworkOutbound`. A resource read can carry arbitrary
        /// third-party content, and the MCP server itself is third-party code;
        /// the per-server approval is the trust decision, not a narrower
        /// filesystem claim.
        pub fn register_resource_tools(self: &Arc<Self>, reg: &mut ToolRegistry) -> usize {
            let before = reg.names().len();
            let schema =
                |name: &str, description: &str, props: Value, required: Vec<&str>| ToolSchema {
                    name: name.into(),
                    description: description.into(),
                    parameters: serde_json::json!({
                        "type": "object",
                        "properties": props,
                        "required": required,
                    }),
                };

            let mgr = Arc::clone(self);
            reg.register(
            schema(
                "mcp.resources.list",
                "List MCP resources. With 'server', list one server; without it, list every configured server. Returns resources plus any per-server errors.",
                serde_json::json!({
                    "server": {"type":"string","description":"MCP server name (default: all configured servers)"},
                }),
                vec![],
            ),
            Capability::NetworkOutbound,
            move |args| {
                let v = parse_args(args)?;
                let names = match v.get("server").and_then(Value::as_str) {
                    Some(s) => vec![s.to_string()],
                    None => mgr.spec_names(),
                };
                let mut out = serde_json::Map::new();
                for name in names {
                    match mgr.list_resources(&name) {
                        Ok(resources) => {
                            out.insert(name, serde_json::json!({"resources": resources}));
                        }
                        Err(e) => {
                            out.insert(name, serde_json::json!({"error": e.to_string()}));
                        }
                    }
                }
                serde_json::to_string_pretty(&Value::Object(out))
                    .map_err(|e| mcp_err(McpError::Protocol(e.to_string())))
            },
        );

            let mgr = Arc::clone(self);
            reg.register(
            schema(
                "mcp.resources.templates",
                "List MCP resource templates (parameterized URIs). With 'server', list one server; without it, list every configured server.",
                serde_json::json!({
                    "server": {"type":"string","description":"MCP server name (default: all configured servers)"},
                }),
                vec![],
            ),
            Capability::NetworkOutbound,
            move |args| {
                let v = parse_args(args)?;
                let names = match v.get("server").and_then(Value::as_str) {
                    Some(s) => vec![s.to_string()],
                    None => mgr.spec_names(),
                };
                let mut out = serde_json::Map::new();
                for name in names {
                    match mgr.list_resource_templates(&name) {
                        Ok(templates) => {
                            out.insert(name, serde_json::json!({"templates": templates}));
                        }
                        Err(e) => {
                            out.insert(name, serde_json::json!({"error": e.to_string()}));
                        }
                    }
                }
                serde_json::to_string_pretty(&Value::Object(out))
                    .map_err(|e| mcp_err(McpError::Protocol(e.to_string())))
            },
        );

            let mgr = Arc::clone(self);
            reg.register(
            schema(
                "mcp.resources.read",
                "Read one MCP resource by URI. Returns all content blocks the server sends (text and/or base64 blob).",
                serde_json::json!({
                    "server": {"type":"string","description":"MCP server name"},
                    "uri": {"type":"string","description":"Resource URI"},
                }),
                vec!["server", "uri"],
            ),
            Capability::NetworkOutbound,
            move |args| {
                let v = parse_args(args)?;
                let server = v
                    .get("server")
                    .and_then(Value::as_str)
                    .ok_or_else(|| mcp_err(McpError::Protocol("missing 'server'".into())))?;
                let uri = v
                    .get("uri")
                    .and_then(Value::as_str)
                    .ok_or_else(|| mcp_err(McpError::Protocol("missing 'uri'".into())))?;
                let contents = mgr.read_resource(server, uri).map_err(mcp_err)?;
                serde_json::to_string_pretty(&serde_json::json!({
                    "server": server,
                    "uri": uri,
                    "contents": contents,
                }))
                .map_err(|e| mcp_err(McpError::Protocol(e.to_string())))
            },
        );

            let mgr = Arc::clone(self);
            reg.register(
            schema(
                "mcp.prompts.list",
                "List MCP prompt templates. With 'server', list one server; without it, list every configured server. Returns prompts plus any per-server errors.",
                serde_json::json!({
                    "server": {"type":"string","description":"MCP server name (default: all configured servers)"},
                }),
                vec![],
            ),
            Capability::NetworkOutbound,
            move |args| {
                let v = parse_args(args)?;
                let names = match v.get("server").and_then(Value::as_str) {
                    Some(s) => vec![s.to_string()],
                    None => mgr.spec_names(),
                };
                let mut out = serde_json::Map::new();
                for name in names {
                    match mgr.list_prompts(&name) {
                        Ok(prompts) => {
                            out.insert(name, serde_json::json!({"prompts": prompts}));
                        }
                        Err(e) => {
                            out.insert(name, serde_json::json!({"error": e.to_string()}));
                        }
                    }
                }
                serde_json::to_string_pretty(&Value::Object(out))
                    .map_err(|e| mcp_err(McpError::Protocol(e.to_string())))
            },
        );

            let mgr = Arc::clone(self);
            reg.register(
            schema(
                "mcp.prompts.get",
                "Expand one MCP prompt template with arguments. Returns the prompt messages (content stays structured).",
                serde_json::json!({
                    "server": {"type":"string","description":"MCP server name"},
                    "name": {"type":"string","description":"Prompt name"},
                    "arguments": {"type":"object","description":"Prompt arguments (string values)"},
                }),
                vec!["server", "name"],
            ),
            Capability::NetworkOutbound,
            move |args| {
                let v = parse_args(args)?;
                let server = v
                    .get("server")
                    .and_then(Value::as_str)
                    .ok_or_else(|| mcp_err(McpError::Protocol("missing 'server'".into())))?;
                let name = v
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| mcp_err(McpError::Protocol("missing 'name'".into())))?;
                let arguments = v
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({}));
                let result = mgr.get_prompt(server, name, &arguments).map_err(mcp_err)?;
                serde_json::to_string_pretty(&serde_json::json!({
                    "server": server,
                    "name": name,
                    "result": result,
                }))
                .map_err(|e| mcp_err(McpError::Protocol(e.to_string())))
            },
        );

            reg.names().len() - before
        }

        fn register_names(
            self: &Arc<Self>,
            reg: &mut ToolRegistry,
            names: &[String],
            overrides: &McpCapabilityOverrides,
        ) -> McpReport {
            let mut report = McpReport::default();
            // Dedup against names already in the registry, not just
            // within this call: two servers sanitizing to the same
            // `mcp_*` name across register_tools + register_server_tools
            // previously overwrote each other silently.
            let mut used: HashSet<String> = reg.names().into_iter().collect();
            // Hold the lock for the whole pass: server I/O is sequential and
            // the agent loop calls tools one at a time anyway.
            let mut inner = match self.inner.lock() {
                Ok(g) => g,
                Err(_) => {
                    report
                        .skipped
                        .push(("<manager>".into(), "lock poisoned".into()));
                    return report;
                }
            };
            let store = ApprovalStore::open(&inner.scope_dir());
            for name in names {
                let name = name.clone();
                let Some(st) = inner.states.get_mut(&name) else {
                    continue;
                };
                if !st.spec.enabled {
                    st.status = ServerStatus::Disabled;
                    report.skipped.push((name, "disabled".into()));
                    continue;
                }
                let hash = match st.hash.clone() {
                    Some(h) => h,
                    None => {
                        st.status = ServerStatus::Failed;
                        let e = st
                            .hash_error
                            .clone()
                            .unwrap_or_else(|| "cannot hash server".to_string());
                        st.last_error = Some(e.clone());
                        report.skipped.push((name, format!("hash failed: {e}")));
                        continue;
                    }
                };
                // Pre-connect gate: no hash-matching record, no execution.
                // Only the canonical bundled recipe skips the consent store;
                // for it the enabled flag (checked above) is the only gate.
                // A name-colliding spec goes through full approval like any
                // custom server.
                let pre = is_canonical_bundled(&st.spec)
                    || store.get(&name).is_some_and(|r| r.dir_hash == hash);
                if !pre {
                    st.status = ServerStatus::Unapproved;
                    report.pending_approval.push(name);
                    continue;
                }
                if let Err(e) = Self::ensure_connected_inner(&mut inner, &name) {
                    let st = inner.states.get_mut(&name).expect("state");
                    st.last_error = Some(e.to_string());
                    report.skipped.push((name, e.to_string()));
                    continue;
                }
                // Post-handshake gate: the content hash must be approved.
                // The hash is authoritative; the version the server
                // reports on handshake is recorded for DISPLAY only and
                // never gates execution (a version string can lie, and a
                // version bump with identical content is not a consent
                // change). Only a content change lapses consent. Only the
                // canonical bundled recipe skips this: there is no consent
                // record to lapse, and catalog updates are pinned by
                // recipe. A name-colliding spec is a custom server and must
                // be approved.
                let (version, server_impl_version, ok) = {
                    let st = inner.states.get(&name).expect("state");
                    let c = st.conn.as_ref().expect("connected");
                    let server_impl_version = c.server_version().map(str::to_string);
                    let v = server_impl_version
                        .clone()
                        .unwrap_or_else(|| c.negotiated_version().to_string());
                    let ok =
                        is_canonical_bundled(&st.spec) || store.is_approved_for_hash(&name, &hash);
                    (v, server_impl_version, ok)
                };
                if !ok {
                    let st = inner.states.get_mut(&name).expect("state");
                    if let Some(mut c) = st.conn.take() {
                        c.shutdown();
                    }
                    st.status = ServerStatus::Unapproved;
                    st.last_error = Some(format!(
                    "approval lapsed: content hash {hash} is not approved for '{name}' (server reports version {version:?}; version is display-only, the hash gates)"
                ));
                    report.pending_approval.push(name);
                    continue;
                }
                // Per-turn tool list: reuse the last good list when
                // this connection already supplied it. A transient
                // list failure then falls back to the cache instead of
                // dropping the server's tools for the turn.
                let tools = {
                    let st = inner.states.get_mut(&name).expect("state");
                    if !st.tools.is_empty() && st.tools_connects == st.connects {
                        st.tools.clone()
                    } else {
                        match st.conn.as_mut().expect("connected").list_tools() {
                            Ok(t) => t,
                            Err(e) => {
                                if !st.tools.is_empty() {
                                    st.tools.clone()
                                } else {
                                    Self::note_failure(st, e.to_string());
                                    let msg = st.last_error.clone().unwrap_or_default();
                                    report.skipped.push((name, msg));
                                    continue;
                                }
                            }
                        }
                    }
                };
                {
                    let st = inner.states.get_mut(&name).expect("state");
                    st.tools = tools.clone();
                    st.tools_connects = st.connects;
                    st.status = ServerStatus::Ready;
                    st.failures = 0;
                    st.last_error = None;
                }
                let san_server = sanitize_segment(&name);
                for def in &tools {
                    let mut ns = namespaced_tool_name(&name, &def.name);
                    let mut n = 2u32;
                    while !used.insert(ns.clone()) {
                        ns = format!("mcp_{san_server}_{}_{n}", sanitize_segment(&def.name));
                        n += 1;
                    }
                    // The projected capability is `McpUnclassified` by default:
                    // an MCP tool is third-party code whose effects Pantheon's
                    // taxonomy cannot see inside, and the server approval
                    // (name + version + content hash) is a trust decision about
                    // *which* server, not about what each of its tools does.
                    // `McpUnclassified` is parked by the default policies, so an
                    // unclassified tool stops for a human instead of riding an
                    // allowed capability. A caller may override the capability
                    // per server or per tool (see [`McpCapabilityOverrides`]):
                    // the CUA driver projects as `ComputerUse`, the inward
                    // desktop-control capability, which parks for human
                    // approval by default. Built-in classifications (e.g. REA's
                    // static-vs-tracing split) apply underneath caller overrides.
                    //
                    // `name`/`def.name` are the authoritative handshake pair;
                    // the lookup never splits the namespaced `mcp_<s>_<t>`
                    // string, so a tool cannot accidentally match another
                    // server or tool.
                    let cap = resolve_tool_capability_for_version(
                        overrides,
                        &name,
                        &def.name,
                        server_impl_version.as_deref(),
                    );
                    let schema = ToolSchema {
                        name: ns.clone(),
                        description: format!("[{}] {}", name, def.description),
                        parameters: def.input_schema.clone(),
                    };
                    let mgr = Arc::clone(self);
                    let server = name.clone();
                    let tool = def.name.clone();
                    reg.register(schema, cap, move |args_str| {
                        let args = parse_args(args_str)?;
                        let value = mgr.call_tool(&server, &tool, &args).map_err(mcp_err)?;
                        Ok(crate::result_text(&value))
                    });
                    report.registered.push(ns);
                }
            }
            drop(inner);
            self.write_live_state();
            report
        }

        /// Run one request against a connected server. Reconnects with backoff
        /// when the transport died; on a transport failure mid-call it
        /// reconnects once and retries the request once.
        fn with_conn<T>(
            &self,
            server: &str,
            f: impl Fn(&mut dyn McpConn) -> Result<T, McpError>,
        ) -> Result<T, McpError> {
            let result = self.with_conn_inner(server, f);
            if result.is_err() {
                // A failed call changes status / last_error in memory;
                // live.json must reflect it instead of showing the old
                // tool counts and state until the next health check.
                self.write_live_state();
            }
            result
        }

        fn with_conn_inner<T>(
            &self,
            server: &str,
            f: impl Fn(&mut dyn McpConn) -> Result<T, McpError>,
        ) -> Result<T, McpError> {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| McpError::Io("mcp manager lock poisoned".to_string()))?;
            if !inner.states.contains_key(server) {
                return Err(McpError::Protocol(format!(
                    "mcp server '{server}' is not configured"
                )));
            }
            Self::ensure_connected_inner(&mut inner, server)?;
            let res = {
                let st = inner.states.get_mut(server).expect("state");
                f(st.conn.as_deref_mut().expect("connected"))
            };
            match res {
                Ok(v) => {
                    // Success resets the consecutive-failure count (see
                    // `check_health`): a recovered server must not keep
                    // stale failures toward the repair threshold.
                    let st = inner.states.get_mut(server).expect("state");
                    st.last_error = None;
                    st.failures = 0;
                    Ok(v)
                }
                Err(e) if is_transport_error(&e) => {
                    // The server died mid-call: one reconnect + one retry.
                    // Failures still count toward backoff for the *next*
                    // call; this retry is the call's single second chance.
                    let resolver = inner.env_resolver.clone();
                    let policy = inner.policy.clone();
                    let scope = inner.scope_dir();
                    let st = inner.states.get_mut(server).expect("state");
                    let msg = e.to_string();
                    st.conn = None;
                    st.failures += 1;
                    st.not_before = Some(Instant::now() + backoff_delay(st.failures));
                    st.last_error = Some(msg);
                    let spec = st.spec.clone();
                    // #5: the mid-call reconnect is a (re)connect - gate it
                    // like every other connect path, so a lapsed or revoked
                    // approval blocks the retry instead of silently
                    // re-executing.
                    Self::gate_approval(&mut inner.states, &scope, server, true)?;
                    let secrets = inner.secrets.clone();
                    let elicitation = inner.elicitation.clone();
                    match Self::connect_new(
                        &spec,
                        &resolver,
                        &policy,
                        secrets.as_ref(),
                        elicitation.as_ref(),
                    ) {
                        Ok(conn) => {
                            let st = inner.states.get_mut(server).expect("state");
                            st.conn = Some(conn);
                            st.connects += 1;
                            st.status = ServerStatus::Ready;
                            st.not_before = None;
                            let retry = {
                                let conn = st.conn.as_deref_mut().expect("connected");
                                f(conn)
                            };
                            st.last_error = match &retry {
                                Ok(_) => None,
                                Err(e) => Some(e.to_string()),
                            };
                            if retry.is_ok() {
                                st.failures = 0;
                            }
                            retry
                        }
                        Err(ce) => {
                            let st = inner.states.get_mut(server).expect("state");
                            st.status = ServerStatus::Failed;
                            st.last_error = Some(ce.to_string());
                            Err(ce)
                        }
                    }
                }
                Err(e) => Err(e),
            }
        }

        /// Call one server tool by (server, raw tool name).
        pub fn call_tool(&self, server: &str, tool: &str, args: &Value) -> Result<Value, McpError> {
            self.with_conn(server, |conn| conn.call_tool(tool, args))
        }

        /// List a server's static resources (`resources/list`).
        pub fn list_resources(&self, server: &str) -> Result<Vec<McpResource>, McpError> {
            self.with_conn(server, |conn| conn.list_resources())
        }

        /// List a server's parameterized resource templates.
        pub fn list_resource_templates(
            &self,
            server: &str,
        ) -> Result<Vec<McpResourceTemplate>, McpError> {
            self.with_conn(server, |conn| conn.list_resource_templates())
        }

        /// Read one resource by URI (`resources/read`).
        pub fn read_resource(
            &self,
            server: &str,
            uri: &str,
        ) -> Result<Vec<McpResourceContent>, McpError> {
            self.with_conn(server, |conn| conn.read_resource(uri))
        }

        /// List a server's prompt templates (`prompts/list`).
        pub fn list_prompts(&self, server: &str) -> Result<Vec<McpPrompt>, McpError> {
            self.with_conn(server, |conn| conn.list_prompts())
        }

        /// Expand one prompt with arguments (`prompts/get`).
        pub fn get_prompt(
            &self,
            server: &str,
            name: &str,
            args: &Value,
        ) -> Result<McpPromptResult, McpError> {
            self.with_conn(server, |conn| conn.get_prompt(name, args))
        }

        /// The configured spec for a server, if any.
        pub fn spec(&self, name: &str) -> Option<McpServerSpec> {
            let Ok(inner) = self.inner.lock() else {
                return None;
            };
            inner.states.get(name).map(|st| st.spec.clone())
        }

        /// Health snapshot without probing: cheap liveness only.
        pub fn health(&self) -> Vec<ServerHealth> {
            let Ok(mut inner) = self.inner.lock() else {
                return Vec::new();
            };
            let mut out = Vec::new();
            for st in inner.states.values_mut() {
                let live = st.conn.as_mut().is_some_and(|c| c.alive());
                if st.status == ServerStatus::Ready && !live {
                    st.status = ServerStatus::Failed;
                    // A dead server keeps no tool count: live.json must
                    // not keep advertising tools it can no longer serve.
                    st.tools.clear();
                    st.last_error = Some("connection lost".to_string());
                }
                out.push(health_of(st));
            }
            out.sort_by(|a, b| a.name.cmp(&b.name));
            drop(inner);
            self.write_live_state();
            out
        }

        /// Real health check: cheap liveness first, then a `tools/list`
        /// round-trip on every ready server. Updates status, tool counts,
        /// and the live state file.
        pub fn check_health(&self) -> Vec<ServerHealth> {
            {
                let Ok(mut inner) = self.inner.lock() else {
                    return Vec::new();
                };
                let names: Vec<String> = inner.specs.iter().map(|s| s.name.clone()).collect();
                let scope = inner.scope_dir();
                for name in names {
                    let live = inner
                        .states
                        .get_mut(&name)
                        .and_then(|st| st.conn.as_mut())
                        .is_some_and(|c| c.alive());
                    let needs_probe = {
                        let st = inner.states.get(&name).expect("state");
                        (st.status == ServerStatus::Ready && !live)
                            || st.status == ServerStatus::Failed
                            || st.status == ServerStatus::Backoff
                    };
                    if needs_probe {
                        // ensure_connected_inner respects backoff; a skipped
                        // attempt is not a new failure.
                        let _ = Self::ensure_connected_inner(&mut inner, &name);
                    }
                    let probe = {
                        // Gate the probe like every other execution path:
                        // a revoked-but-still-connected server must not
                        // be probed past its revoked approval.
                        if Self::gate_approval(&mut inner.states, &scope, &name, false).is_err() {
                            continue;
                        }
                        let st = match inner.states.get_mut(&name) {
                            Some(s) => s,
                            None => continue,
                        };
                        if st.status != ServerStatus::Ready {
                            continue;
                        }
                        match st.conn.as_mut() {
                            Some(c) => c.list_tools(),
                            None => continue,
                        }
                    };
                    match probe {
                        Ok(tools) => {
                            let st = inner.states.get_mut(&name).expect("state");
                            st.tools = tools;
                            // `failures` counts consecutive failures: a
                            // successful probe resets it, so a recovered
                            // server cannot cross the repair threshold
                            // on stale failures.
                            st.failures = 0;
                            st.last_error = None;
                        }
                        Err(e) => {
                            let st = inner.states.get_mut(&name).expect("state");
                            Self::note_failure(st, e.to_string());
                        }
                    }
                }
            }
            let h = self.health();
            self.write_live_state();
            h
        }

        /// Force one real connect attempt for `name`, bypassing the reconnect
        /// backoff cooldown. This is the nightly repair loop's flapping-server
        /// second chance: `check_health` and the lazy paths respect
        /// `not_before`, so a server still inside its cooldown would never
        /// actually be retried. A failed attempt re-arms the backoff, exactly
        /// like the other connect paths.
        ///
        /// A forced reconnect is still a (re)connect: the approval gate runs
        /// (#5) - a missing, lapsed, or revoked approval refuses the attempt
        /// with an error naming the server.
        pub fn retry_now(&self, name: &str) -> Result<(), McpError> {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| McpError::Io("mcp manager lock poisoned".to_string()))?;
            let scope = inner.scope_dir();
            let resolver = inner.env_resolver.clone();
            let policy = inner.policy.clone();
            let spec = {
                let st = inner.states.get_mut(name).ok_or_else(|| {
                    McpError::Protocol(format!("mcp server '{name}' is not configured"))
                })?;
                if !st.spec.enabled {
                    st.status = ServerStatus::Disabled;
                    return Err(McpError::Unavailable { retry_in_secs: 0 });
                }
                // Clear the cooldown so this is a real attempt, not a skip.
                st.not_before = None;
                st.conn = None;
                st.status = ServerStatus::Connecting;
                st.spec.clone()
            };
            Self::gate_approval(&mut inner.states, &scope, name, true)?;
            let secrets = inner.secrets.clone();
            let elicitation = inner.elicitation.clone();
            let result = match Self::connect_new(
                &spec,
                &resolver,
                &policy,
                secrets.as_ref(),
                elicitation.as_ref(),
            ) {
                Ok(conn) => {
                    let st = inner.states.get_mut(name).expect("state");
                    st.conn = Some(conn);
                    st.status = ServerStatus::Ready;
                    st.connects += 1;
                    st.failures = 0;
                    st.not_before = None;
                    st.last_error = None;
                    Ok(())
                }
                Err(e) => {
                    let st = inner.states.get_mut(name).expect("state");
                    Self::note_failure(st, e.to_string());
                    Err(e)
                }
            };
            // Live state must not keep showing the pre-attempt tools /
            // status after a failed (or successful) forced retry.
            drop(inner);
            self.write_live_state();
            result
        }

        /// Shut every server down: kill children, drop streams. Idempotent.
        pub fn shutdown(&self) {
            let Ok(mut inner) = self.inner.lock() else {
                return;
            };
            for st in inner.states.values_mut() {
                if let Some(mut c) = st.conn.take() {
                    c.shutdown();
                }
                if st.status == ServerStatus::Ready {
                    st.status = ServerStatus::Failed;
                    st.last_error = Some("shut down".to_string());
                }
            }
            drop(inner);
            self.write_live_state();
        }

        /// Best-effort snapshot for the TUI and dashboard
        /// (`<data_dir>/mcp/live.json`). Never fails the caller.
        pub fn write_live_state(&self) {
            let Ok(inner) = self.inner.lock() else {
                return;
            };
            let servers: Vec<serde_json::Value> = {
                let mut v: Vec<serde_json::Value> = inner
                    .states
                    .values()
                    .map(|st| {
                        serde_json::json!({
                            "name": st.spec.name,
                            "transport": st.spec.transport.as_str(),
                            "target": st.spec.target(),
                            "status": st.status.as_str(),
                            "tools": st.tools.len(),
                            "connects": st.connects,
                            "failures": st.failures,
                            "last_error": st.last_error,
                        })
                    })
                    .collect();
                v.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
                v
            };
            let doc = serde_json::json!({
                "updated_at_ms": now_ms(),
                "servers": servers,
            });
            let path = inner.live_path();
            drop(inner);
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(text) = serde_json::to_string_pretty(&doc) {
                let tmp = path.with_extension("json.tmp");
                if std::fs::write(&tmp, text).is_ok() {
                    let _ = std::fs::rename(&tmp, &path);
                }
            }
        }

        /// Approval gate for every path that can execute server code (#5).
        ///
        /// Consults the approval store - the same store and record shape
        /// `register_names` uses at registration time, no parallel store
        /// and refuses with an error naming the server when the approval is
        /// missing, lapsed, or revoked.
        ///
        /// Only the canonical bundled recipe skips the gate: the `enabled`
        /// flag is its only gate (mirrors the dashboard's "enabled AND
        /// (bundled OR approved)" model and the bundled approve-endpoint
        /// exemption). A spec that borrows a bundled name but differs from
        /// the recipe is refused like any unapproved custom server, with the
        /// name collision called out in the message.
        ///
        /// When `reverify` is true (the (re)connect path) the content hash is
        /// recomputed from the current spec first, so a content change lapses
        /// the approval even if the cached hash is stale. Otherwise the
        /// configure-time hash is reused (cheap) - revocation is still caught
        /// by the store lookup below.
        ///
        /// On refusal the server is marked `Unapproved` and any live
        /// connection is dropped, so a revoked server cannot keep executing
        /// on a stale connection.
        fn gate_approval(
            states: &mut HashMap<String, ServerState>,
            scope: &Path,
            name: &str,
            reverify: bool,
        ) -> Result<(), McpError> {
            let spec = {
                let st = states.get(name).ok_or_else(|| {
                    McpError::Protocol(format!("mcp server '{name}' is not configured"))
                })?;
                // Only the canonical bundled recipe skips the gate: the
                // enabled flag is its only gate. A name-colliding spec falls
                // through to full approval below.
                if is_canonical_bundled(&st.spec) {
                    return Ok(());
                }
                st.spec.clone()
            };
            if reverify {
                match content_hash(&spec) {
                    Ok(h) => {
                        let st = states.get_mut(name).expect("checked above");
                        st.hash = Some(h);
                        st.hash_error = None;
                    }
                    Err(e) => {
                        let msg = format!(
                        "mcp server '{name}': cannot verify server content ({e}); refusing to connect"
                    );
                        let st = states.get_mut(name).expect("checked above");
                        st.hash = None;
                        st.hash_error = Some(e.to_string());
                        Self::mark_unapproved(st, msg.clone());
                        return Err(McpError::Protocol(msg));
                    }
                }
            }
            let hash = states.get(name).expect("checked above").hash.clone();
            let hash = match hash {
                Some(h) => h,
                None => {
                    let msg = format!(
                        "mcp server '{name}': cannot verify server content; refusing to connect"
                    );
                    let st = states.get_mut(name).expect("checked above");
                    Self::mark_unapproved(st, msg.clone());
                    return Err(McpError::Protocol(msg));
                }
            };
            let approved = ApprovalStore::open(scope)
                .get(name)
                .is_some_and(|r| r.dir_hash == hash);
            if !approved {
                let collision = if is_bundled_name_collision(&spec) {
                    format!(
                        " (name '{name}' collides with a bundled catalog recipe but the \
                     spec does not match the canonical recipe, so it is treated as a \
                     custom server)"
                    )
                } else {
                    String::new()
                };
                let msg = format!(
                    "mcp server '{name}' is not approved for its current content \
                 (approval missing, lapsed, or revoked); refusing to connect{collision}"
                );
                let st = states.get_mut(name).expect("checked above");
                Self::mark_unapproved(st, msg.clone());
                return Err(McpError::Protocol(msg));
            }
            Ok(())
        }

        /// Drop any live connection, mark `Unapproved`, record why.
        fn mark_unapproved(st: &mut ServerState, msg: String) {
            if let Some(mut c) = st.conn.take() {
                c.shutdown();
            }
            st.status = ServerStatus::Unapproved;
            st.last_error = Some(msg);
        }

        /// Ensure `name` is connected, connecting with backoff when needed.
        /// Caller holds the lock; blocking I/O happens under it (one
        /// in-flight operation per manager, by design).
        ///
        /// Every call passes the approval gate first (#5): even with a live
        /// connection the store is consulted, so a revoked approval blocks
        /// execution instead of riding a stale connection. On (re)connect
        /// the content hash is re-verified, so a content change lapses the
        /// approval.
        fn ensure_connected_inner(inner: &mut Inner, name: &str) -> Result<(), McpError> {
            let resolver = inner.env_resolver.clone();
            let policy = inner.policy.clone();
            let scope = inner.scope_dir();
            // Probe liveness without holding the state borrow across the
            // approval gate below.
            let alive = {
                let st = inner.states.get_mut(name).ok_or_else(|| {
                    McpError::Protocol(format!("mcp server '{name}' is not configured"))
                })?;
                if !st.spec.enabled {
                    st.status = ServerStatus::Disabled;
                    return Err(McpError::Unavailable { retry_in_secs: 0 });
                }
                match st.conn.as_mut() {
                    Some(c) => {
                        if c.alive() {
                            st.status = ServerStatus::Ready;
                            true
                        } else {
                            st.conn = None;
                            false
                        }
                    }
                    None => false,
                }
            };
            Self::gate_approval(&mut inner.states, &scope, name, false)?;
            if alive {
                return Ok(());
            }
            let st = inner.states.get_mut(name).expect("checked above");
            if let Some(not_before) = st.not_before {
                if Instant::now() < not_before {
                    st.status = ServerStatus::Backoff;
                    let secs = not_before
                        .saturating_duration_since(Instant::now())
                        .as_secs();
                    return Err(McpError::Unavailable {
                        retry_in_secs: secs,
                    });
                }
            }
            Self::gate_approval(&mut inner.states, &scope, name, true)?;
            let spec = {
                let st = inner.states.get_mut(name).expect("checked above");
                st.status = ServerStatus::Connecting;
                st.spec.clone()
            };
            let secrets = inner.secrets.clone();
            let elicitation = inner.elicitation.clone();
            let result = Self::connect_new(
                &spec,
                &resolver,
                &policy,
                secrets.as_ref(),
                elicitation.as_ref(),
            );
            let st = inner.states.get_mut(name).expect("checked above");
            match result {
                Ok(conn) => {
                    st.conn = Some(conn);
                    st.status = ServerStatus::Ready;
                    st.connects += 1;
                    st.failures = 0;
                    st.not_before = None;
                    st.last_error = None;
                    Ok(())
                }
                Err(e) => {
                    Self::note_failure(st, e.to_string());
                    Err(e)
                }
            }
        }

        fn note_failure(st: &mut ServerState, msg: String) {
            st.conn = None;
            st.failures += 1;
            st.not_before = Some(Instant::now() + backoff_delay(st.failures));
            st.status = ServerStatus::Failed;
            st.last_error = Some(msg);
        }

        /// Connect one spec: resolve `env:NAME` refs, then dispatch on
        /// transport. Env *values* never appear in errors - only names.
        fn connect_new(
            spec: &McpServerSpec,
            resolver: &Option<EnvResolver>,
            policy: &Option<Policy>,
            secrets: Option<&SecretsBroker>,
            elicitation: Option<&crate::client::ElicitationHandler>,
        ) -> Result<Box<dyn McpConn>, McpError> {
            match spec.transport {
                McpTransport::Stdio => {
                    let command = spec
                        .command
                        .clone()
                        .filter(|c| !c.trim().is_empty())
                        .ok_or_else(|| {
                            McpError::Protocol(format!(
                                "mcp server '{}': stdio transport needs a command",
                                spec.name
                            ))
                        })?;
                    let mut env = HashMap::new();
                    for (k, v) in &spec.env {
                        let value = match v.strip_prefix("env:") {
                            Some(var) => resolver
                                .as_ref()
                                .and_then(|r| r(var))
                                .or_else(|| std::env::var(var).ok())
                                .filter(|s| !s.is_empty())
                                .ok_or_else(|| {
                                    McpError::Io(format!(
                                        "mcp server '{}': secret env var '{var}' did not resolve",
                                        spec.name
                                    ))
                                })?,
                            None => v.clone(),
                        };
                        env.insert(k.clone(), value);
                    }
                    let cfg = McpServerConfig::new(&spec.name, command)
                        .with_args(spec.args.clone())
                        .with_env(env)
                        .with_timeout(spec.timeout);
                    Ok(Box::new(McpClient::connect_with(
                        &cfg,
                        policy.as_ref(),
                        elicitation.cloned(),
                    )?))
                }
                McpTransport::Sse | McpTransport::Http => {
                    let url = spec
                        .url
                        .clone()
                        .filter(|u| !u.trim().is_empty())
                        .ok_or_else(|| {
                            McpError::Protocol(format!(
                                "mcp server '{}': {} transport needs a url",
                                spec.name,
                                spec.transport.as_str()
                            ))
                        })?;
                    let t = match spec.transport {
                        McpTransport::Sse => HttpTransport::Sse,
                        _ => HttpTransport::Streamable,
                    };
                    // Resolve header `env:NAME` refs at connect time
                    // through the broker-first resolver (the stdio env
                    // path already does this): a vault-only token must
                    // reach the server, and an unresolvable ref fails
                    // loudly here instead of surfacing as a later 401.
                    let mut headers: Vec<(String, String)> = Vec::new();
                    for (k, v) in &spec.headers {
                        let value = match v.strip_prefix("env:") {
                            Some(var) => resolver
                                .as_ref()
                                .and_then(|r| r(var))
                                .or_else(|| std::env::var(var).ok())
                                .filter(|s| !s.is_empty())
                                .ok_or_else(|| {
                                    McpError::Io(format!(
                                    "mcp server '{}': header '{k}' env var '{var}' did not resolve",
                                    spec.name
                                ))
                                })?,
                            None => v.clone(),
                        };
                        headers.push((k.clone(), value));
                    }
                    headers.sort();
                    let has_auth = headers
                        .iter()
                        .any(|(k, _)| k.eq_ignore_ascii_case("authorization"));
                    if spec.auth == McpAuth::OAuth && !has_auth {
                        let broker = secrets.ok_or_else(|| {
                        McpError::Unauthorized(format!(
                            "mcp server '{}': no secret store is installed for OAuth; run `pantheon mcp login {}` from the CLI",
                            spec.name, spec.name
                        ))
                    })?;
                        let (token, token_type) = crate::oauth::ensure_token(spec, broker)?;
                        // Honour the stored token type (default Bearer):
                        // a server that issued e.g. `DPoP` or `mac` must
                        // not be sent a Bearer scheme it did not issue.
                        let scheme = if token_type.trim().is_empty() {
                            "Bearer".to_string()
                        } else {
                            token_type
                        };
                        headers.push(("Authorization".to_string(), format!("{scheme} {token}")));
                    }
                    Ok(Box::new(HttpMcpClient::connect_with(
                        &spec.name,
                        t,
                        &url,
                        spec.timeout,
                        &headers,
                        elicitation.cloned(),
                    )?))
                }
            }
        }
    }

    fn health_of(st: &ServerState) -> ServerHealth {
        ServerHealth {
            name: st.spec.name.clone(),
            transport: st.spec.transport.as_str(),
            target: st.spec.target(),
            status: st.status,
            tools: st.tools.len(),
            connects: st.connects,
            failures: st.failures,
            last_error: st.last_error.clone(),
        }
    }

    // Small deterministic invariants only: naming, sanitizing, backoff,
    // spec equality. Anything with a subprocess, socket, or clock lives in
    // `pantheon-eval` (`eval/tests/mcp_manager.rs`).

    #[cfg(test)]
    mod capability_resolution_tests {
        use super::*;

        fn empty() -> McpCapabilityOverrides {
            McpCapabilityOverrides::default()
        }

        #[test]
        fn unclassified_tools_park_for_approval() {
            // No override and no built-in inventory entry: the tool must not
            // ride an allowed capability. `McpUnclassified` is parked by the
            // coder policy, so the run stops for a human before a
            // third-party tool it never classified can write/send/delete.
            assert_eq!(
                resolve_tool_capability(&empty(), "github", "get_issue"),
                Capability::McpUnclassified
            );
            assert_eq!(
                resolve_tool_capability(&empty(), "anything", "whatever"),
                Capability::McpUnclassified
            );
            assert_eq!(
                pantheon_api::capability::Policy::coder().check(&Capability::McpUnclassified),
                pantheon_api::capability::Decision::Approval
            );
        }

        #[test]
        fn caller_server_override_beats_default() {
            // The CUA driver pattern: whole-server override.
            let o = McpCapabilityOverrides::server_level("cua", Capability::ComputerUse);
            assert_eq!(
                resolve_tool_capability(&o, "cua", "click"),
                Capability::ComputerUse
            );
            // Other servers keep the unclassified default (parked).
            assert_eq!(
                resolve_tool_capability(&o, "github", "get_issue"),
                Capability::McpUnclassified
            );
        }

        #[test]
        fn caller_per_tool_beats_caller_server() {
            let mut o = McpCapabilityOverrides::server_level("srv", Capability::ComputerUse);
            o.tool.insert(
                ("srv".to_string(), "read_only_thing".to_string()),
                Capability::FilesystemRead,
            );
            assert_eq!(
                resolve_tool_capability(&o, "srv", "read_only_thing"),
                Capability::FilesystemRead
            );
            assert_eq!(
                resolve_tool_capability(&o, "srv", "other_tool"),
                Capability::ComputerUse
            );
        }

        #[test]
        fn builtin_rea_capability_reflects_external_effects() {
            assert_eq!(
                resolve_tool_capability_for_version(
                    &empty(),
                    "rea",
                    "goto_address",
                    Some(crate::rea_audit::REA_PINNED_VERSION),
                ),
                Capability::FilesystemRead
            );
            assert_eq!(
                resolve_tool_capability_for_version(
                    &empty(),
                    "rea",
                    "capture_process_scenario",
                    Some(crate::rea_audit::REA_PINNED_VERSION),
                ),
                Capability::BinaryTrace
            );
        }

        #[test]
        fn builtin_rea_tracing_is_binary_trace() {
            assert_eq!(
                resolve_tool_capability_for_version(
                    &empty(),
                    "rea",
                    "capture_process_scenario",
                    Some(crate::rea_audit::REA_PINNED_VERSION),
                ),
                Capability::BinaryTrace
            );
        }

        #[test]
        fn unknown_rea_tool_falls_back_to_restrictive() {
            // Fail-closed: a tool REA adds in a future version parks for
            // approval instead of inheriting the permissive default.
            assert_eq!(
                resolve_tool_capability(&empty(), "rea", "some_future_tool"),
                Capability::BinaryTrace
            );
        }

        #[test]
        fn caller_override_beats_builtin_rea() {
            // An operator who explicitly classifies a REA tool wins over the
            // built-in catalogue (e.g. to tighten further).
            let mut o = McpCapabilityOverrides::default();
            o.tool.insert(
                ("rea".to_string(), "goto_address".to_string()),
                Capability::BinaryTrace,
            );
            assert_eq!(
                resolve_tool_capability(&o, "rea", "goto_address"),
                Capability::BinaryTrace
            );
            // Unmentioned REA tools still use the builtin classification.
            assert_eq!(
                resolve_tool_capability_for_version(
                    &o,
                    "rea",
                    "list_unknowns",
                    Some(crate::rea_audit::REA_PINNED_VERSION),
                ),
                Capability::FilesystemRead
            );
        }

        #[test]
        fn similar_names_do_not_cross_match() {
            // Tuple-keyed lookup: no string splitting, so near-miss names
            // cannot accidentally inherit another entry's capability.
            assert_eq!(
                resolve_tool_capability(&empty(), "rea2", "goto_address"),
                Capability::McpUnclassified
            );
            assert_eq!(
                resolve_tool_capability(&empty(), "myrea", "trace_call_path"),
                Capability::McpUnclassified
            );
            // A server literally named "rea::x" is a different server.
            assert_eq!(
                resolve_tool_capability(&empty(), "rea::x", "goto_address"),
                Capability::McpUnclassified
            );
        }

        #[test]
        fn empty_tool_name_does_not_match() {
            assert_eq!(
                resolve_tool_capability(&empty(), "rea", ""),
                Capability::BinaryTrace
            );
            assert_eq!(
                resolve_tool_capability(&empty(), "", "goto_address"),
                Capability::McpUnclassified
            );
        }
    }
}

pub mod oauth {
    //! MCP OAuth 2.1: discovery, dynamic client registration, PKCE, and
    //! encrypted token storage.
    //!
    //! Remote MCP servers advertise protected-resource metadata (RFC 9728)
    //! and authorization-server metadata (RFC 8414). Pantheon discovers both,
    //! registers a public client with the authorization server when no
    //! `client_id` is configured, runs the RFC 8252 loopback authorization
    //! code flow with PKCE S256, and stores the resulting token set through
    //! the same [`SecretsBroker`] the rest of Pantheon uses (OS keychain, or
    //! the encrypted file vault on headless hosts).
    //!
    //! This module deliberately has no UI. `pantheon mcp login <server>` runs
    //! the interactive flow; runtime connects only load and refresh tokens.
    //! That split keeps a browser prompt from blocking an agent turn and makes
    //! the failure mode explicit: an unauthenticated server reports
    //! [`McpError::Unauthorized`] with the login command to run.

    use crate::manager::{McpAuth, McpServerSpec};
    use crate::McpError;
    use pantheon_secrets::oauth::pkce_pair;
    use pantheon_secrets::{SecretValue, SecretsBroker};
    use serde::{Deserialize, Serialize};
    use serde_json::Value;
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
    const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

    #[derive(Debug, Clone, Default)]
    struct ResourceMetadata {
        resource: Option<String>,
        authorization_servers: Vec<String>,
        scopes_supported: Vec<String>,
    }

    #[derive(Debug, Clone)]
    struct AuthServerMetadata {
        issuer: String,
        authorization_endpoint: String,
        token_endpoint: String,
        registration_endpoint: Option<String>,
        scopes_supported: Vec<String>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct StoredMeta {
        /// Server name is stored so a sanitized-key collision cannot make one
        /// server read another server's tokens.
        server: String,
        expires_at: Option<u64>,
        token_endpoint: String,
        client_id: String,
        scopes: Vec<String>,
        resource: Option<String>,
        token_type: String,
    }

    /// A stored OAuth token: access token, refresh token (optional), metadata,
    /// and client secret (optional). Returned by [`TokenStore::load`].
    struct StoredToken {
        access: String,
        refresh: Option<String>,
        meta: StoredMeta,
        secret: Option<String>,
    }

    struct TokenStore<'a> {
        broker: &'a SecretsBroker,
        prefix: String,
    }

    impl<'a> TokenStore<'a> {
        fn new(broker: &'a SecretsBroker, server: &str) -> Self {
            // Secrets names are [A-Za-z0-9_]. The FNV suffix keeps `a-b` and
            // `a_b` from sharing a token slot while staying human-readable.
            let mut hash: u64 = 0xcbf29ce484222325;
            for b in server.as_bytes() {
                hash ^= u64::from(*b);
                hash = hash.wrapping_mul(0x100000001b3);
            }
            Self {
                broker,
                prefix: format!(
                    "mcp_oauth_{}_{hash:016x}",
                    crate::manager::sanitize_segment(server)
                ),
            }
        }

        fn key(&self, field: &str) -> String {
            format!("{}_{field}", self.prefix)
        }

        fn save(
            &self,
            access_token: &str,
            refresh_token: Option<&str>,
            meta: &StoredMeta,
            client_secret: Option<&str>,
        ) -> Result<(), McpError> {
            self.set_secret(&self.key("access_token"), access_token)?;
            match refresh_token {
                Some(t) if !t.is_empty() => self.set_secret(&self.key("refresh_token"), t)?,
                _ => self.delete_secret(&self.key("refresh_token"))?,
            }
            match client_secret {
                Some(s) if !s.is_empty() => self.set_secret(&self.key("client_secret"), s)?,
                _ => self.delete_secret(&self.key("client_secret"))?,
            }
            let json = serde_json::to_string(meta)
                .map_err(|e| McpError::Protocol(format!("oauth metadata encode: {e}")))?;
            self.set_secret(&self.key("meta"), &json)
        }

        fn set_secret(&self, key: &str, value: &str) -> Result<(), McpError> {
            self.broker
            .set(key, SecretValue::new(value.to_string()))
            .map_err(|e| {
                McpError::Unauthorized(format!(
                    "cannot store MCP OAuth token ({e}); run `pantheon mcp login` on a host with a durable vault"
                ))
            })
        }

        fn delete_secret(&self, key: &str) -> Result<(), McpError> {
            self.broker
                .delete(key)
                .map_err(|e| McpError::Protocol(format!("oauth token delete: {e}")))
        }

        fn load(&self) -> Result<Option<StoredToken>, McpError> {
            let Some(access) = self
                .broker
                .resolve(&self.key("access_token"))
                .map_err(|e| McpError::Protocol(format!("oauth token read: {e}")))?
            else {
                return Ok(None);
            };
            let Some(meta) = self
                .broker
                .resolve(&self.key("meta"))
                .map_err(|e| McpError::Protocol(format!("oauth metadata read: {e}")))?
            else {
                return Ok(None);
            };
            let meta: StoredMeta = serde_json::from_str(meta.expose())
                .map_err(|e| McpError::Protocol(format!("oauth metadata corrupt: {e}")))?;
            let refresh = self
                .broker
                .resolve(&self.key("refresh_token"))
                .map_err(|e| McpError::Protocol(format!("oauth refresh read: {e}")))?
                .map(|v| v.expose().to_string());
            let secret = self
                .broker
                .resolve(&self.key("client_secret"))
                .map_err(|e| McpError::Protocol(format!("oauth secret read: {e}")))?
                .map(|v| v.expose().to_string());
            Ok(Some(StoredToken {
                access: access.expose().to_string(),
                refresh,
                meta,
                secret,
            }))
        }
    }

    /// Resolve a usable access token for a remote server, refreshing it
    /// when needed, plus the stored token type (default `Bearer`).
    /// `None`/expired-without-refresh is an actionable auth error.
    /// Callers must honour the token type: sending `Bearer` for a token
    /// the server issued as another scheme is an auth failure.
    pub fn ensure_token(
        spec: &McpServerSpec,
        broker: &SecretsBroker,
    ) -> Result<(String, String), McpError> {
        if spec.auth != McpAuth::OAuth {
            return Err(McpError::Unauthorized(format!(
                "mcp server '{}': OAuth mode is not enabled",
                spec.name
            )));
        }
        let store = TokenStore::new(broker, &spec.name);
        let Some(tok) = store.load()? else {
            return Err(McpError::Unauthorized(format!(
                "mcp server '{}' is not authenticated; run `pantheon mcp login {}`",
                spec.name, spec.name
            )));
        };
        let access = tok.access;
        let refresh = tok.refresh;
        let meta = tok.meta;
        let client_secret = tok.secret;
        if meta.server != spec.name {
            return Err(McpError::Unauthorized(format!(
                "mcp server '{}': stored OAuth token belongs to '{}'; run `pantheon mcp login {}`",
                spec.name, meta.server, spec.name
            )));
        }
        if !expired(meta.expires_at) {
            return Ok((access, meta.token_type));
        }
        let Some(refresh) = refresh else {
            return Err(McpError::Unauthorized(format!(
            "mcp server '{}' OAuth token expired and no refresh token is stored; run `pantheon mcp login {}`",
            spec.name, spec.name
        )));
        };
        let (new_access, new_refresh) = refresh_token(&meta, &refresh, client_secret.as_deref())
            .map_err(|e| {
                McpError::Unauthorized(format!(
                    "mcp server '{}' OAuth refresh failed: {e}; run `pantheon mcp login {}`",
                    spec.name, spec.name
                ))
            })?;
        let mut next_meta = meta;
        next_meta.expires_at = expires_at_from_response(&new_access);
        // A refresh can re-issue the token under a different scheme;
        // keep the stored type in step with the live token.
        next_meta.token_type = new_access.token_type.clone();
        store.save(
            &new_access.access_token,
            new_refresh.as_deref().or(Some(&refresh)),
            &next_meta,
            client_secret.as_deref(),
        )?;
        Ok((new_access.access_token, new_access.token_type))
    }

    /// Interactive login for `pantheon mcp login <server>`.
    pub fn login(spec: &McpServerSpec, broker: &SecretsBroker) -> Result<(), McpError> {
        if spec.auth != McpAuth::OAuth {
            return Err(McpError::Unauthorized(format!(
                "mcp server '{}' does not set auth = \"oauth\"",
                spec.name
            )));
        }
        let resource_url = spec
            .url
            .as_deref()
            .filter(|u| !u.trim().is_empty())
            .ok_or_else(|| McpError::Unauthorized("OAuth requires a remote url".into()))?;
        let resource = discover_resource(resource_url)?;
        let auth_server = resource
            .authorization_servers
            .first()
            .cloned()
            .unwrap_or_else(|| origin(resource_url).unwrap_or_else(|| resource_url.to_string()));
        let auth = discover_auth_server(&auth_server)?;

        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|e| McpError::Io(format!("oauth callback bind: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| McpError::Io(format!("oauth callback address: {e}")))?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}/callback");

        let (client_id, client_secret) = match spec.client_id.clone() {
            Some(id) if !id.trim().is_empty() => (id, None),
            _ => register_client(&auth, &redirect_uri)?,
        };
        let (verifier, challenge) = pkce_pair();
        let state = random_state();
        let scopes = if !spec.scopes.is_empty() {
            spec.scopes.clone()
        } else if !resource.scopes_supported.is_empty() {
            resource.scopes_supported.clone()
        } else {
            auth.scopes_supported.clone()
        };
        let resource_param = resource
            .resource
            .clone()
            .unwrap_or_else(|| resource_url.to_string());

        let mut url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256",
        auth.authorization_endpoint,
        url_encode(&client_id),
        url_encode(&redirect_uri),
        url_encode(&state),
        url_encode(&challenge),
    );
        if !scopes.is_empty() {
            url.push_str(&format!("&scope={}", url_encode(&scopes.join(" "))));
        }
        url.push_str(&format!("&resource={}", url_encode(&resource_param)));

        eprintln!(
        "Opening the browser for MCP server '{}'.\nIf it does not open, visit:\n{url}\n\nWaiting for the callback on {redirect_uri} ...",
        spec.name
    );
        open_browser(&url);
        let code = wait_for_code(&listener, &state)?;
        let token = exchange_code(
            &auth.token_endpoint,
            &client_id,
            client_secret.as_deref(),
            &code,
            &redirect_uri,
            &verifier,
        )?;
        let meta = StoredMeta {
            server: spec.name.clone(),
            expires_at: token.expires_at,
            token_endpoint: auth.token_endpoint.clone(),
            client_id: client_id.clone(),
            scopes,
            resource: Some(resource_param),
            token_type: token.token_type.clone(),
        };
        TokenStore::new(broker, &spec.name).save(
            &token.access_token,
            token.refresh_token.as_deref(),
            &meta,
            client_secret.as_deref(),
        )?;
        eprintln!("MCP server '{}' is authenticated.", spec.name);
        Ok(())
    }

    /// Remove every token/registration record for a server.
    pub fn logout(spec: &McpServerSpec, broker: &SecretsBroker) -> Result<(), McpError> {
        let store = TokenStore::new(broker, &spec.name);
        for field in ["access_token", "refresh_token", "meta", "client_secret"] {
            store.delete_secret(&store.key(field))?;
        }
        Ok(())
    }

    /// `(connected, scopes, expires_at)` without exposing token bytes.
    pub fn status(
        spec: &McpServerSpec,
        broker: &SecretsBroker,
    ) -> Result<(bool, Vec<String>, Option<u64>), McpError> {
        if spec.auth != McpAuth::OAuth {
            return Ok((false, Vec::new(), None));
        }
        let store = TokenStore::new(broker, &spec.name);
        match store.load()? {
            Some(tok) if tok.meta.server == spec.name => {
                Ok((true, tok.meta.scopes, tok.meta.expires_at))
            }
            _ => Ok((false, Vec::new(), None)),
        }
    }

    // ---------------------------------------------------------------------------
    // Discovery
    // ---------------------------------------------------------------------------

    fn discover_resource(resource_url: &str) -> Result<ResourceMetadata, McpError> {
        let base = origin(resource_url)
            .ok_or_else(|| McpError::Unauthorized(format!("invalid MCP url {resource_url:?}")))?;
        let path = path_of(resource_url);
        let mut candidates = vec![format!("{base}/.well-known/oauth-protected-resource")];
        if !path.is_empty() && path != "/" {
            candidates.push(format!(
                "{base}/.well-known/oauth-protected-resource{}",
                path.trim_end_matches('/')
            ));
        }
        for url in candidates {
            if let Some(v) = get_json_opt(&url)? {
                return Ok(resource_from(&v));
            }
        }
        // RFC 9728 also allows the resource to point at its metadata in a
        // WWW-Authenticate challenge. Probe the MCP endpoint once; a 401 is
        // expected and carries the pointer when the server implements it.
        // Statuses surface as responses (ureq 3), so one Ok arm covers the
        // 401 too; only a transport failure loses the challenge.
        let agent = http_agent();
        let challenge = match agent.get(resource_url).call() {
            Ok(resp) => resp
                .headers()
                .get("WWW-Authenticate")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
            Err(_) => None,
        };
        if let Some(challenge) = challenge {
            if let Some(url) = header_param(&challenge, "resource_metadata") {
                if let Some(v) = get_json_opt(&url)? {
                    return Ok(resource_from(&v));
                }
            }
        }
        Ok(ResourceMetadata::default())
    }

    fn discover_auth_server(issuer: &str) -> Result<AuthServerMetadata, McpError> {
        let base = origin(issuer).ok_or_else(|| {
            McpError::Unauthorized(format!("invalid authorization server {issuer:?}"))
        })?;
        let path = path_of(issuer);
        let mut candidates = vec![format!("{base}/.well-known/oauth-authorization-server")];
        if !path.is_empty() && path != "/" {
            candidates.push(format!(
                "{base}/.well-known/oauth-authorization-server{}",
                path.trim_end_matches('/')
            ));
        }
        candidates.push(format!("{base}/.well-known/openid-configuration"));
        for url in candidates {
            if let Some(v) = get_json_opt(&url)? {
                let authorization_endpoint = str_field(&v, "authorization_endpoint");
                let token_endpoint = str_field(&v, "token_endpoint");
                if let (Some(a), Some(t)) = (authorization_endpoint, token_endpoint) {
                    let issuer = str_field(&v, "issuer").unwrap_or_else(|| issuer.to_string());
                    return Ok(AuthServerMetadata {
                        issuer,
                        authorization_endpoint: a,
                        token_endpoint: t,
                        registration_endpoint: str_field(&v, "registration_endpoint"),
                        scopes_supported: string_array(&v, "scopes_supported"),
                    });
                }
            }
        }
        Err(McpError::Unauthorized(format!(
        "no OAuth authorization-server metadata at {issuer}; check the server URL or set auth = \"none\" with explicit headers"
    )))
    }

    fn resource_from(v: &Value) -> ResourceMetadata {
        ResourceMetadata {
            resource: str_field(v, "resource"),
            authorization_servers: string_array(v, "authorization_servers"),
            scopes_supported: string_array(v, "scopes_supported"),
        }
    }

    // ---------------------------------------------------------------------------
    // Dynamic registration + token exchange
    // ---------------------------------------------------------------------------

    fn register_client(
        auth: &AuthServerMetadata,
        redirect_uri: &str,
    ) -> Result<(String, Option<String>), McpError> {
        let endpoint = auth.registration_endpoint.as_deref().ok_or_else(|| {
        McpError::Unauthorized(format!(
            "authorization server {} does not support dynamic client registration; set client_id in [mcp.servers.<name>]",
            auth.issuer
        ))
    })?;
        let body = serde_json::json!({
            "client_name": "Pantheon",
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        });
        let resp = http_agent()
            .post(endpoint)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .send_json(body)
            .map_err(|e| {
                McpError::Unauthorized(format!("dynamic client registration failed: {e}"))
            })?;
        if resp.status().as_u16() >= 400 {
            // Same message the ureq 2 status error displayed.
            let code = resp.status().as_u16();
            return Err(McpError::Unauthorized(format!(
                "dynamic client registration failed: {endpoint}: status code {code}"
            )));
        }
        let v: Value = resp
            .into_body()
            .read_json()
            .map_err(|e| McpError::Protocol(format!("registration response: {e}")))?;
        let client_id = str_field(&v, "client_id")
            .ok_or_else(|| McpError::Protocol("registration response has no client_id".into()))?;
        Ok((client_id, str_field(&v, "client_secret")))
    }

    #[derive(Debug)]
    struct TokenSet {
        access_token: String,
        refresh_token: Option<String>,
        expires_at: Option<u64>,
        token_type: String,
    }

    fn exchange_code(
        token_endpoint: &str,
        client_id: &str,
        client_secret: Option<&str>,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
    ) -> Result<TokenSet, McpError> {
        let mut params = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
            ("code_verifier", verifier),
        ];
        if let Some(secret) = client_secret.filter(|s| !s.is_empty()) {
            params.push(("client_secret", secret));
        }
        token_request(token_endpoint, &params)
    }

    fn refresh_token(
        meta: &StoredMeta,
        refresh: &str,
        client_secret: Option<&str>,
    ) -> Result<(TokenSet, Option<String>), McpError> {
        let mut params = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", meta.client_id.as_str()),
        ];
        if let Some(secret) = client_secret.filter(|s| !s.is_empty()) {
            params.push(("client_secret", secret));
        }
        let token = token_request(&meta.token_endpoint, &params)?;
        let rotated = token.refresh_token.clone();
        Ok((token, rotated))
    }

    fn token_request(endpoint: &str, params: &[(&str, &str)]) -> Result<TokenSet, McpError> {
        let resp = http_agent()
            .post(endpoint)
            .header("Accept", "application/json")
            .send_form(params.iter().copied())
            .map_err(|e| McpError::Unauthorized(format!("token request failed: {e}")))?;
        if resp.status().as_u16() >= 400 {
            let code = resp.status().as_u16();
            let detail = resp.into_body().read_to_string().unwrap_or_default();
            return Err(McpError::Unauthorized(format!(
                "token endpoint returned {code}: {}",
                detail.chars().take(300).collect::<String>()
            )));
        }
        let v: Value = resp
            .into_body()
            .read_json()
            .map_err(|e| McpError::Protocol(format!("token response is not JSON: {e}")))?;
        if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
            let desc = v
                .get("error_description")
                .and_then(|d| d.as_str())
                .unwrap_or("");
            return Err(McpError::Unauthorized(format!(
                "token endpoint error: {err} {desc}"
            )));
        }
        let access_token = str_field(&v, "access_token")
            .ok_or_else(|| McpError::Protocol("token response has no access_token".into()))?;
        Ok(TokenSet {
            access_token,
            refresh_token: str_field(&v, "refresh_token"),
            expires_at: v
                .get("expires_in")
                .and_then(|e| e.as_u64())
                .map(|s| now_secs().saturating_add(s)),
            token_type: str_field(&v, "token_type").unwrap_or_else(|| "Bearer".into()),
        })
    }

    fn expires_at_from_response(token: &TokenSet) -> Option<u64> {
        token.expires_at
    }

    fn expired(at: Option<u64>) -> bool {
        match at {
            Some(at) => now_secs().saturating_add(60) >= at,
            None => false,
        }
    }

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    // ---------------------------------------------------------------------------
    // Loopback callback + small URL/HTTP helpers
    // ---------------------------------------------------------------------------

    fn wait_for_code(listener: &TcpListener, expected_state: &str) -> Result<String, McpError> {
        let deadline = Instant::now() + LOGIN_TIMEOUT;
        listener
            .set_nonblocking(true)
            .map_err(|e| McpError::Io(format!("oauth listener: {e}")))?;
        loop {
            if Instant::now() > deadline {
                return Err(McpError::Timeout {
                    method: "oauth callback".into(),
                });
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    if let Some(code) = handle_callback(stream, expected_state)? {
                        return Ok(code);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(McpError::Io(format!("oauth callback: {e}"))),
            }
        }
    }

    fn handle_callback(
        stream: TcpStream,
        expected_state: &str,
    ) -> Result<Option<String>, McpError> {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| McpError::Io(format!("oauth callback: {e}")))?;
        let mut head = Vec::new();
        {
            let mut reader = BufReader::new(&stream);
            loop {
                let mut line = Vec::new();
                let n = reader
                    .read_until(b'\n', &mut line)
                    .map_err(|e| McpError::Io(format!("oauth callback read: {e}")))?;
                if n == 0 {
                    break;
                }
                head.extend_from_slice(&line);
                if head.len() > 16_384 {
                    return Err(McpError::Protocol("oauth callback head too large".into()));
                }
                if line == b"\r\n" {
                    break;
                }
            }
        }
        let head = String::from_utf8_lossy(&head);
        let target = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("");
        if !target.starts_with("/callback") {
            let _ = respond(&stream, 404, "Not found.");
            return Ok(None);
        }
        let params = parse_query(target.split('?').nth(1).unwrap_or(""));
        if let Some(err) = params.get("error") {
            let _ = respond(&stream, 400, "Authorization failed.");
            return Err(McpError::Unauthorized(format!(
                "authorization server returned error: {err}"
            )));
        }
        match (params.get("code"), params.get("state")) {
            (Some(code), Some(state)) if state == expected_state && !code.is_empty() => {
                let _ = respond(&stream, 200, "Signed in. You can close this tab.");
                Ok(Some(code.clone()))
            }
            _ => {
                let _ = respond(&stream, 400, "Authorization failed.");
                Err(McpError::Unauthorized(
                    "oauth callback missing code or state mismatch (possible CSRF)".into(),
                ))
            }
        }
    }

    fn respond(stream: &TcpStream, status: u16, message: &str) -> Result<(), McpError> {
        let reason = if status == 200 { "OK" } else { "Error" };
        let body = format!(
        "<html><body><h2>{message}</h2><p>You can close this tab and return to Pantheon.</p></body></html>"
    );
        let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
        let mut stream = stream;
        stream
            .write_all(response.as_bytes())
            .map_err(|e| McpError::Io(format!("oauth callback write: {e}")))?;
        stream
            .flush()
            .map_err(|e| McpError::Io(format!("oauth callback flush: {e}")))
    }

    fn http_agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(HTTP_TIMEOUT))
            .max_redirects(5)
            .http_status_as_error(false)
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .new_agent()
    }

    fn get_json_opt(url: &str) -> Result<Option<Value>, McpError> {
        // Statuses never surface as errors on this agent (ureq 3): a 4xx is
        // a miss on the discovery probe, not a failure.
        let resp = http_agent()
            .get(url)
            .header("Accept", "application/json")
            .call()
            .map_err(|e| McpError::Io(format!("{url}: {e}")))?;
        if resp.status().as_u16() >= 400 {
            return Ok(None);
        }
        let v: Value = resp
            .into_body()
            .read_json()
            .map_err(|e| McpError::Protocol(format!("{url}: bad JSON: {e}")))?;
        Ok(Some(v))
    }

    fn origin(url: &str) -> Option<String> {
        let mut parts = url.splitn(4, '/');
        let scheme = parts.next()?.trim_end_matches(':');
        let _empty = parts.next()?;
        let host = parts.next()?;
        if scheme != "http" && scheme != "https" {
            return None;
        }
        if host.is_empty() {
            return None;
        }
        Some(format!("{scheme}://{host}"))
    }

    fn path_of(url: &str) -> &str {
        let rest = url
            .strip_prefix("http://")
            .or_else(|| url.strip_prefix("https://"))
            .unwrap_or(url);
        match rest.find('/') {
            Some(i) => &rest[i..],
            None => "",
        }
    }

    fn str_field(v: &Value, key: &str) -> Option<String> {
        v.get(key)
            .and_then(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    fn string_array(v: &Value, key: &str) -> Vec<String> {
        v.get(key)
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn header_param(header: &str, name: &str) -> Option<String> {
        // `WWW-Authenticate` is a comma-separated auth-param list such as
        // `Bearer resource_metadata="https://..."`. Find the parameter name
        // case-insensitively, then take a quoted string (which may itself
        // contain commas) or a bare token.
        let needle = format!("{}=", name.to_ascii_lowercase());
        let lower = header.to_ascii_lowercase();
        let start = lower.find(&needle)? + needle.len();
        let rest = header.get(start..)?.trim_start();
        if let Some(rest) = rest.strip_prefix('"') {
            let end = rest.find('"')?;
            return Some(rest[..end].to_string());
        }
        let end = rest
            .find(|c: char| c == ',' || c == ';' || c.is_whitespace())
            .unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }

    fn random_state() -> String {
        use rand::RngExt;
        const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        let mut rng = rand::rng();
        (0..32)
            .map(|_| CHARSET[rng.random_range(0..CHARSET.len())] as char)
            .collect()
    }

    fn url_encode(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                out.push(b as char);
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out
    }

    fn url_decode(s: &str) -> String {
        let mut out = Vec::with_capacity(s.len());
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                if let (Some(h), Some(l)) = (
                    (bytes[i + 1] as char).to_digit(16),
                    (bytes[i + 2] as char).to_digit(16),
                ) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                    continue;
                }
            }
            if bytes[i] == b'+' {
                out.push(b' ');
            } else {
                out.push(bytes[i]);
            }
            i += 1;
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
        query
            .split('&')
            .filter_map(|pair| {
                let (k, v) = pair.split_once('=')?;
                Some((url_decode(k), url_decode(v)))
            })
            .collect()
    }

    fn open_browser(url: &str) {
        #[cfg(target_os = "macos")]
        let prog = "open";
        #[cfg(target_os = "windows")]
        let prog = "cmd";
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let prog = "xdg-open";
        #[cfg(target_os = "windows")]
        let result = std::process::Command::new(prog)
            .args(["/c", "start", "", url])
            .spawn();
        #[cfg(not(target_os = "windows"))]
        let result = std::process::Command::new(prog).arg(url).spawn();
        let _ = result;
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn origin_and_path_keep_the_port_but_strip_the_path() {
            assert_eq!(
                origin("https://mcp.example.com:8443/a/b?x=1").as_deref(),
                Some("https://mcp.example.com:8443")
            );
            assert_eq!(path_of("https://mcp.example.com:8443/a/b?x=1"), "/a/b?x=1");
        }

        #[test]
        fn auth_header_param_is_quoted_and_case_insensitive() {
            let header =
                r#"Bearer resource_metadata="https://x/.well-known/oauth-protected-resource""#;
            assert_eq!(
                header_param(header, "resource_metadata").as_deref(),
                Some("https://x/.well-known/oauth-protected-resource")
            );
        }

        #[test]
        fn query_round_trip_handles_spaces_and_slashes() {
            let q = format!("scope={}&state={}", url_encode("a b/c"), url_encode("xyz"));
            let parsed = parse_query(&q);
            assert_eq!(parsed.get("scope").map(String::as_str), Some("a b/c"));
            assert_eq!(parsed.get("state").map(String::as_str), Some("xyz"));
        }

        #[test]
        fn token_store_keys_do_not_collide_on_punctuation() {
            let broker = SecretsBroker::new();
            let a = format!(
                "mcp_oauth_{}_{}",
                crate::manager::sanitize_segment("a-b"),
                0
            );
            let b = format!(
                "mcp_oauth_{}_{}",
                crate::manager::sanitize_segment("a_b"),
                0
            );
            assert_eq!(a, b, "the sanitized prefix alone is not unique");
            let store_a = TokenStore::new(&broker, "a-b");
            let store_b = TokenStore::new(&broker, "a_b");
            assert_ne!(store_a.prefix, store_b.prefix);
        }
    }
}

pub mod rea {
    //! REA ("Reverse Engineer Anything", morluto/rea) tool classification.
    //!
    //! REA is a third-party MCP server that exposes binary reverse-engineering
    //! tools to agents. Its tool catalogue spans two risk tiers:
    //!
    //! * **Static analysis** (read-only): decompile, inspect, and compare
    //!   binaries without executing the target. These map to
    //!   [`Capability::FilesystemRead`] only if the tool also avoids spawning
    //!   processes and accessing the network.
    //! * **External or mutating effects**: process launches, network access,
    //!   writes, and dynamic tracing map to [`Capability::BinaryTrace`], which
    //!   default policies mark Approval so the run parks for a human.
    //!
    //! Classification is generated from the REA 6.5.0 tool effect audit
    //! embedded in [`crate::rea_audit`]: 110 tools, each storing its full
    //! effect vector and the tier derived from it. Anything not in the map
    //! (including tools REA adds in future versions) falls back to the
    //! server-level [`Capability::BinaryTrace`], which parks for approval.
    //! That is the fail-closed direction: a new tool can never silently
    //! inherit a permissive capability.
    //!
    //! The classification keys on the configured server name `"rea"` (the
    //! documented setup). Tool identity uses exact `(server, tool)` tuple
    //! keys built from the authoritative MCP handshake pair — never string
    //! splitting — so a tool cannot accidentally match another server.

    use pantheon_api::capability::Capability;
    use std::collections::HashMap;

    /// Configured server name the REA classification applies to.
    pub const REA_SERVER_NAME: &str = "rea";

    /// Restrictive server-level fallback for REA: any REA tool without an
    /// explicit per-tool classification parks for human approval.
    pub fn rea_server_capability() -> Capability {
        Capability::BinaryTrace
    }

    /// Explicit per-tool classification for REA, keyed by
    /// `(server_name, tool_name)` from the MCP handshake.
    ///
    /// Delegates to the generated 6.5.0 inventory in [`crate::rea_audit`],
    /// which embeds the full REA tool effect audit. Static analysis tools map to
    /// [`Capability::FilesystemRead`] only when they neither launch a process nor
    /// access the network; those external effects, mutations, and execution map to
    /// [`Capability::BinaryTrace`] (parks for approval). Tools absent from the map
    /// fall back to [`rea_server_capability`].
    pub fn rea_tool_capabilities() -> HashMap<(String, String), Capability> {
        crate::rea_audit::rea_tool_capabilities()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn static_tools_are_filesystem_read() {
            let m = rea_tool_capabilities();
            for tool in ["goto_address", "list_unknowns"] {
                assert_eq!(
                    m.get(&(REA_SERVER_NAME.to_string(), tool.to_string())),
                    Some(&Capability::FilesystemRead),
                    "tool {tool}"
                );
            }
        }

        #[test]
        fn tracing_tools_are_binary_trace() {
            let m = rea_tool_capabilities();
            for tool in [
                "capture_process_scenario",
                "gdb_console",
                "instrument_with_frida",
            ] {
                assert_eq!(
                    m.get(&(REA_SERVER_NAME.to_string(), tool.to_string())),
                    Some(&Capability::BinaryTrace),
                    "tool {tool}"
                );
            }
        }

        #[test]
        fn server_fallback_is_restrictive() {
            // Unknown future REA tools must park, not inherit permissiveness.
            assert_eq!(rea_server_capability(), Capability::BinaryTrace);
        }

        #[test]
        fn unknown_tool_has_no_per_tool_entry() {
            let m = rea_tool_capabilities();
            assert!(!m.contains_key(&(REA_SERVER_NAME.to_string(), "some_future_tool".to_string())));
        }

        #[test]
        fn tuple_keys_cannot_cross_match() {
            // (server, tool) tuples never collide across servers: a tool named
            // "rea" on another server, or a server named "rea::x", cannot hit
            // REA's entries because no string splitting is involved.
            let m = rea_tool_capabilities();
            assert!(!m.contains_key(&("other".to_string(), "rea".to_string())));
            assert!(!m.contains_key(&("rea::x".to_string(), "goto_address".to_string())));
        }
    }
}

pub mod rea_audit {
    //! REA 6.5.0 tool capability map, generated from the vendor effect audit.
    //!
    //! Every row stores the full ten-flag effect vector as a bitmask plus the
    //! tier derived from it. [`reaudit`] recomputes the tier from the flags and
    //! fails on any disagreement. That is the drift check the live tools/list
    //! gate calls before trusting the map. Effect bit order:
    //!
    //! | bit | flag               |
    //! |---|--------------------|
    //! | 0   | mutatesTarget      |
    //! | 1   | writesFilesystem   |
    //! | 2   | mutatesDevice      |
    //! | 3   | mayDiscardData     |
    //! | 4   | executesForeignCode|
    //! | 5   | launchesProcess    |
    //! | 6   | accessesNetwork    |
    //! | 7   | mutatesSession     |
    //! | 8   | changesUiState     |
    //! | 9   | idempotent         |
    //!
    //! Source: assets/rea_audit_split.json, extracted from REA's own
    //! toolEffects.ts (the contract that derives readOnlyHint/destructiveHint).
    //! The buckets reconcile exactly: 43 Read, 42 Write, 25 Exec, 110 tools.

    use crate::rea::REA_SERVER_NAME;
    use pantheon_api::capability::Capability;
    use std::collections::HashMap;

    /// REA release the embedded inventory was audited against.
    pub const REA_PINNED_VERSION: &str = "6.5.0";

    /// SHA-256 of the exact rea-agents-6.5.0.tgz this inventory came from.
    pub const REA_TARBALL_SHA256: &str =
        "531717a06e92ca651aab52691190f916fac044272c01f875e22c5e293218e699";

    /// SRI value (base64 SHA-512) of the same tarball, kept for reference.
    pub const REA_TARBALL_SRI: &str = "sha512-C6BG7LyrAdR74mSjertM6PrxJkcNZ2bfQNOJornPo9erDEp2tyPRVYPnV5Yodr+1RqewCQla68WulXlVnveD+A==";

    /// The three risk tiers.
    ///
    /// Read: static analysis. The tool reads the target and returns analysis;
    /// no new privilege beyond what the agent already has.
    /// Write: mutates the target file, the REA session, or the filesystem.
    /// Exec: runs the target or executes foreign code in it.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum ReaTier {
        Read,
        Write,
        Exec,
    }

    /// One audited REA tool: name, effect bitmask, and derived tier.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct ReaTool {
        pub name: &'static str,
        pub mask: u32,
        pub tier: ReaTier,
    }

    const FLAG_MUTATES_TARGET: u32 = 0x1;
    const FLAG_WRITES_FILESYSTEM: u32 = 0x2;
    const FLAG_MUTATES_DEVICE: u32 = 0x4;
    const FLAG_MAY_DISCARD_DATA: u32 = 0x8;
    const FLAG_EXECUTES_FOREIGN_CODE: u32 = 0x10;
    const FLAG_LAUNCHES_PROCESS: u32 = 0x20;
    const FLAG_ACCESSES_NETWORK: u32 = 0x40;
    const FLAG_MUTATES_SESSION: u32 = 0x80;
    const FLAG_CHANGES_UI_STATE: u32 = 0x100;
    const FLAG_IDEMPOTENT: u32 = 0x200;

    /// Decode an effect bitmask back into the ten individual flags, in the
    /// order listed at the top of this file. Exposed so a future audit can
    /// re-derive the tier from the flags without re-fetching the REA source.
    pub fn mask_flags(mask: u32) -> [bool; 10] {
        [
            mask & FLAG_MUTATES_TARGET != 0,
            mask & FLAG_WRITES_FILESYSTEM != 0,
            mask & FLAG_MUTATES_DEVICE != 0,
            mask & FLAG_MAY_DISCARD_DATA != 0,
            mask & FLAG_EXECUTES_FOREIGN_CODE != 0,
            mask & FLAG_LAUNCHES_PROCESS != 0,
            mask & FLAG_ACCESSES_NETWORK != 0,
            mask & FLAG_MUTATES_SESSION != 0,
            mask & FLAG_CHANGES_UI_STATE != 0,
            mask & FLAG_IDEMPOTENT != 0,
        ]
    }

    /// Derive the tier from an effect bitmask.
    ///
    /// Exec if the tool runs foreign code, or launches a process into a target
    /// it mutates. Write if it mutates the target, filesystem, or device, or
    /// can discard data. Read otherwise.
    pub fn tier_from_mask(mask: u32) -> ReaTier {
        let exec = mask & FLAG_EXECUTES_FOREIGN_CODE != 0
            || (mask & FLAG_LAUNCHES_PROCESS != 0) && (mask & FLAG_MUTATES_TARGET != 0);
        let write = mask & FLAG_MUTATES_TARGET != 0
            || mask & FLAG_WRITES_FILESYSTEM != 0
            || mask & FLAG_MUTATES_DEVICE != 0
            || mask & FLAG_MAY_DISCARD_DATA != 0;
        if exec {
            ReaTier::Exec
        } else if write {
            ReaTier::Write
        } else {
            ReaTier::Read
        }
    }

    /// The full REA 6.5.0 inventory: 110 tools with effect vectors and tiers.
    const REA_TOOLS: &[ReaTool] = &[
        ReaTool {
            name: "annotate_native_function",
            mask: 0x2ab,
            tier: ReaTier::Exec,
        },
        ReaTool {
            name: "binary_session",
            mask: 0x220,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "capture_adb_screen",
            mask: 0xe2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "capture_browser_scenario",
            mask: 0x1fb,
            tier: ReaTier::Exec,
        },
        ReaTool {
            name: "capture_electron_scenario",
            mask: 0x1fb,
            tier: ReaTier::Exec,
        },
        ReaTool {
            name: "capture_native_ui_scenario",
            mask: 0x1fb,
            tier: ReaTier::Exec,
        },
        ReaTool {
            name: "capture_process_scenario",
            mask: 0xf2,
            tier: ReaTier::Exec,
        },
        ReaTool {
            name: "close_binary",
            mask: 0x8a,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "close_frida_session",
            mask: 0xe1,
            tier: ReaTier::Exec,
        },
        ReaTool {
            name: "close_gdb_session",
            mask: 0xb1,
            tier: ReaTier::Exec,
        },
        ReaTool {
            name: "close_rizin_debug_session",
            mask: 0x2b1,
            tier: ReaTier::Exec,
        },
        ReaTool {
            name: "collect_adb_bugreport",
            mask: 0xe2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "compare_bundles",
            mask: 0x280,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "cutter_command",
            mask: 0x1f3,
            tier: ReaTier::Exec,
        },
        ReaTool {
            name: "decode_android_resources",
            mask: 0x2a2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "decode_signal_recording",
            mask: 0x2a2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "decompile_jeb_item",
            mask: 0x2c0,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "evaluate_reconstruction_coverage",
            mask: 0x200,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "execute_rizin_command",
            mask: 0xfb,
            tier: ReaTier::Exec,
        },
        ReaTool {
            name: "export_evidence_bundle",
            mask: 0x20a,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "export_web_scripts",
            mask: 0x82,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "extract_artifact",
            mask: 0xa2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "extract_firmware",
            mask: 0xa2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "frida_session_status",
            mask: 0x200,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "gdb_console",
            mask: 0xf3,
            tier: ReaTier::Exec,
        },
        ReaTool {
            name: "gdb_session_status",
            mask: 0x200,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "get_evidence_bundle",
            mask: 0x200,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "get_navigation_context",
            mask: 0x280,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "goto_address",
            mask: 0x380,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "identify_flutter_build",
            mask: 0x282,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "import_evidence_bundle",
            mask: 0x280,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "inspect_adb_client",
            mask: 0x2a0,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "inspect_adb_device",
            mask: 0x2e0,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "inspect_adb_display",
            mask: 0x2e0,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "inspect_adb_package",
            mask: 0x2e0,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "inspect_adb_window",
            mask: 0x2e0,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "inspect_address_context",
            mask: 0x280,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "inspect_android_class",
            mask: 0x2a2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "inspect_android_method",
            mask: 0x2a2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "inspect_android_package",
            mask: 0x2a2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "inspect_apktool_client",
            mask: 0x2a0,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "inspect_artifact",
            mask: 0x2a2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "inspect_binary_layout",
            mask: 0x2a2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "inspect_dart_aot",
            mask: 0x282,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "inspect_evm_interface",
            mask: 0x2a2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "inspect_firmware_regions",
            mask: 0x2a2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "inspect_jeb_client",
            mask: 0x2c0,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "inspect_native_api",
            mask: 0x80,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "inspect_packet_capture",
            mask: 0x2a2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "inspect_packet_fields",
            mask: 0x2a2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "inspect_pcap_capture",
            mask: 0x2a2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "inspect_pcap_client",
            mask: 0x2a2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "inspect_recorded_crash",
            mask: 0x2a2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "inspect_signal_decoders",
            mask: 0x2a2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "inspect_sqlite_database",
            mask: 0x2a2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "inspect_wasm_artifact",
            mask: 0x2a2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "inspect_web_network_capture",
            mask: 0x2a2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "inspect_with_objdump",
            mask: 0x2a0,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "install_adb_package",
            mask: 0xe5,
            tier: ReaTier::Exec,
        },
        ReaTool {
            name: "instrument_with_frida",
            mask: 0xf1,
            tier: ReaTier::Exec,
        },
        ReaTool {
            name: "list_adb_devices",
            mask: 0x2e0,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "list_adb_directory",
            mask: 0x2e0,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "list_adb_features",
            mask: 0x2e0,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "list_adb_packages",
            mask: 0x2e0,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "list_adb_processes",
            mask: 0x2e0,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "list_adb_services",
            mask: 0x2e0,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "list_cutter_sessions",
            mask: 0x240,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "list_frida_devices",
            mask: 0x260,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "list_frida_processes",
            mask: 0x260,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "list_jeb_units",
            mask: 0x2c0,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "list_unknowns",
            mask: 0x200,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "load_frida_script",
            mask: 0xf1,
            tier: ReaTier::Exec,
        },
        ReaTool {
            name: "observe_native_calls",
            mask: 0x1f2,
            tier: ReaTier::Exec,
        },
        ReaTool {
            name: "observe_native_ui",
            mask: 0x2a2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "observe_web_execution",
            mask: 0xc9,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "open_binary",
            mask: 0x2a0,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "open_jeb_project",
            mask: 0xc0,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "pull_adb_file",
            mask: 0xe2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "pull_adb_package",
            mask: 0xe2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "push_adb_file",
            mask: 0xe7,
            tier: ReaTier::Exec,
        },
        ReaTool {
            name: "read_adb_logcat",
            mask: 0x2e0,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "read_adb_setting",
            mask: 0x2e0,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "record_unknown",
            mask: 0x80,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "recover_javascript_sources",
            mask: 0xa2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "resolve_adb_packages",
            mask: 0x2e0,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "resume_frida_session",
            mask: 0xf1,
            tier: ReaTier::Exec,
        },
        ReaTool {
            name: "rizin_debug_command",
            mask: 0x1fb,
            tier: ReaTier::Exec,
        },
        ReaTool {
            name: "rizin_debug_session_status",
            mask: 0x280,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "search_android_classes",
            mask: 0x2a2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "set_address_name",
            mask: 0x289,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "set_addresses_names",
            mask: 0x289,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "set_bookmark",
            mask: 0x289,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "set_comment",
            mask: 0x289,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "set_inline_comment",
            mask: 0x289,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "start_adb_activity",
            mask: 0xe5,
            tier: ReaTier::Exec,
        },
        ReaTool {
            name: "start_adb_app",
            mask: 0xe5,
            tier: ReaTier::Exec,
        },
        ReaTool {
            name: "start_frida_session",
            mask: 0xe1,
            tier: ReaTier::Exec,
        },
        ReaTool {
            name: "start_gdb_session",
            mask: 0xa0,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "start_rizin_debug_session",
            mask: 0xb1,
            tier: ReaTier::Exec,
        },
        ReaTool {
            name: "stop_adb_app",
            mask: 0xe5,
            tier: ReaTier::Exec,
        },
        ReaTool {
            name: "trace_android_references",
            mask: 0x2a2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "trace_call_path",
            mask: 0x80,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "trace_feature",
            mask: 0x80,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "trace_web_module_imports",
            mask: 0x2e2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "trace_web_source_location",
            mask: 0x2a2,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "uninstall_adb_package",
            mask: 0xe5,
            tier: ReaTier::Exec,
        },
        ReaTool {
            name: "unload_frida_script",
            mask: 0xe1,
            tier: ReaTier::Exec,
        },
        ReaTool {
            name: "unset_bookmark",
            mask: 0x289,
            tier: ReaTier::Write,
        },
        ReaTool {
            name: "update_unknown",
            mask: 0x80,
            tier: ReaTier::Read,
        },
        ReaTool {
            name: "verify_unknown_resolution",
            mask: 0x200,
            tier: ReaTier::Read,
        },
    ];

    /// Per-tool capability map, keyed by `(server_name, tool_name)` like the
    /// old hand-maintained allowlist in [`crate::rea`]. Read-tier tools are
    /// [`Capability::FilesystemRead`] ONLY when they neither launch a process
    /// nor access the network (the rule [`crate::rea`] documents): 26
    /// Read-tier tools in this inventory launch processes and/or touch the
    /// network (e.g. `start_gdb_session`, `inspect_adb_device`,
    /// `list_cutter_sessions`), and those map to [`Capability::BinaryTrace`]
    /// so default policies park them for approval instead of letting a
    /// process-launching "read" tool ride the permissive FilesystemRead
    /// capability. Write and Exec-tier tools are [`Capability::BinaryTrace`]
    /// as well. Tools absent from this map fall back to
    /// [`crate::rea::rea_server_capability`].
    pub fn rea_tool_capabilities() -> HashMap<(String, String), Capability> {
        REA_TOOLS
            .iter()
            .map(|t| {
                // The audit tier alone does not decide the capability:
                // a Read-tier tool that spawns a process or touches the
                // network is an external effect, not a static read.
                let external =
                    t.mask & FLAG_LAUNCHES_PROCESS != 0 || t.mask & FLAG_ACCESSES_NETWORK != 0;
                let cap = match t.tier {
                    ReaTier::Read if !external => Capability::FilesystemRead,
                    _ => Capability::BinaryTrace,
                };
                ((REA_SERVER_NAME.to_string(), t.name.to_string()), cap)
            })
            .collect()
    }

    /// Drift check: recompute every tool's tier from its stored effect flags
    /// and fail if the stored tier disagrees. The live tools/list gate calls
    /// this at handshake; if REA renames a tool, the unknown tool surfaces on
    /// its own, and this invariant keeps the embedded map self-consistent.
    pub fn reaudit() -> Result<(), String> {
        for tool in REA_TOOLS {
            let derived = tier_from_mask(tool.mask);
            if derived != tool.tier {
                return Err(format!(
                    "tool {}: stored tier {:?} disagrees with derived tier {:?} (mask {:#06x})",
                    tool.name, tool.tier, derived, tool.mask
                ));
            }
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn map_has_exactly_110_entries() {
            assert_eq!(REA_TOOLS.len(), 110);
            let m = rea_tool_capabilities();
            assert_eq!(m.len(), 110);
        }

        #[test]
        fn tiers_are_derivable_from_flags() {
            assert_eq!(tier_from_mask(FLAG_EXECUTES_FOREIGN_CODE), ReaTier::Exec);
            assert_eq!(
                tier_from_mask(FLAG_LAUNCHES_PROCESS | FLAG_MUTATES_TARGET),
                ReaTier::Exec
            );
            assert_eq!(tier_from_mask(FLAG_LAUNCHES_PROCESS), ReaTier::Read);
            assert_eq!(tier_from_mask(FLAG_WRITES_FILESYSTEM), ReaTier::Write);
            assert_eq!(
                tier_from_mask(FLAG_MUTATES_TARGET | FLAG_MUTATES_SESSION),
                ReaTier::Write
            );
            assert_eq!(
                tier_from_mask(FLAG_ACCESSES_NETWORK | FLAG_MUTATES_SESSION),
                ReaTier::Read
            );
            assert_eq!(tier_from_mask(0), ReaTier::Read);
        }

        #[test]
        fn read_tools_are_filesystem_read() {
            // Only non-external Read tools are FilesystemRead: a Read
            // tool that launches a process or touches the network parks
            // as BinaryTrace (26 tools in this inventory).
            let m = rea_tool_capabilities();
            let mut external_read = 0;
            for t in REA_TOOLS {
                if t.tier == ReaTier::Read {
                    let key = (REA_SERVER_NAME.to_string(), t.name.to_string());
                    let external =
                        t.mask & FLAG_LAUNCHES_PROCESS != 0 || t.mask & FLAG_ACCESSES_NETWORK != 0;
                    if external {
                        external_read += 1;
                    }
                    assert_eq!(
                        m.get(&key),
                        Some(if external {
                            &Capability::BinaryTrace
                        } else {
                            &Capability::FilesystemRead
                        }),
                        "tool {} {:#06x}",
                        t.name,
                        t.mask
                    );
                }
            }
            assert_eq!(external_read, 26);
            for tool in [
                "start_gdb_session",
                "inspect_adb_device",
                "list_cutter_sessions",
            ] {
                let key = (REA_SERVER_NAME.to_string(), tool.to_string());
                assert_eq!(m.get(&key), Some(&Capability::BinaryTrace), "tool {tool}");
            }
            for tool in ["goto_address", "list_unknowns", "trace_call_path"] {
                let key = (REA_SERVER_NAME.to_string(), tool.to_string());
                assert_eq!(
                    m.get(&key),
                    Some(&Capability::FilesystemRead),
                    "tool {tool}"
                );
            }
        }

        #[test]
        fn write_and_exec_tools_are_binary_trace() {
            let m = rea_tool_capabilities();
            for t in REA_TOOLS {
                if t.tier != ReaTier::Read {
                    let key = (REA_SERVER_NAME.to_string(), t.name.to_string());
                    assert_eq!(
                        m.get(&key),
                        Some(&Capability::BinaryTrace),
                        "tool {} {:#06x}",
                        t.name,
                        t.mask
                    );
                }
            }
        }

        #[test]
        fn known_exec_tools_are_present() {
            let m = rea_tool_capabilities();
            for tool in ["gdb_console", "instrument_with_frida", "cutter_command"] {
                let key = (REA_SERVER_NAME.to_string(), tool.to_string());
                assert_eq!(m.get(&key), Some(&Capability::BinaryTrace), "tool {tool}");
            }
        }

        #[test]
        fn mask_flags_round_trips() {
            for t in REA_TOOLS {
                let flags = mask_flags(t.mask);
                let rebuilt: u32 = flags
                    .iter()
                    .enumerate()
                    .fold(0, |acc, (i, b)| acc | (*b as u32) << i);
                assert_eq!(rebuilt, t.mask, "tool {} {:#06x}", t.name, t.mask);
            }
        }

        #[test]
        fn reaudit_passes_on_embedded_data() {
            assert!(reaudit().is_ok());
        }
    }
}
