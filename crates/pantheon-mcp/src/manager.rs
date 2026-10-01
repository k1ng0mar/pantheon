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
//! connected until the operator has approved its identity — name,
//! self-reported version, and content hash — through the unified
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
//! Bundled catalog servers are first-party, not third-party — but only
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
//! is held — one in-flight call per manager, which matches the agent
//! loop's sequential tool execution.

use crate::{
    bundled::{find_bundled, is_bundled},
    http::{HttpMcpClient, HttpTransport},
    McpClient, McpConn, McpError, McpServerConfig, McpToolDef,
};
use pantheon_api::approval::{self, ApprovalRecord, ApprovalStore};
use pantheon_api::capability::{Capability, Policy};
use pantheon_api::config::McpServerEntry;
use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::message::ToolSchema;
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
/// `Debug` prints env variable *names* only — values (which may be
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
/// exactly (transport, command, args, env, url) — i.e. it is what
/// [`BundledMcpServer`](crate::bundled::BundledMcpServer)::to_config_entry
/// materializes via the single canonical writer. `enabled` and the
/// timeout are operator-controlled and excluded from the comparison;
/// `pinned_version` is informational and not part of the parsed entry.
///
/// A name-colliding spec with any difference is a custom server: it
/// goes through full approval like any other third-party server, and
/// refusal messages flag the collision.
fn is_canonical_bundled(spec: &McpServerSpec) -> bool {
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

/// Sanitize one name segment for the `mcp_<server>_<tool>` namespace:
/// lowercase alphanumeric runs joined by single underscores; anything
/// else collapses. Never empty.
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

/// Backoff after `failures` consecutive failures: 2s, 4s, 8s … capped
/// at 5 minutes.
fn backoff_delay(failures: u32) -> Duration {
    // 1s, 2s, 4s, 8s, ... capped at 300s. saturating_pow never panics
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
/// resolves to a file, so `python3 server.py` binds `server.py` too) —
/// except when the command is a launcher shim (`npx`, `uvx`, `docker`,
/// ...), where the shim binary says nothing about the server code that
/// actually runs. For shims the hash covers the command plus the full
/// argument list instead: the package spec (`name@pin`) and flags are
/// what determine the code that runs, so a pin bump or an arg change
/// lapses the approval. For remote transports the hash is the endpoint
/// URL. Names only, never values, in errors.
///
/// Honest limit: for launcher shims this binds the *requested* package
/// spec, not the bytes the registry served — registry-fetched code can
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
                bytes.extend_from_slice(b"mcp-launcher-shim\0");
                bytes.extend_from_slice(cmd.as_bytes());
                for a in &spec.args {
                    bytes.push(0);
                    bytes.extend_from_slice(a.as_bytes());
                }
                return Ok(approval::bytes_hash(&bytes));
            }
            match resolve_command(cmd) {
                Some(p) => match std::fs::read(&p) {
                    Ok(b) => bytes.extend_from_slice(&b),
                    Err(e) => return Err(McpError::Io(format!("hash {}: {e}", p.display()))),
                },
                None => {
                    bytes.extend_from_slice(cmd.as_bytes());
                    bytes.push(0);
                }
            }
            // Bind script files passed as arguments: the interpreter
            // binary alone would not lapse approval when the script
            // changes.
            for a in &spec.args {
                let p = Path::new(a);
                if p.is_file() {
                    if let Ok(b) = std::fs::read(p) {
                        bytes.push(0);
                        bytes.extend_from_slice(&b);
                    }
                }
            }
            Ok(approval::bytes_hash(&bytes))
        }
        McpTransport::Sse | McpTransport::Http => {
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
    /// first — this only persists the record, like the plugin flow.
    pub fn approve_server(&self, name: &str) -> Result<ApprovalRecord, McpError> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| McpError::Io("mcp manager lock poisoned".to_string()))?;
        let resolver = inner.env_resolver.clone();
        let policy = inner.policy.clone();
        let scope = inner.scope_dir();
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
        // Fingerprint: connect, then bind name + version + hash.
        let conn = Self::connect_new(&st.spec, &resolver, &policy)?;
        let version = conn
            .server_version()
            .map(str::to_string)
            .unwrap_or_else(|| conn.negotiated_version().to_string());
        let hash = st.hash.clone().unwrap_or_default();
        let rec = ApprovalRecord {
            plugin: name.to_string(),
            version,
            dir_hash: hash,
            approved_at_ms: now_ms(),
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
        self.register_names(reg, &names, None)
    }

    /// Project one server's tools into the registry, with the same gates
    /// as [`McpManager::register_tools`] but restricted to `server`. Used
    /// for servers gated by their own toggle rather than the Plugins
    /// group — today, the CUA driver behind the ComputerUse group.
    ///
    /// `capability` overrides the projected tools' capability (default
    /// [`Capability::NetworkOutbound`]). The CUA driver passes
    /// [`Capability::ComputerUse`]: desktop control is an inward
    /// capability and must park for human approval under the default
    /// policy, not ride the outward network treatment.
    pub fn register_server_tools(
        self: &Arc<Self>,
        reg: &mut ToolRegistry,
        server: &str,
        capability: Option<Capability>,
    ) -> McpReport {
        let mut caps = HashMap::new();
        if let Some(cap) = capability {
            caps.insert(server.to_string(), cap);
        }
        self.register_names(reg, &[server.to_string()], Some(&caps))
    }

    fn register_names(
        self: &Arc<Self>,
        reg: &mut ToolRegistry,
        names: &[String],
        caps: Option<&HashMap<String, Capability>>,
    ) -> McpReport {
        let mut report = McpReport::default();
        let mut used: HashSet<String> = HashSet::new();
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
            // Post-handshake gate: the exact identity must be approved,
            // so a version bump lapses consent even with the same hash.
            // Only the canonical bundled recipe skips this: there is no
            // consent record to lapse, and catalog updates are pinned by
            // recipe. A name-colliding spec is a custom server and must
            // be approved.
            let (version, ok) = {
                let st = inner.states.get(&name).expect("state");
                let c = st.conn.as_ref().expect("connected");
                let v = c
                    .server_version()
                    .map(str::to_string)
                    .unwrap_or_else(|| c.negotiated_version().to_string());
                let ok = is_canonical_bundled(&st.spec) || store.is_approved(&name, &v, &hash);
                (v, ok)
            };
            if !ok {
                let st = inner.states.get_mut(&name).expect("state");
                if let Some(mut c) = st.conn.take() {
                    c.shutdown();
                }
                st.status = ServerStatus::Unapproved;
                st.last_error = Some(format!(
                    "approval lapsed: version {version:?} is not approved for this content hash"
                ));
                report.pending_approval.push(name);
                continue;
            }
            let tools = {
                let st = inner.states.get_mut(&name).expect("state");
                let res = st.conn.as_mut().expect("connected").list_tools();
                match res {
                    Ok(t) => t,
                    Err(e) => {
                        Self::note_failure(st, e.to_string());
                        let msg = st.last_error.clone().unwrap_or_default();
                        report.skipped.push((name, msg));
                        continue;
                    }
                }
            };
            {
                let st = inner.states.get_mut(&name).expect("state");
                st.tools = tools.clone();
                st.status = ServerStatus::Ready;
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
                // The projected capability is `NetworkOutbound` by default
                // (deliberate: an MCP tool is third-party code whose
                // effects Pantheon's taxonomy cannot see inside of. The
                // trust decision is the explicit per-server approval
                // (name + version + content hash); `NetworkOutbound` is
                // the honest "talks to the outside world" mapping, which
                // the default policies allow for approved servers. A
                // `Other("mcp…")` token would deny every MCP tool under
                // `Policy::coder()`, silently.) A caller may override the
                // capability for a server it knows: the CUA driver
                // projects as `ComputerUse`, the inward desktop-control
                // capability, which parks for human approval by default.
                let cap = caps
                    .and_then(|m| m.get(&name).cloned())
                    .unwrap_or(Capability::NetworkOutbound);
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

    /// Call one server tool by (server, raw tool name). Reconnects with
    /// backoff when the transport died; on a transport failure mid-call
    /// it reconnects once and retries the call once.
    pub fn call_tool(&self, server: &str, tool: &str, args: &Value) -> Result<Value, McpError> {
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
            st.conn.as_mut().expect("connected").call_tool(tool, args)
        };
        match res {
            Ok(v) => {
                inner.states.get_mut(server).expect("state").last_error = None;
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
                // #5: the mid-call reconnect is a (re)connect — gate it
                // like every other connect path, so a lapsed or revoked
                // approval blocks the retry instead of silently
                // re-executing.
                Self::gate_approval(&mut inner.states, &scope, server, true)?;
                match Self::connect_new(&spec, &resolver, &policy) {
                    Ok(conn) => {
                        let st = inner.states.get_mut(server).expect("state");
                        st.conn = Some(conn);
                        st.connects += 1;
                        st.status = ServerStatus::Ready;
                        st.not_before = None;
                        st.conn.as_mut().expect("connected").call_tool(tool, args)
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
                st.last_error = Some("connection lost".to_string());
            }
            out.push(health_of(st));
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
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
    /// (#5) — a missing, lapsed, or revoked approval refuses the attempt
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
        match Self::connect_new(&spec, &resolver, &policy) {
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
        }
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
    /// Consults the approval store — the same store and record shape
    /// `register_names` uses at registration time, no parallel store —
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
    /// configure-time hash is reused (cheap) — revocation is still caught
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
        let st = inner.states.get_mut(name).expect("checked above");
        st.status = ServerStatus::Connecting;
        match Self::connect_new(&st.spec, &resolver, &policy) {
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
    /// transport. Env *values* never appear in errors — only names.
    fn connect_new(
        spec: &McpServerSpec,
        resolver: &Option<EnvResolver>,
        policy: &Option<Policy>,
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
                Ok(Box::new(McpClient::connect(&cfg, policy.as_ref())?))
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
                Ok(Box::new(HttpMcpClient::connect(
                    &spec.name,
                    t,
                    &url,
                    spec.timeout,
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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// Small deterministic invariants only: naming, sanitizing, backoff,
// spec equality. Anything with a subprocess, socket, or clock lives in
// `pantheon-eval` (`eval/tests/mcp_manager.rs`).
