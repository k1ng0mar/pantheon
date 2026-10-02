//! MCP server management: the dashboard half of the Tools & MCPs view.
//!
//! ## One enablement state
//!
//! The config file is the source of truth. `[mcp.servers.<name>]` with
//! `enabled` (default false) is read and written identically by the
//! config file, the dashboard (`GET /api/mcp/servers`,
//! `POST /api/mcp/servers/:name/{enable,disable,toggle}`), the mobile
//! app (which talks to the same dashboard endpoints), and the agent's
//! `enable_mcp` tool. Migration declaration files
//! (`<data_dir>/mcp/*.json`) are legacy: they are still *read* (a name
//! the config section does not define keeps its declaration), but every
//! write - add, enable, disable, toggle, delete - lands in `config.toml`.
//! A config entry shadows a declaration of the same name everywhere.
//!
//! ## Which gate wins: `enabled` vs approval
//!
//! Two different gates, two different questions:
//!
//! - `enabled` (this file's flag) is the **operator's switch**: is this
//!   server supposed to run? Fresh installs enable zero servers; every
//!   enable is explicit.
//! - The approval store (`<data_dir>/mcp/.approvals.json`,
//!   `pantheon_api::approval`) is **consent to run third-party code**:
//!   it binds the server's name to the content hash of its binary (or
//!   URL). An unapproved custom server never connects no matter what the
//!   flag says.
//!
//! A server launches iff it is enabled AND (bundled OR approved).
//! Bundled catalog servers are first-party: they skip the consent store,
//! so for them the `enabled` flag is the only gate. Disabling flips the
//! switch only - a recorded approval persists, so re-enabling the same
//! binary/URL resumes without re-consent; if the binary or URL changes,
//! the approval lapses and consent is asked again.
//!
//! ## Custom servers: validated, never handshaked here
//!
//! `POST /api/mcp/servers` validates the *shape* without a live
//! handshake: a stdio command must resolve on PATH (or be an existing
//! path), an sse/http URL must parse (http/https scheme, non-empty
//! host). No process is spawned, no TCP connection is opened - that is
//! `pantheon doctor`'s job, and the `test` probe's. New servers are
//! added disabled; consent (for custom servers) and enabling are
//! separate explicit acts.
//!
//! This surface never launches a server. A full handshake needs an
//! explicit `pantheon mcp approve <name>` on the operator's machine, or
//! the `approve` endpoint below (which records consent against a
//! locally-computed content hash, still without connecting).
//!
//! ## Catalog source
//!
//! Bundled-catalog types come from the canonical
//! `pantheon_api::mcp_catalog` - consumed directly, never redefined here.
//! (`pantheon_api::mcp_catalog` remains only for `pantheon-tools`' agent
//! `enable_mcp` tool, which cannot depend on `pantheon-mcp` without a
//! dependency cycle.)

use crate::{bad_json, body_json, err_json, json_ok, App};
use pantheon_api::approval::{self, ApprovalRecord, ApprovalStore};
use pantheon_api::mcp_catalog;
use pantheon_gateway::http::{Request, Response};

/// Bundled-catalog membership: the canonical catalog in
/// `pantheon_api::mcp_catalog` is the only list of first-party servers.
/// Bundled servers are first-party - the `enabled` flag is their only
/// gate - so this exemption mirrors the launcher's manager and the
/// dashboard's "enabled AND (bundled OR approved)" model.
fn is_bundled(name: &str) -> bool {
    mcp_catalog::find(name).is_some()
}
use pantheon_migration::{read_mcp_declarations, server_readiness, McpServer};
use std::collections::HashMap;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn mcp_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("mcp")
}

fn config_path(data_dir: &Path) -> PathBuf {
    data_dir.join("config.toml")
}

/// The launcher's live snapshot, keyed by server name. `None` when no
/// session has written one yet.
fn live_map(data_dir: &Path) -> HashMap<String, serde_json::Value> {
    let mut out = HashMap::new();
    let text = std::fs::read_to_string(mcp_dir(data_dir).join("live.json")).unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    if let Some(arr) = v.get("servers").and_then(|s| s.as_array()) {
        for s in arr {
            if let Some(name) = s.get("name").and_then(|n| n.as_str()) {
                out.insert(name.to_string(), s.clone());
            }
        }
    }
    out
}

/// Approved server names (names only - records never leave the vault path).
fn approved_names(data_dir: &Path) -> Vec<String> {
    ApprovalStore::open(&mcp_dir(data_dir)).names()
}

fn safe_name(name: &str) -> bool {
    !(name.trim().is_empty()
        || name.len() > 64
        || name.contains('/')
        || name.contains('\\')
        || name.contains(".."))
}

