//! `pantheon mcp`: manage MCP servers.
//!
//! Two sources declare servers: `[mcp.servers.<name>]` in config.toml
//! (primary) and the migration declarations at `<data_dir>/mcp/*.json`
//! (fill in names the config section does not define). Both feed the
//! launcher in `pantheon_mcp::manager`, which registers each server's
//! tools as `mcp_<server>_<tool>` in the agent's tool registry.
//!
//! A server never runs without an explicit approval: `pantheon mcp
//! approve <name>` prints exactly what the server is, shows the warning,
//! and asks for confirmation. The approval binds the server's name,
//! reported version, and a content hash — any upgrade or change
//! invalidates it and the operator is asked again.

use crate::config;
use crate::terminal::data_dir;
use pantheon_api::approval::ApprovalStore;
use pantheon_mcp::manager::{McpManager, McpServerSpec, McpTransport, ServerStatus};
use pantheon_migration::{read_mcp_declarations, McpServer};
use std::path::Path;

/// Re-exported from `pantheon_migration` (single source of truth, shared
/// with the dashboard).
pub use pantheon_migration::server_readiness;

/// Build a manager wired like the session's: the data-dir scope,
/// secrets-backed env resolver, and resolved server specs.
///
/// `pub(crate)` so the nightly repair adapter can drive the same manager
/// the sessions use. Configured, not connected: no servers are launched.
pub(crate) fn manager_for(dd: &Path) -> McpManager {
    let cfg = config::Config::load_or_report(dd);
    let secrets = config::chat_secrets(cfg.as_ref());
    let declarations = read_mcp_declarations(dd);
    let mcfg =
        config::resolve_mcp_section(cfg.as_ref().and_then(|c| c.mcp.as_ref()), &declarations);
    let m = McpManager::new(dd.to_path_buf());
    m.set_env_resolver(std::sync::Arc::new(move |var: &str| {
        secrets
            .resolve(var)
            .ok()
            .flatten()
            .map(|v| v.expose().to_owned())
            .or_else(|| std::env::var(var).ok())
    }));
    m.configure(mcfg.servers);
    m
}

/// One row of the merged server view: config section first, declarations
/// fill names the section does not define.
struct Row {
    name: String,
    origin: &'static str,
    spec: McpServerSpec,
    /// Readiness blocker from the declaration, if this row came from one.
    decl_blocker: Option<String>,
}

