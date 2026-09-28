//! `pantheon mcp`: inspect the MCP servers a migration declared.
//!
//! Migration writes a declaration per source at `<data_dir>/mcp/<source>.json`.
//! Before this verb existed that file was inert — written, and read by nothing.
//!
//! This lists what was declared and whether it is *ready* to register, which is
//! a deliberately honest report: Pantheon still has no MCP server launcher
//! (spec section 15), so a declaration here is a prepared-but-unattached
//! server. The report says which of the two halves is true rather than
//! implying the servers are live.

use crate::terminal::data_dir;
use pantheon_migration::read_mcp_declarations;

/// Re-exported from `pantheon_migration` (single source of truth, shared
/// with the dashboard). Kept as a local name so existing callers inside
/// this crate do not churn.
pub use pantheon_migration::server_readiness;

/// Re-scan the MCP declaration files and report per-server readiness.
/// Pure file re-read: it cannot fail the session, and on any read problem
/// the previous state simply stands — the session is never lost to a
/// reload.
pub fn reload_report(dd: &std::path::Path) -> Vec<String> {
    let groups = read_mcp_declarations(dd);
    let mut lines = vec!["mcp: re-scanned declarations".to_string()];
    let mut total = 0usize;
    let mut ready = 0usize;
    for g in &groups {
        for s in &g.servers {
            total += 1;
            match server_readiness(s) {
                None => {
                    ready += 1;
                    lines.push(format!("  {}/{} [{}] ok", g.source, s.name, s.transport));
                }
                Some(blocker) => {
                    lines.push(format!(
                        "  {}/{} [{}] FAIL: {}",
                        g.source, s.name, s.transport, blocker
                    ));
                }
            }
        }
    }
    if total == 0 {
        lines.push("  no MCP declarations found".to_string());
    }
    lines.push(format!("{total} server(s), {ready} ready"));
    lines
}

/// One server's readiness, and why.
struct Row {
    source: String,
    name: String,
    transport: String,
    target: String,
    ready: bool,
    /// Empty when ready; otherwise the blocker.
    blocker: String,
}

pub fn cmd_mcp(args: &[String]) {
    let verb = args.first().map(|s| s.as_str()).unwrap_or("list");
    match verb {
        "list" | "" => list(args),
        other => {
            eprintln!("mcp: unknown verb '{other}'");
            eprintln!("  usage: pantheon mcp list [--json]");
            std::process::exit(2);
        }
    }
}

fn has_json(args: &[String]) -> bool {
    args.iter().any(|a| a == "--json")
}

fn collect(dd: &Path) -> (Vec<Row>, usize) {
    let groups = read_mcp_declarations(dd);
    let mut rows = Vec::new();
    let mut usable = 0usize;
    for g in &groups {
        for s in &g.servers {
            // Ready means: we know how to launch it, and nothing is missing
            // that only the operator can supply. A stdio server needs an
            // absolute command; an http/sse server needs a url.
            let (ready, blocker) = match s.transport.as_str() {
                "stdio" => match s.command.as_deref() {
                    Some(c) if !c.trim().is_empty() => (true, String::new()),
                    _ => (false, "no command declared".into()),
                },
                "http" | "sse" => match s.url.as_deref() {
                    Some(u) if !u.trim().is_empty() => (true, String::new()),
                    _ => (false, "no url declared".into()),
                },
                other => (false, format!("unsupported transport {other:?}")),
            };
            let ready = ready && !s.needs_credentials;
            let blocker = if s.needs_credentials && blocker.is_empty() {
                format!(
                    "needs a credential ({})",
                    if s.requires_env.is_empty() {
                        "value not in the source".to_string()
                    } else {
                        s.requires_env.join(", ")
                    }
                )
            } else {
                blocker
            };
            if ready {
                usable += 1;
            }
            let target = s
                .url
                .clone()
                .or_else(|| s.command.clone())
                .unwrap_or_else(|| "-".into());
            rows.push(Row {
                source: g.source.clone(),
                name: s.name.clone(),
                transport: s.transport.clone(),
                target,
                ready,
                blocker,
            });
        }
    }
    rows.sort_by(|a, b| {
        (a.source.as_str(), a.name.as_str()).cmp(&(b.source.as_str(), b.name.as_str()))
    });
    (rows, usable)
}

fn list(args: &[String]) {
    let dd = data_dir();
    let (rows, usable) = collect(&dd);
    if has_json(args) {
        let v: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "source": r.source, "name": r.name, "transport": r.transport,
                    "target": r.target, "ready": r.ready,
                    "blocker": if r.blocker.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(r.blocker.clone()) },
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
            "no MCP declarations under {} — run `pantheon migrate apply` to bring some over",
            dd.join("mcp").display()
        );
        return;
    }
    println!(
        "{:<10} {:<26} {:<7} {:<7} TARGET",
        "SOURCE", "NAME", "TRANS", "READY"
    );
    for r in &rows {
        println!(
            "{:<10} {:<26} {:<7} {:<7} {}{}",
            r.source,
            r.name,
            r.transport,
            if r.ready { "yes" } else { "no" },
            r.target,
            if r.blocker.is_empty() {
                String::new()
            } else {
                format!("  ({})", r.blocker)
            }
        );
    }
    println!();
    println!(
        "{} server(s) declared, {usable} ready to register",
        rows.len()
    );
    println!(
        "note: pantheon has no MCP launcher yet (spec section 15), so a 'ready' server is \
         prepared but not attached."
    );
}

use std::path::Path;