/// Parse `config.toml` as a TOML document for surgical edits. A missing
/// file is an empty document; a file that exists but does not parse is
/// an error (fail closed - never guess at enablement).
fn read_config_doc(data_dir: &Path) -> Result<toml::Value, String> {
    let path = config_path(data_dir);
    match std::fs::read_to_string(&path) {
        Ok(text) => text
            .parse()
            .map_err(|e| format!("parse {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(toml::Value::Table(toml::map::Map::new()))
        }
        Err(e) => Err(format!("read {}: {e}", path.display())),
    }
}

/// The document is edited, not re-serialized from the struct, so
/// unknown keys survive. Comments and original key order do not: the
/// TOML document model drops comments and sorts keys.
fn write_config_doc(data_dir: &Path, doc: &toml::Value) -> Result<(), String> {
    let path = config_path(data_dir);
    let text = toml::to_string_pretty(doc).map_err(|e| format!("serialize: {e}"))?;
    crate::util::atomic_write(&path, &text)
}

/// The `[mcp.servers.<name>]` table, creating parents as needed.
fn server_table<'a>(
    doc: &'a mut toml::Value,
    name: &str,
) -> Result<&'a mut toml::map::Map<String, toml::Value>, String> {
    // The fixed-prefix walk is the shared `parent_of`; the server name is
    // appended as a literal leaf key because it may itself contain dots,
    // which must not be treated as path separators.
    let (parent, leaf) = crate::util::parent_of(doc, "mcp.servers")?;
    let servers = parent
        .entry(leaf)
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    let servers_table = servers
        .as_table_mut()
        .ok_or_else(|| "config has a non-table [mcp.servers] value".to_string())?;
    let entry = servers_table
        .entry(name.to_string())
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    entry
        .as_table_mut()
        .ok_or_else(|| format!("[mcp.servers.{name}] is not a table"))
}

/// Fill an empty `[mcp.servers.<name>]` table from a shape. Never
/// overwrites keys the operator already set: materialization only fills
/// gaps.
fn materialize_shape(table: &mut toml::map::Map<String, toml::Value>, shape: &ServerShape) {
    if !table.contains_key("transport") {
        table.insert(
            "transport".to_string(),
            toml::Value::String(shape.transport.clone()),
        );
    }
    if let Some(command) = &shape.command {
        if !table.contains_key("command") {
            table.insert("command".to_string(), toml::Value::String(command.clone()));
        }
    }
    if !shape.args.is_empty() && !table.contains_key("args") {
        table.insert(
            "args".to_string(),
            toml::Value::Array(
                shape
                    .args
                    .iter()
                    .map(|a| toml::Value::String(a.clone()))
                    .collect(),
            ),
        );
    }
    if let Some(url) = &shape.url {
        if !table.contains_key("url") {
            table.insert("url".to_string(), toml::Value::String(url.clone()));
        }
    }
    if !shape.requires_env.is_empty() && !table.contains_key("env") {
        let mut env_table = toml::map::Map::new();
        for name in &shape.requires_env {
            env_table.insert(name.clone(), toml::Value::String(format!("env:{name}")));
        }
        table.insert("env".to_string(), toml::Value::Table(env_table));
    }
}

/// The spawn-relevant shape of a server, from any source (catalog,
/// config, declaration).
struct ServerShape {
    transport: String,
    command: Option<String>,
    args: Vec<String>,
    url: Option<String>,
    requires_env: Vec<String>,
}

impl ServerShape {
    fn from_catalog(s: &mcp_catalog::BundledMcpServer) -> Self {
        ServerShape {
            transport: s.transport.clone(),
            command: s.command.clone(),
            args: s.args.clone(),
            url: s.url.clone(),
            // Secret *names* the recipe needs; values are never stored.
            requires_env: s.requires_env.clone(),
        }
    }

    fn from_config(e: &pantheon_api::config::McpServerEntry) -> Self {
        ServerShape {
            transport: e.transport.clone(),
            command: e.command.clone(),
            args: e.args.clone(),
            url: e.url.clone(),
            requires_env: e.env.keys().cloned().collect(),
        }
    }

    fn from_declaration(s: &McpServer) -> Self {
        ServerShape {
            transport: s.transport.clone(),
            command: s.command.clone(),
            args: s.args.clone(),
            url: s.url.clone(),
            requires_env: s.requires_env.clone(),
        }
    }

