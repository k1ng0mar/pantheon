//! MCP server management over `<data_dir>/mcp/*.json`.
//!
//! The declaration files are the real store
//! ([`read_mcp_declarations`](pantheon_migration::read_mcp_declarations));
//! readiness is the shared
//! [`server_readiness`](pantheon_migration::server_readiness). Honest
//! scope, carried through to the UI: Pantheon has no MCP launcher, so
//! "ready" means prepared-but-unattached, and "reload" re-scans the
//! files on disk — there are no live clients to reconnect.

use crate::server::{Request, Response};
use crate::{bad_json, body_json, err_json, json_ok, App};
use pantheon_migration::{read_mcp_declarations, server_readiness, McpServer};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn mcp_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("mcp")
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

fn server_json(s: &McpServer) -> serde_json::Value {
    serde_json::json!({
        "name": s.name,
        "transport": s.transport,
        "command": s.command,
        "args": s.args,
        "url": s.url,
        "requires_env": s.requires_env,
        "needs_credentials": s.needs_credentials,
        "enabled": s.enabled,
        "readiness": server_readiness(s),
    })
}

/// `GET /api/mcp`
pub fn list(app: &App) -> Response {
    let decls = read_mcp_declarations(&app.data_dir);
    let out: Vec<serde_json::Value> = decls
        .iter()
        .map(|d| {
            serde_json::json!({
                "source": d.source,
                "servers": d.servers.iter().map(server_json).collect::<Vec<_>>(),
            })
        })
        .collect();
    json_ok(serde_json::json!({
        "servers": out,
        "note": "Pantheon has no MCP launcher: ready means prepared-but-unattached",
    }))
}

/// Rewrite one declaration file with `f` applied to its servers.
fn rewrite(
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

fn parse_new_server(body: &serde_json::Value) -> Result<McpServer, String> {
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "name is required".to_string())?;
    if name.len() > 64 || name.contains('/') || name.contains('\\') || name.contains("..") {
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
    match transport {
        "stdio" => {
            if command.is_none() {
                return Err("stdio servers need a command".to_string());
            }
        }
        _ => {
            if url.is_none() {
                return Err("http/sse servers need a url".to_string());
            }
        }
    }
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
    Ok(McpServer {
        name: name.to_string(),
        transport: transport.to_string(),
        command,
        args,
        url,
        requires_env,
        needs_credentials: false,
        enabled: true,
    })
}

/// `POST /api/mcp` — `{source?, name, transport, command?, args?, url?,
/// requires_env?, confirm: true}`.
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
    let source = body
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("pantheon");
    let server = match parse_new_server(&body) {
        Ok(s) => s,
        Err(e) => return bad_json(&e),
    };
    let name = server.name.clone();
    rewrite(app, source, |servers| {
        if servers.iter().any(|s| s.name == name) {
            return Err(format!(
                "a server named '{name}' already exists in '{source}'"
            ));
        }
        servers.push(server);
        Ok(())
    })
}

/// `POST /api/mcp/servers/:name/enable` (or `/disable`).
pub fn set_enabled(app: &App, name: &str, enabled: bool) -> Response {
    // The server may live in any declaration file; walk them all.
    let decls = read_mcp_declarations(&app.data_dir);
    let mut touched: Option<String> = None;
    for d in &decls {
        if d.servers.iter().any(|s| s.name == name) {
            touched = Some(d.source.clone());
            break;
        }
    }
    match touched {
        Some(source) => rewrite(app, &source, |servers| {
            match servers.iter_mut().find(|s| s.name == name) {
                Some(s) => {
                    s.enabled = enabled;
                    Ok(())
                }
                None => Err("server vanished".to_string()),
            }
        }),
        None => err_json(404, "NOT_FOUND", "no server by that name"),
    }
}

/// `DELETE /api/mcp/:name`.
pub fn delete(app: &App, name: &str) -> Response {
    let decls = read_mcp_declarations(&app.data_dir);
    let mut touched: Option<String> = None;
    for d in &decls {
        if d.servers.iter().any(|s| s.name == name) {
            touched = Some(d.source.clone());
            break;
        }
    }
    match touched {
        Some(source) => rewrite(app, &source, |servers| {
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

/// `POST /api/mcp/reload` — re-scan the declaration files from disk.
pub fn reload(app: &App) -> Response {
    list(app)
}

/// `POST /api/mcp/:name/test` — a real probe: for stdio, resolve the
/// command on PATH (or as an absolute path); for http/sse, attempt a
/// TCP connect to the URL's host:port with a 3s timeout. The readiness
/// blocker, if any, is reported alongside.
pub fn test(app: &App, name: &str) -> Response {
    let decls = read_mcp_declarations(&app.data_dir);
    let server = decls
        .iter()
        .flat_map(|d| &d.servers)
        .find(|s| s.name == name);
    let s = match server {
        Some(s) => s,
        None => return err_json(404, "NOT_FOUND", "no server by that name"),
    };
    if let Some(blocker) = server_readiness(s) {
        return json_ok(serde_json::json!({
            "ok": false, "name": name,
            "detail": format!("not ready: {blocker}"),
        }));
    }
    let (ok, detail) = match s.transport.as_str() {
        "stdio" => {
            let cmd = s.command.as_deref().unwrap_or("");
            (resolve_command(cmd).is_some(), format!("command {cmd:?}"))
        }
        _ => match s.url.as_deref() {
            Some(url) => probe_url(url),
            None => (false, "no url".to_string()),
        },
    };
    json_ok(serde_json::json!({"ok": ok, "name": name, "detail": detail}))
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
