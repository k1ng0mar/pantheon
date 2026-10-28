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
/// classification > built-in server fallback > [`Capability::NetworkOutbound`].
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
/// then built-in server fallback (REA's restrictive fallback), then the
/// [`Capability::NetworkOutbound`] default for all other servers.
///
/// Fail-closed properties:
/// * Unknown tools (no per-tool entry anywhere) fall to the server
///   fallback or the default — never to a *more* permissive capability
///   than the server's own classification.
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
    // 4. Default for all other servers (backwards compatible).
    Capability::NetworkOutbound
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
                        Err(e) => return Err(McpError::Io(format!("hash {}: {e}", p.display()))),
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
                            return Err(McpError::Io(format!(
                                "hash {}: {e}",
                                p.display()
                            )))
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
    pub fn oauth_status(&self, name: &str) -> Result<(bool, Vec<String>, Option<u64>), McpError> {
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
            // Post-handshake gate: the exact identity must be approved,
            // so a version bump lapses consent even with the same hash.
            // Only the canonical bundled recipe skips this: there is no
            // consent record to lapse, and catalog updates are pinned by
            // recipe. A name-colliding spec is a custom server and must
            // be approved.
            let (version, server_impl_version, ok) = {
                let st = inner.states.get(&name).expect("state");
                let c = st.conn.as_ref().expect("connected");
                let server_impl_version = c.server_version().map(str::to_string);
                let v = server_impl_version
                    .clone()
                    .unwrap_or_else(|| c.negotiated_version().to_string());
                let ok = is_canonical_bundled(&st.spec) || store.is_approved(&name, &v, &hash);
                (v, server_impl_version, ok)
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
                // The projected capability is `NetworkOutbound` by default
                // (deliberate: an MCP tool is third-party code whose
                // effects Pantheon's taxonomy cannot see inside of. The
                // trust decision is the explicit per-server approval
                // (name + version + content hash); `NetworkOutbound` is
                // the honest "talks to the outside world" mapping, which
                // the default policies allow for approved servers. A
                // `Other("mcp...")` token would deny every MCP tool under
                // `Policy::coder()`, silently.) A caller may override the
                // capability per server or per tool (see
                // [`McpCapabilityOverrides`]): the CUA driver projects as
                // `ComputerUse`, the inward desktop-control capability,
                // which parks for human approval by default. Built-in
                // classifications (e.g. REA's static-vs-tracing split)
                // apply underneath caller overrides.
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
    fn default_is_network_outbound_for_unknown_servers() {
        // Backwards compatibility: servers with no overrides keep the
        // long-standing default.
        assert_eq!(
            resolve_tool_capability(&empty(), "github", "get_issue"),
            Capability::NetworkOutbound
        );
        assert_eq!(
            resolve_tool_capability(&empty(), "anything", "whatever"),
            Capability::NetworkOutbound
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
        // Other servers unaffected.
        assert_eq!(
            resolve_tool_capability(&o, "github", "get_issue"),
            Capability::NetworkOutbound
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
            Capability::NetworkOutbound
        );
        assert_eq!(
            resolve_tool_capability(&empty(), "myrea", "trace_call_path"),
            Capability::NetworkOutbound
        );
        // A server literally named "rea::x" is a different server.
        assert_eq!(
            resolve_tool_capability(&empty(), "rea::x", "goto_address"),
            Capability::NetworkOutbound
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
            Capability::NetworkOutbound
        );
    }
}