    /// Shape problems, without any live handshake: stdio needs a
    /// command, remote transports need a URL, and the transport must be
    /// known. This is the add-time validation.
    fn problems(&self, name: &str) -> Vec<String> {
        let mut out = Vec::new();
        match self.transport.as_str() {
            "stdio" => {
                if self
                    .command
                    .as_deref()
                    .map(str::trim)
                    .unwrap_or("")
                    .is_empty()
                {
                    out.push(format!(
                        "mcp.servers.{name}: transport is stdio but no command is set"
                    ));
                } else if resolve_command(self.command.as_deref().unwrap_or("")).is_none() {
                    out.push(format!(
                        "mcp.servers.{name}: command {:?} does not resolve on PATH",
                        self.command.as_deref().unwrap_or("")
                    ));
                }
                if self.url.is_some() {
                    out.push(format!(
                        "mcp.servers.{name}: url is ignored for stdio transport"
                    ));
                }
            }
            "sse" | "http" => {
                match self.url.as_deref() {
                    Some(url) if url_shape_ok(url) => {}
                    _ => out.push(format!(
                        "mcp.servers.{name}: transport is {} but no valid url is set (want http(s)://host/...)",
                        self.transport
                    )),
                }
                if self.command.is_some() {
                    out.push(format!(
                        "mcp.servers.{name}: command is ignored for {} transport",
                        self.transport
                    ));
                }
            }
            other => out.push(format!(
                "mcp.servers.{name}: unknown transport {other:?} (stdio|sse|http)"
            )),
        }
        out
    }
}

/// URL shape check without any network: http/https scheme and a
/// non-empty host. DNS and TCP are `test`'s / `pantheon doctor`'s job.
fn url_shape_ok(url: &str) -> bool {
    let url = url.trim();
    let after = match url.split("://").next() {
        Some("http") | Some("https") => url.split("://").nth(1).unwrap_or(""),
        _ => return false,
    };
    let host = after.split('/').next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or("");
    !host.trim().is_empty() && !host.contains(char::is_whitespace)
}

/// True when `command` is a launcher shim rather than the server
/// itself: the program on the command line fetches (npx/uvx/bunx) or
/// runs (docker/podman) a packaged server instead of being the server
/// code. Mirrors `pantheon_mcp::manager`'s fingerprint - the two must
/// stay in lockstep, because the approve endpoint records the hash and
/// the launcher's approval gate checks it.
fn is_launcher_shim(command: &str) -> bool {
    let base = Path::new(command)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(command);
    matches!(base, "npx" | "bunx" | "pnpx" | "uvx" | "docker" | "podman")
}