fn collect(dd: &Path) -> Vec<Row> {
    let cfg = config::Config::load_or_report(dd);
    let declarations = read_mcp_declarations(dd);
    let mcfg =
        config::resolve_mcp_section(cfg.as_ref().and_then(|c| c.mcp.as_ref()), &declarations);
    let mut rows: Vec<Row> = mcfg
        .servers
        .into_iter()
        .map(|spec| {
            let origin = if cfg
                .as_ref()
                .and_then(|c| c.mcp.as_ref())
                .is_some_and(|m| m.servers.contains_key(&spec.name))
            {
                "config"
            } else {
                "declaration"
            };
            Row {
                name: spec.name.clone(),
                origin,
                spec,
                decl_blocker: None,
            }
        })
        .collect();
    // Declarations that resolve_mcp_section skipped (disabled or
    // malformed) still show in `list`, marked with their blocker.
    for d in &declarations {
        for s in &d.servers {
            if rows.iter().any(|r| r.name == s.name) {
                continue;
            }
            rows.push(Row {
                name: s.name.clone(),
                origin: "declaration",
                spec: decl_spec(s),
                decl_blocker: Some(decl_blocker(s)),
            });
        }
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

/// Best-effort spec for a declaration row that the resolver skipped.
fn decl_spec(s: &McpServer) -> McpServerSpec {
    McpServerSpec {
        name: s.name.clone(),
        transport: match s.transport.as_str() {
            "sse" => McpTransport::Sse,
            "http" => McpTransport::Http,
            _ => McpTransport::Stdio,
        },
        command: s.command.clone(),
        args: s.args.clone(),
        env: s
            .requires_env
            .iter()
            .map(|v| (v.clone(), format!("env:{v}")))
            .collect(),
        url: s.url.clone(),
        enabled: s.enabled,
        timeout: std::time::Duration::from_secs(30),
    }
}

fn decl_blocker(s: &McpServer) -> String {
    if !s.enabled {
        return "disabled in declaration".to_string();
    }
    server_readiness(s).unwrap_or_else(|| "skipped by resolver".to_string())
}

fn target_of(spec: &McpServerSpec) -> String {
    match spec.transport {
        McpTransport::Stdio => {
            let cmd = spec.command.as_deref().unwrap_or("-");
            if spec.args.is_empty() {
                cmd.to_string()
            } else {
                format!("{cmd} {}", spec.args.join(" "))
            }
        }
        _ => spec.url.as_deref().unwrap_or("-").to_string(),
    }
}

fn transport_name(t: &McpTransport) -> &'static str {
    match t {
        McpTransport::Stdio => "stdio",
        McpTransport::Sse => "sse",
        McpTransport::Http => "http",
    }
}

/// Approval state for the list view: a stored record exists. Whether it
/// still matches the live version + content hash is verified at connect
/// time — that check is authoritative, this column is a hint.
fn approved_names(dd: &Path) -> Vec<String> {
    ApprovalStore::open(&dd.join("mcp")).names()
}

pub fn cmd_mcp(args: &[String]) {
    let verb = args.first().map(|s| s.as_str()).unwrap_or("list");
    match verb {
        "list" | "" => list(args),
        "status" => status(args),
        "health" => health(args),
        "approve" => approve(args),
        "enable" => set_decl_enabled(args, true),
        "disable" => set_decl_enabled(args, false),
        other => {
            eprintln!("mcp: unknown verb '{other}'");
            eprintln!("  usage: pantheon mcp <list|status|health|approve|enable|disable> [--json]");
            std::process::exit(2);
        }
    }
}

fn need_name(args: &[String], verb: &str) -> String {
    match args.get(1) {
        Some(n) if !n.starts_with('-') => n.clone(),
        _ => {
            eprintln!("mcp {verb}: needs a server name");
            std::process::exit(2);
        }
    }
}

fn has_json(args: &[String]) -> bool {
    args.iter().any(|a| a == "--json")
}

fn list(args: &[String]) {
    let dd = data_dir();
    let rows = collect(&dd);
    let approved = approved_names(&dd);
    if has_json(args) {
        let v: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "name": r.name,
                    "origin": r.origin,
                    "transport": transport_name(&r.spec.transport),
                    "target": target_of(&r.spec),
                    "env": r.spec.env.keys().cloned().collect::<Vec<_>>(),
                    "approved": approved.iter().any(|a| a == &r.name),
                    "blocker": r.decl_blocker.clone(),
                })
            })
            .collect();
        match serde_json::to_string_pretty(&v) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("mcp: encode failed: {e}"),
        }
        return;
    }
    if rows.is_empty() {
        println!(
            "no MCP servers declared — add `[mcp.servers.<name>]` to {} or run `pantheon migrate apply`",
            dd.join("config.toml").display()
        );
        return;
    }
    println!(
        "{:<26} {:<11} {:<7} {:<8} TARGET",
        "NAME", "ORIGIN", "TRANS", "APPROVED"
    );
    for r in &rows {
        let appr = if approved.iter().any(|a| a == &r.name) {
            "yes"
        } else {
            "no"
        };
        let mut line = format!(
            "{:<26} {:<11} {:<7} {:<8} {}",
            r.name,
            r.origin,
            transport_name(&r.spec.transport),
            appr,
            target_of(&r.spec)
        );
        if let Some(b) = &r.decl_blocker {
            line.push_str(&format!("  ({b})"));
        }
        println!("{line}");
    }
    println!();
    println!(
        "{} server(s); unapproved servers never launch — `pantheon mcp approve <name>`",
        rows.len()
    );
}

/// `status`: the live snapshot from `<data_dir>/mcp/live.json`, written
/// by the manager whenever a session registers tools or runs a health
/// check. No session running = the last known state.
fn status(args: &[String]) {
    let dd = data_dir();
    let live = read_live(&dd);
    let servers = live
        .as_ref()
        .and_then(|v| v.get("servers"))
        .and_then(|v| v.as_array());
    if has_json(args) {
        match serde_json::to_string_pretty(&live.unwrap_or(serde_json::json!({}))) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("mcp: encode failed: {e}"),
        }
        return;
    }
    let Some(servers) = servers else {
        println!("no live MCP state yet — start a session to launch servers");
        return;
    };
    if servers.is_empty() {
        println!("no MCP servers configured");
        return;
    }
    println!(
        "{:<26} {:<15} {:>5} {:>8} {:>8}",
        "NAME", "STATUS", "TOOLS", "CONNECTS", "FAILS"
    );
    let mut rows: Vec<&serde_json::Value> = servers.iter().collect();
    rows.sort_by(|a, b| {
        a.get("name")
            .and_then(|v| v.as_str())
            .cmp(&b.get("name").and_then(|v| v.as_str()))
    });
    for s in rows {
        let n = s.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        let st = s.get("status").and_then(|v| v.as_str()).unwrap_or("?");
        let tools = s.get("tools").and_then(|v| v.as_u64()).unwrap_or(0);
        let connects = s.get("connects").and_then(|v| v.as_u64()).unwrap_or(0);
        let fails = s.get("failures").and_then(|v| v.as_u64()).unwrap_or(0);
        let mut line = format!("{n:<26} {st:<15} {tools:>5} {connects:>8} {fails:>8}");
        if let Some(err) = s.get("last_error").and_then(|v| v.as_str()) {
            line.push_str(&format!("  ({err})"));
        }
        if st == "unapproved" {
            line.push_str("  [needs `mcp approve`]");
        }
        println!("{line}");
    }
}