/// Content hash the approval binds, mirroring
/// `pantheon_mcp::manager`'s fingerprint: the server binary (plus script
/// args) for stdio - except when the command is a launcher shim
/// (`npx`, `uvx`, `docker`, ...), where the hash covers the command
/// plus the full argument list instead, so a pin bump or an arg change
/// lapses the approval - or the endpoint URL for remote transports.
/// Computed locally - no process spawned, no connection opened.
///
/// Honest limit: for launcher shims this binds the *requested* package
/// spec, not the bytes the registry served - registry-fetched code can
/// change under a pin, so the approval binds intent, not bytes.
fn content_hash(shape: &ServerShape) -> Result<String, String> {
    match shape.transport.as_str() {
        "stdio" => {
            let cmd = shape.command.as_deref().unwrap_or("");
            let mut bytes = Vec::new();
            if is_launcher_shim(cmd) {
                // Bind what determines the code, not the shim binary.
                bytes.extend_from_slice(b"mcp-launcher-shim\0");
                bytes.extend_from_slice(cmd.as_bytes());
                for a in &shape.args {
                    bytes.push(0);
                    bytes.extend_from_slice(a.as_bytes());
                }
                return Ok(approval::bytes_hash(&bytes));
            }
            match resolve_command(cmd) {
                Some(p) => match std::fs::read(&p) {
                    Ok(b) => bytes.extend_from_slice(&b),
                    Err(e) => {
                        return Err(format!("hash {}: {e}", p.display()));
                    }
                },
                None => {
                    bytes.extend_from_slice(cmd.as_bytes());
                    bytes.push(0);
                }
            }
            for a in &shape.args {
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
        "sse" | "http" => {
            let url = shape.url.as_deref().unwrap_or("");
            Ok(approval::bytes_hash(url.as_bytes()))
        }
        other => Err(format!("unknown transport {other:?}")),
    }
}

fn server_json(
    s: &McpServer,
    live: &HashMap<String, serde_json::Value>,
    approved: &[String],
) -> serde_json::Value {
    serde_json::json!({
        "name": s.name,
        "transport": s.transport,
        "command": s.command,
        "args": s.args,
        "url": s.url,
        "requires_env": s.requires_env,
        "needs_credentials": s.needs_credentials,
        "enabled": s.enabled,
        "bundled": false,
        "readiness": server_readiness(s),
        "approved": approved.iter().any(|a| a == &s.name),
        "health": live.get(&s.name).cloned().unwrap_or(serde_json::Value::Null),
    })
}

/// A config-section server rendered in the same shape as a declaration
/// server, so the UI shows one merged view.
fn config_server_json(
    name: &str,
    e: &pantheon_api::config::McpServerEntry,
    live: &HashMap<String, serde_json::Value>,
    approved: &[String],
) -> serde_json::Value {
    let bundled = is_bundled(name);
    serde_json::json!({
        "name": name,
        "transport": e.transport,
        "command": e.command,
        "args": e.args,
        "url": e.url,
        "requires_env": e.env.keys().cloned().collect::<Vec<_>>(),
        "needs_credentials": false,
        "enabled": e.enabled,
        "bundled": bundled,
        "readiness": e.problems(name).into_iter().next(),
        // Bundled servers are first-party: no consent record needed, the
        // enabled flag is the only gate.
        "approved": bundled || approved.iter().any(|a| a == name),
        "health": live.get(name).cloned().unwrap_or(serde_json::Value::Null),
    })
}

/// A bundled catalog server with no config entry yet: always disabled,
/// first-party (no consent record needed).
fn catalog_server_json(s: &mcp_catalog::BundledMcpServer) -> serde_json::Value {
    let shape = ServerShape::from_catalog(s);
    let mut readiness = shape.problems(&s.name).into_iter().next();
    // Surface missing secrets as the readiness blocker: the row is
    // pinned and shape-valid, so env is the usual reason it cannot run.
    if readiness.is_none() {
        if let Some(missing) = s.requires_env.iter().find(|n| std::env::var(n).is_err()) {
            readiness = Some(format!("env {missing} is not set"));
        }
    }
    let mut privilege_notes = s.privilege_notes.clone();
    if let Some(w) = &s.deprecated_warning {
        privilege_notes.push_str("\n\nDeprecation warning: ");
        privilege_notes.push_str(w);
    }
    serde_json::json!({
        "name": s.name.clone(),
        "transport": s.transport.clone(),
        "command": s.command.clone(),
        "args": s.args.clone(),
        "url": s.url.clone(),
        "requires_env": s.requires_env.clone(),
        "needs_credentials": !s.requires_env.is_empty(),
        "enabled": false,
        "bundled": true,
        // Package-only pin: unpinned (remote) recipes get no stamp.
        "pinned_version": s.package.clone(),
        "description": s.description.clone(),
        "privilege_notes": privilege_notes,
        "readiness": readiness,
        "approved": true,
        "health": serde_json::Value::Null,
    })
}

/// Config-defined servers, primary over declarations.
fn config_servers(data_dir: &Path) -> Vec<(String, pantheon_api::config::McpServerEntry)> {
    let raw = std::fs::read_to_string(config_path(data_dir)).unwrap_or_default();
    let cfg: Result<pantheon_api::config::Config, _> = toml::from_str(&raw);
    cfg.map(|c| {
        c.mcp
            .map(|m| m.servers.into_iter().collect::<Vec<_>>())
            .unwrap_or_default()
    })
    .unwrap_or_default()
}

/// `GET /api/mcp/servers` - bundled catalog entries, config-section
/// servers, and legacy declarations, each merged with its live health
/// snapshot and approval state. The config entry is the enablement
/// state; declarations shadowed by the config show under the config
/// group instead of twice.
pub fn list(app: &App) -> Response {
    json_ok(list_json(app))
}

fn list_json(app: &App) -> serde_json::Value {
    let decls = read_mcp_declarations(&app.data_dir);
    let live = live_map(&app.data_dir);
    let approved = approved_names(&app.data_dir);
    let cfg_servers = config_servers(&app.data_dir);
    let config_names: Vec<&str> = cfg_servers.iter().map(|(n, _)| n.as_str()).collect();
    let catalog = mcp_catalog::bundled_catalog();
    let bundled_group: Vec<serde_json::Value> = catalog
        .iter()
        .filter(|s| !config_names.contains(&s.name.as_str()))
        .map(catalog_server_json)
        .collect();
    let mut out: Vec<serde_json::Value> = Vec::new();
    if !bundled_group.is_empty() {
        out.push(serde_json::json!({
            "source": "bundled catalog",
            "origin": "bundled",
            "note": "first-party servers, all disabled by default; enabling materializes the pinned row into config.toml",
            "servers": bundled_group,
        }));
    }
    let cfg_group: Vec<serde_json::Value> = cfg_servers
        .iter()
        .map(|(n, e)| config_server_json(n, e, &live, &approved))
        .collect();
    if !cfg_group.is_empty() {
        out.push(serde_json::json!({
            "source": "config.toml",
            "origin": "config",
            "note": "the enablement state: this is what the dashboard, the mobile app, and the agent's enable_mcp tool all read and write",
            "servers": cfg_group,
        }));
    }
    let decl_groups: Vec<serde_json::Value> = decls
        .iter()
        .map(|d| {
            // Declarations shadowed by the config section show under the
            // config group instead of twice.
            let servers: Vec<serde_json::Value> = d
                .servers
                .iter()
                .filter(|s| !config_names.contains(&s.name.as_str()))
                .map(|s| server_json(s, &live, &approved))
                .collect();
            serde_json::json!({
                "source": d.source,
                "origin": "declaration",
                "note": "legacy: read-only here; enabling a declaration server materializes it into config.toml",
                "servers": servers,
            })
        })
        .filter(|g| !g["servers"].as_array().map(|a| a.is_empty()).unwrap_or(true))
        .collect();
    out.extend(decl_groups);
    let pending: Vec<&str> = live
        .values()
        .filter(|s| s.get("status").and_then(|v| v.as_str()) == Some("unapproved"))
        .filter_map(|s| s.get("name").and_then(|v| v.as_str()))
        .collect();
    serde_json::json!({
        "servers": out,
        "pending_approval": pending,
        "note": "a server launches iff enabled AND (bundled OR approved); live health comes from the session launcher's mcp/live.json snapshot",
    })
}

/// `GET /api/mcp/health` - the launcher's live snapshot verbatim
/// (status, tool counts, connects, failures, backoff errors), plus a
/// freshness note. No session has run yet = empty servers with a note.
pub fn health(app: &App) -> Response {
    let text =
        std::fs::read_to_string(mcp_dir(&app.data_dir).join("live.json")).unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::json!({}));
    let empty = v
        .get("servers")
        .and_then(|s| s.as_array())
        .map(|a| a.is_empty())
        .unwrap_or(true);
    json_ok(serde_json::json!({
        "live": v,
        "note": if empty {
            "no live MCP state yet - start a session to launch servers"
        } else {
            "snapshot written by the session's MCP manager"
        },
    }))
}

/// Find a server's shape from any source: config first, then the
/// bundled catalog, then declarations.
fn find_shape(data_dir: &Path, name: &str) -> Option<(ServerShape, &'static str)> {
    if let Some((_, e)) = config_servers(data_dir)
        .into_iter()
        .find(|(n, _)| n == name)
    {
        return Some((ServerShape::from_config(&e), "config"));
    }
    if let Some(s) = mcp_catalog::find(name) {
        return Some((ServerShape::from_catalog(&s), "bundled"));
    }
    for d in read_mcp_declarations(data_dir) {
        if let Some(s) = d.servers.iter().find(|s| s.name == name) {
            return Some((ServerShape::from_declaration(s), "declaration"));
        }
    }
    None
}

/// Write the bundled-catalog enablement state. Materialization funnels
/// through the canonical writer
/// [`pantheon_api::mcp_catalog::materialize_recipe_table`] - the same
/// writer the agent's `enable_mcp` tool uses - so both surfaces produce
/// byte-identical tables and first-writer-wins divergence is impossible.
/// This is the no-arbitrary-command boundary: only catalog names are
/// ever written. An empty table is materialized from the canonical
/// recipe (disabled by default), so the launcher never sees a bare
/// `enabled = true` with no command; keys the operator already set are
/// left alone.
fn write_bundled_enabled(data_dir: &Path, name: &str, enabled: bool) -> Result<(), String> {
    let recipe =
        mcp_catalog::find(name).ok_or_else(|| format!("'{name}' is not a bundled MCP server"))?;
    let mut doc = read_config_doc(data_dir)?;
    let table = server_table(&mut doc, name)?;
    if table.is_empty() {
        mcp_catalog::materialize_recipe_table(table, &recipe);
    }
    table.insert("enabled".to_string(), toml::Value::Boolean(enabled));
    write_config_doc(data_dir, &doc)?;
    Ok(())
}

/// Write the ONE enablement state: `[mcp.servers.<name>].enabled` in
/// `config.toml`. A name unknown to the config, the bundled catalog,
/// and the declarations is a 404 - there is no path that enables an
/// arbitrary command. Enabling a bundled or declaration-only server
/// materializes its full shape into the config table first, so the
/// launcher never sees a bare `enabled = true` with no command.
fn write_enabled(data_dir: &Path, name: &str, enabled: bool) -> Result<&'static str, String> {
    if !safe_name(name) {
        return Err("unsafe server name".to_string());
    }
    // Bundled names funnel through the sibling catalog: the pinned row
    // is the only shape the agent, dashboard, and app may ever write.
    if is_bundled(name) {
        write_bundled_enabled(data_dir, name, enabled)?;
        return Ok("bundled");
    }
    let mut doc = read_config_doc(data_dir)?;
    let table = server_table(&mut doc, name)?;
    if table.is_empty() {
        // Declaration-only server: materialize its shape so the config
        // entry is complete on its own.
        let mut shape_opt = None;
        for d in read_mcp_declarations(data_dir) {
            if let Some(s) = d.servers.iter().find(|s| s.name == name) {
                shape_opt = Some(ServerShape::from_declaration(s));
                break;
            }
        }
        match shape_opt {
            Some(shape) => materialize_shape(table, &shape),
            None => return Err("no server by that name".to_string()),
        }
    }
    table.insert("enabled".to_string(), toml::Value::Boolean(enabled));
    write_config_doc(data_dir, &doc)?;
    Ok("config")
}

/// `POST /api/mcp/servers/:name/enable` (or `/disable`, `/toggle`).
/// Writes the config enablement state; the response is the refreshed
/// server list.
pub fn set_enabled(app: &App, name: &str, enabled: bool) -> Response {
    match write_enabled(&app.data_dir, name, enabled) {
        Ok(origin) => {
            let mut body = list_json(app);
            body["toggled"] = serde_json::json!({"name": name, "enabled": enabled, "via": origin});
            json_ok(body)
        }
        Err(e) if e == "no server by that name" => err_json(404, "NOT_FOUND", &e),
        Err(e) => bad_json(&e),
    }
}

/// `POST /api/mcp/servers/:name/toggle` - flip the effective enabled
/// state, whatever source currently defines it.
pub fn toggle(app: &App, name: &str) -> Response {
    if !safe_name(name) {
        return bad_json("unsafe server name");
    }
    let currently = config_servers(&app.data_dir)
        .into_iter()
        .find(|(n, _)| n == name)
        .map(|(_, e)| e.enabled)
        .unwrap_or_else(|| {
            // No config entry yet: bundled catalog entries are disabled
            // by default; declaration servers keep their file flag.
            if is_bundled(name) {
                false
            } else {
                read_mcp_declarations(&app.data_dir)
                    .iter()
                    .flat_map(|d| &d.servers)
                    .find(|s| s.name == name)
                    .map(|s| s.enabled)
                    .unwrap_or(false)
            }
        });
    set_enabled(app, name, !currently)
}