/// Synthesize the CUA driver's MCP spec into the manager when it is not
/// already declared. Mirrors what the runtime does at tool-registration
/// time: the driver answers to the ComputerUse group and `[computer_use]`,
/// not to `[mcp.servers]`.
fn ensure_driver_spec(m: &McpManager, dd: &Path) {
    if m.spec(pantheon_runtime::computer::CUA_DRIVER_SERVER)
        .is_some()
    {
        return;
    }
    let cfg = config::Config::load_or_report(dd);
    let section = cfg.as_ref().and_then(|c| c.computer_use.clone());
    let (driver, binary) = match section.as_ref() {
        Some(s) => (s.driver.as_deref(), s.binary.as_deref()),
        None => (None, None),
    };
    if let Some(spec) = pantheon_runtime::computer::cua_driver_spec(driver, binary) {
        let mut specs: Vec<_> = m
            .spec_names()
            .into_iter()
            .filter_map(|n| m.spec(&n))
            .collect();
        specs.push(spec);
        m.configure(specs);
    }
}

/// `health <name>`: full detail for one server from the live snapshot.
fn health(args: &[String]) {
    let name = need_name(args, "health");
    let dd = data_dir();
    let live = read_live(&dd);
    let entry = live.as_ref().and_then(|v| v.get("servers")).and_then(|v| {
        v.as_array()?
            .iter()
            .find(|s| s.get("name").and_then(|n| n.as_str()) == Some(name.as_str()))
    });
    if has_json(args) {
        match serde_json::to_string_pretty(&entry.unwrap_or(&serde_json::Value::Null)) {
            Ok(s) => println!("{s}"),
            Err(e) => eprintln!("mcp: encode failed: {e}"),
        }
        return;
    }
    let Some(e) = entry else {
        eprintln!("mcp: no live state for '{name}' — is it configured? (`mcp list`)");
        std::process::exit(1);
    };
    let get = |k: &str| e.get(k).map(|v| v.to_string()).unwrap_or_default();
    println!("server:  {name}");
    println!("status:  {}", get("status"));
    println!("target:  {}", get("target"));
    println!("tools:   {}", get("tools"));
    println!("connects: {}", get("connects"));
    println!("failures: {}", get("failures"));
    if !get("last_error").is_empty() && get("last_error") != "null" {
        println!("error:   {}", get("last_error"));
    }
}