fn parse_new_server(body: &serde_json::Value) -> Result<ServerShape, String> {
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "name is required".to_string())?;
    if !safe_name(name) {
        return Err("unsafe server name".to_string());
    }
    let transport = body
        .get("transport")
        .and_then(|v| v.as_str())
        .unwrap_or("stdio");
    if !matches!(transport, "stdio" | "http" | "sse") {
        return Err("transport must be stdio, http, or sse".to_string());
    }
    let command = body
        .get("command")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let url = body
        .get("url")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let args: Vec<String> = body
        .get("args")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let requires_env: Vec<String> = body
        .get("requires_env")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let shape = ServerShape {
        transport: transport.to_string(),
        command,
        args,
        url,
        requires_env,
    };
    // Validate the shape WITHOUT a live handshake: a stdio command must
    // resolve on PATH (or be an existing path); an sse/http URL must
    // parse. No process is spawned and no connection is opened - deeper
    // verification is `pantheon doctor`'s job.
    if let Some(problem) = shape.problems(name).into_iter().next() {
        return Err(problem);
    }
    Ok(shape)
}

/// `POST /api/mcp/servers` - `{name, transport, command?, args?, url?,
/// requires_env?, confirm: true}`.
///
/// Custom servers are added **disabled** into `[mcp.servers.<name>]` in
/// `config.toml` (the one enablement state). The shape is validated
/// without a live handshake; a name the bundled catalog already owns is
/// rejected (enable the catalog entry instead of shadowing it).
pub fn add(app: &App, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if !body
        .get("confirm")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return bad_json("add requires confirm: true");
    }
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    if !safe_name(name) {
        return bad_json("name is required");
    }
    if is_bundled(name) {
        return bad_json(&format!(
            "'{name}' is a bundled server: enable the catalog entry instead of adding a custom one"
        ));
    }
    if find_shape(&app.data_dir, name).is_some() {
        return bad_json(&format!("a server named '{name}' already exists"));
    }
    let shape = match parse_new_server(&body) {
        Ok(s) => s,
        Err(e) => return bad_json(&e),
    };
    let mut doc = match read_config_doc(&app.data_dir) {
        Ok(d) => d,
        Err(e) => return err_json(500, "MCP", &e),
    };
    let table = match server_table(&mut doc, name) {
        Ok(t) => t,
        Err(e) => return err_json(500, "MCP", &e),
    };
    materialize_shape(table, &shape);
    // Disabled by default: consent and enabling are separate explicit acts.
    table.insert("enabled".to_string(), toml::Value::Boolean(false));
    if let Err(e) = write_config_doc(&app.data_dir, &doc) {
        return err_json(500, "MCP", &e);
    }
    let mut body = list_json(app);
    body["added"] = serde_json::json!({"name": name, "enabled": false});
    json_ok(body)
}

/// `POST /api/mcp/servers/:name/approve` - `{confirm: true}`.
///
/// Record operator consent for a **custom** server in the unified
/// approval store, binding the server's name to its content hash (the
/// binary bytes, or the URL bytes for remote transports). The hash is
/// computed locally: no process is spawned and no connection is opened.
/// The UI shows the privilege warning before calling; the endpoint only
/// persists the record.
///
/// Bundled servers are first-party and skip consent (409) - enabling is
/// their only gate. Approval is consent, not enablement: an approved
/// server still needs `enabled`.
pub fn approve(app: &App, name: &str, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if !body
        .get("confirm")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return bad_json("approve requires confirm: true");
    }
    if !safe_name(name) {
        return bad_json("unsafe server name");
    }
    if is_bundled(name) {
        return err_json(
            409,
            "MCP_BUNDLED",
            "bundled servers are first-party: no consent record needed, enabling is the gate",
        );
    }
    let (shape, _) = match find_shape(&app.data_dir, name) {
        Some(found) => found,
        None => return err_json(404, "NOT_FOUND", "no server by that name"),
    };
    if let Some(problem) = shape.problems(name).into_iter().next() {
        return bad_json(&format!("cannot approve an invalid server: {problem}"));
    }
    let hash = match content_hash(&shape) {
        Ok(h) => h,
        Err(e) => {
            return err_json(
                400,
                "MCP_UNHASHABLE",
                &format!("cannot fingerprint this server: {e}"),
            )
        }
    };
    let rec = ApprovalRecord {
        plugin: name.to_string(),
        // No handshake here, so no self-reported version: the hash is
        // the identity the launcher checks. A later `pantheon mcp
        // approve` (with handshake) overwrites with the real version.
        version: "unverified".to_string(),
        dir_hash: hash,
        approved_at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    };
    if let Err(e) = ApprovalStore::open(&mcp_dir(&app.data_dir)).record(rec.clone()) {
        return err_json(500, "MCP_APPROVE", &e.to_string());
    }
    json_ok(serde_json::json!({
        "ok": true, "name": name, "approved": true,
        "version": rec.version,
        "note": "consent recorded; the server still needs to be enabled before it launches",
    }))
}

/// `DELETE /api/mcp/servers/:name` - remove a custom server. Config
/// entries are deleted from `config.toml`; legacy declaration entries
/// from their declaration file. Bundled catalog entries cannot be
/// deleted (disable them instead).
pub fn delete(app: &App, name: &str) -> Response {
    if !safe_name(name) {
        return bad_json("unsafe server name");
    }
    if is_bundled(name) {
        return err_json(
            409,
            "MCP_BUNDLED",
            "bundled servers cannot be deleted; disable the catalog entry instead",
        );
    }
    // Config entry first: it is the primary store.
    if let Ok(mut doc) = read_config_doc(&app.data_dir) {
        let removed = doc
            .as_table_mut()
            .and_then(|r| r.get_mut("mcp"))
            .and_then(|m| m.as_table_mut())
            .and_then(|m| m.get_mut("servers"))
            .and_then(|s| s.as_table_mut())
            .map(|t| t.remove(name).is_some())
            .unwrap_or(false);
        if removed {
            // Prune now-empty parents so the file does not accumulate
            // empty `[mcp.servers]` tables.
            if let Some(mcp) = doc
                .as_table_mut()
                .and_then(|r| r.get_mut("mcp"))
                .and_then(|m| m.as_table_mut())
            {
                let servers_empty = mcp
                    .get("servers")
                    .and_then(|s| s.as_table())
                    .map(|t| t.is_empty())
                    .unwrap_or(false);
                if servers_empty {
                    mcp.remove("servers");
                }
                if mcp.is_empty() {
                    doc.as_table_mut().map(|r| r.remove("mcp"));
                }
            }
            if let Err(e) = write_config_doc(&app.data_dir, &doc) {
                return err_json(500, "MCP", &e);
            }
            return list(app);
        }
    }
    // Fall back to legacy declaration files.
    let decls = read_mcp_declarations(&app.data_dir);
    let mut touched: Option<String> = None;
    for d in &decls {
        if d.servers.iter().any(|s| s.name == name) {
            touched = Some(d.source.clone());
            break;
        }
    }
    match touched {
        Some(source) => rewrite_declaration(app, &source, |servers| {
            let before = servers.len();
            servers.retain(|s| s.name != name);
            if servers.len() == before {
                return Err("server vanished".to_string());
            }
            Ok(())
        }),
        None => err_json(404, "NOT_FOUND", "no server by that name"),
    }
}

/// `POST /api/mcp/reload` - re-read the servers from disk.
pub fn reload(app: &App) -> Response {
    list(app)
}

/// `POST /api/mcp/servers/:name/test` - a real probe: for stdio, resolve
/// the command on PATH (or as an absolute path); for http/sse, attempt a
/// TCP connect to the URL's host:port with a 3s timeout. The readiness
/// blocker, if any, is reported alongside.
pub fn test(app: &App, name: &str) -> Response {
    let (shape, origin) = match find_shape(&app.data_dir, name) {
        Some(found) => found,
        None => return err_json(404, "NOT_FOUND", "no server by that name"),
    };
    if let Some(blocker) = shape.problems(name).into_iter().next() {
        return json_ok(serde_json::json!({
            "ok": false, "name": name, "origin": origin,
            "detail": format!("not ready: {blocker}"),
        }));
    }
    let (ok, detail) = match shape.transport.as_str() {
        "stdio" => {
            let cmd = shape.command.as_deref().unwrap_or("");
            (resolve_command(cmd).is_some(), format!("command {cmd:?}"))
        }
        _ => match shape.url.as_deref() {
            Some(url) => probe_url(url),
            None => (false, "no url".to_string()),
        },
    };
    json_ok(serde_json::json!({"ok": ok, "name": name, "origin": origin, "detail": detail}))
}