/// `approve <name> [--yes]`: the explicit approval flow. Prints exactly
/// what the server is, shows the warning, and requires confirmation —
/// interactive (type the server name) unless `--yes` is passed.
fn approve(args: &[String]) {
    let name = need_name(args, "approve");
    let yes = args.iter().any(|a| a == "--yes" || a == "-y");
    let dd = data_dir();
    let m = manager_for(&dd);
    // The CUA driver is not declared in `[mcp.servers]` — the runtime
    // synthesizes its spec from `[computer_use]` when the ComputerUse
    // group is on. Approval must see the same spec, or the driver could
    // never be approved.
    ensure_driver_spec(&m, &dd);
    let spec = match m.spec(&name) {
        Some(s) => s,
        None => {
            eprintln!(
                "mcp: no server named '{name}' — `pantheon mcp list` shows the declared ones"
            );
            std::process::exit(1);
        }
    };
    println!("You are about to approve the MCP server '{name}'.");
    println!();
    match spec.transport {
        McpTransport::Stdio => println!(
            "  launch: {} {}",
            spec.command.as_deref().unwrap_or("?"),
            spec.args.join(" ")
        ),
        McpTransport::Sse => println!("  connect (sse): {}", spec.url.as_deref().unwrap_or("?")),
        McpTransport::Http => {
            println!("  connect (http): {}", spec.url.as_deref().unwrap_or("?"))
        }
    }
    if spec.env.is_empty() {
        println!("  env:    (none declared)");
    } else {
        let mut names: Vec<&String> = spec.env.keys().collect();
        names.sort();
        println!(
            "  env:    {}",
            names
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    println!();
    println!("Approving lets Pantheon launch this third-party server and hand it");
    println!("your agent's tool calls. Only approve servers you trust: a malicious");
    println!("server sees everything the agent can see. Secrets listed above resolve");
    println!("from your vault at launch time and are never printed.");
    println!();
    println!("The approval binds the server's name, its reported version, and a");
    println!("content hash of what would run. Any upgrade or change invalidates it");
    println!("and you will be asked again.");
    println!();
    if !yes {
        use std::io::Write as _;
        print!("Type the server name to approve it: ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() || line.trim() != name {
            println!("mcp: approval aborted");
            std::process::exit(1);
        }
    }
    match m.approve_server(&name) {
        Ok(rec) => {
            let hash12: String = rec.dir_hash.chars().take(12).collect();
            println!(
                "approved '{name}' (version {}, hash {hash12}…)",
                rec.version
            );
        }
        Err(e) => {
            eprintln!("mcp: approve failed: {e}");
            std::process::exit(1);
        }
    }
}

/// `enable` / `disable <name>`: toggle a *declaration* server. Servers
/// defined in config.toml are edited there, not rewritten by this verb.
fn set_decl_enabled(args: &[String], enabled: bool) {
    let verb = if enabled { "enable" } else { "disable" };
    let name = need_name(args, verb);
    let dd = data_dir();
    let declarations = read_mcp_declarations(&dd);
    let source = declarations
        .iter()
        .find(|d| d.servers.iter().any(|s| s.name == name))
        .map(|d| d.source.clone());
    let Some(source) = source else {
        eprintln!("mcp: no declared server named '{name}'");
        eprintln!("  (config-defined servers are toggled with `[mcp.servers.{name}] enabled` in config.toml)");
        std::process::exit(1);
    };
    let path = dd.join("mcp").join(format!("{source}.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut v: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("mcp: cannot parse {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    let mut touched = false;
    if let Some(servers) = v.get_mut("servers").and_then(|s| s.as_array_mut()) {
        for s in servers {
            if s.get("name").and_then(|n| n.as_str()) == Some(name.as_str()) {
                s["enabled"] = serde_json::Value::Bool(enabled);
                touched = true;
            }
        }
    }
    if !touched {
        eprintln!("mcp: server '{name}' not found in {}", path.display());
        std::process::exit(1);
    }
    // Atomic rewrite: temp file + rename.
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_string_pretty(&v).unwrap_or_default()).is_err()
        || std::fs::rename(&tmp, &path).is_err()
    {
        eprintln!("mcp: failed to rewrite {}", path.display());
        std::process::exit(1);
    }
    println!("mcp: {verb}d '{name}'");
}

fn read_live(dd: &Path) -> Option<serde_json::Value> {
    std::fs::read_to_string(dd.join("mcp").join("live.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
}

/// Re-scan declarations + config, push the resolved specs into the
/// session's manager, and report readiness. Called by `/mcp reload` in
/// the TUI: the manager drops live clients for changed specs and
/// reconnects lazily on the next registry build, so a reload can never
/// drop the session — the worst case is an unchanged report.
pub fn reload_report(
    session: &pantheon_runtime::session::Session,
    dd: &std::path::Path,
) -> Vec<String> {
    let cfg = config::Config::load_or_report(dd);
    let declarations = read_mcp_declarations(dd);
    let mcfg =
        config::resolve_mcp_section(cfg.as_ref().and_then(|c| c.mcp.as_ref()), &declarations);
    session.set_mcp_config(mcfg);
    let mut lines = vec!["mcp: re-scanned config + declarations".to_string()];
    let health = session.mcp_manager.health();
    let mut names: Vec<&pantheon_mcp::manager::ServerHealth> = health.iter().collect();
    names.sort_by(|a, b| a.name.cmp(&b.name));
    let mut pending = 0usize;
    for h in names {
        let status = match h.status {
            ServerStatus::Ready => "ready",
            ServerStatus::Connecting => "connecting",
            ServerStatus::Failed => "failed",
            ServerStatus::Backoff => "backoff",
            ServerStatus::Unapproved => {
                pending += 1;
                "needs approval"
            }
            ServerStatus::Disabled => "disabled",
        };
        let mut line = format!("  {} [{status}]", h.name);
        if h.tools > 0 {
            line.push_str(&format!(" ({} tools)", h.tools));
        }
        if let Some(e) = &h.last_error {
            line.push_str(&format!(": {e}"));
        }
        lines.push(line);
    }
    if health.is_empty() {
        lines.push("  no MCP servers configured".to_string());
    }
    if pending > 0 {
        lines.push(format!(
            "{pending} server(s) need `pantheon mcp approve <name>` before they launch"
        ));
    }
    lines
}