fn decl_path(data_dir: &Path, source: &str) -> Result<PathBuf, String> {
    if source.trim().is_empty()
        || source.contains('/')
        || source.contains('\\')
        || source.contains("..")
    {
        return Err(format!("unsafe declaration source {source:?}"));
    }
    let dir = mcp_dir(data_dir);
    let p = dir.join(format!("{source}.json"));
    if !p.starts_with(&dir) {
        return Err("invalid declaration path".to_string());
    }
    Ok(p)
}

fn atomic_write_json(path: &Path, v: &serde_json::Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
    Ok(())
}

/// Rewrite one legacy declaration file with `f` applied to its servers.
/// Only used for deleting declaration-only servers; enablement writes
/// go to `config.toml`.
fn rewrite_declaration(
    app: &App,
    source: &str,
    f: impl FnOnce(&mut Vec<McpServer>) -> Result<(), String>,
) -> Response {
    let path = match decl_path(&app.data_dir, source) {
        Ok(p) => p,
        Err(e) => return bad_json(&e),
    };
    let mut servers: Vec<McpServer> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|b| serde_json::from_str::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("servers").cloned())
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    if let Err(e) = f(&mut servers) {
        return bad_json(&e);
    }
    let doc = serde_json::json!({
        "source": source,
        "note": "Managed by the Pantheon dashboard. Credentials are declared, not stored.",
        "servers": servers,
    });
    if let Err(e) = atomic_write_json(&path, &doc) {
        return err_json(500, "MCP", &format!("write: {e}"));
    }
    list(app)
}

fn resolve_command(cmd: &str) -> Option<PathBuf> {
    if cmd.is_empty() {
        return None;
    }
    let p = Path::new(cmd);
    if p.components().count() > 1 {
        // A path, not a bare name.
        return p.exists().then(|| p.to_path_buf());
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(cmd))
            .find(|p| p.is_file())
    })
}

fn probe_url(url: &str) -> (bool, String) {
    // Minimal: scheme://host[:port][/...]. No regex crate, no URL crate.
    let after = url.split("://").nth(1).unwrap_or(url);
    let host_port = after.split('/').next().unwrap_or("");
    if host_port.is_empty() {
        return (false, "unparseable url".to_string());
    }
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => match p.parse::<u16>() {
            Ok(port) => (h, port),
            Err(_) => (host_port, 80),
        },
        None => (host_port, 80),
    };
    let addrs: Vec<SocketAddr> = match (host, port).to_socket_addrs() {
        Ok(a) => a.collect(),
        Err(e) => return (false, format!("DNS failed: {e}")),
    };
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, Duration::from_secs(3)) {
            Ok(_) => return (true, format!("connected to {addr}")),
            Err(_) => continue,
        }
    }
    (false, format!("no route to {host}:{port}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scratch data dir without adding a dev-dependency: unique per
    /// test recipe, removed afterwards.
    fn scratch_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "pantheon-dash-mcp-test-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("scratch dir");
        p
    }

    fn read_server_table(data_dir: &Path, name: &str) -> toml::Value {
        let text = std::fs::read_to_string(config_path(data_dir)).expect("config written");
        let doc: toml::Value = text.parse().expect("config parses");
        doc.get("mcp")
            .and_then(|m| m.get("servers"))
            .and_then(|s| s.get(name))
            .cloned()
            .expect("server table present")
    }

    /// #7: enabling the same bundled server through the dashboard write
    /// path and through the agent-tool write path must produce
    /// byte-identical `[mcp.servers.<name>]` tables - one canonical
    /// recipe, no first-writer-wins divergence. (Fails before the fix:
    /// the dashboard materializes the launcher copy - e.g. playwright
    /// without `-y`, notion unpinned - while the agent tool materializes
    /// the api copy.)
    #[test]
    fn bundled_enablement_paths_materialize_identically() {
        for recipe in pantheon_api::mcp_catalog::bundled_catalog() {
            let dash_dir = scratch_dir(&format!("dash-{}", recipe.name));
            let agent_dir = scratch_dir(&format!("agent-{}", recipe.name));
            write_bundled_enabled(dash_dir.as_path(), &recipe.name, true)
                .expect("dashboard enable");
            pantheon_api::mcp_catalog::set_enabled(agent_dir.as_path(), &recipe.name, true)
                .expect("agent enable");
            let dash_table = read_server_table(dash_dir.as_path(), &recipe.name);
            let agent_table = read_server_table(agent_dir.as_path(), &recipe.name);
            assert_eq!(
                dash_table, agent_table,
                "materialization diverged for {}",
                recipe.name
            );
            let _ = std::fs::remove_dir_all(&dash_dir);
            let _ = std::fs::remove_dir_all(&agent_dir);
        }
    }
}
