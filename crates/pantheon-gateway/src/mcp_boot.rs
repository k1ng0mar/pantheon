//! MCP health check at gateway boot (F-8).
//!
//! A dead MCP server used to surface only as a mid-turn tool error. This
//! module handshakes every enabled, consented MCP server once at startup
//! with a timeout and logs the outcome, so a broken server is a startup
//! warning instead of a surprise mid-conversation.
//!
//! The smaller surface was picked: log lines at startup. The dashboard's
//! `GET /api/mcp/health` (the session launcher's live snapshot) stays the
//! live view; dashboard surfacing of this boot report is another leaf's job.
//!
//! Hook (one line, in the gateway's boot path - `pantheon gateway run`):
//! ```ignore
//! pantheon_gateway::mcp_boot::check_and_log(&data_dir, std::time::Duration::from_secs(10));
//! ```
//!
//! Safety: only servers the operator already consented to are probed
//! bundled (first-party) servers and servers with an approval record. An
//! unapproved third-party server is never spawned by a health check; it is
//! reported as waiting for approval.

use pantheon_mcp::client::{McpClient, McpServerConfig};
use pantheon_mcp::manager::{McpServerSpec, McpTransport};
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

/// One server's boot handshake outcome.
#[derive(Debug, Clone)]
pub struct McpBootHealth {
    pub name: String,
    pub transport: &'static str,
    pub target: String,
    pub ok: bool,
    /// Human detail: negotiated version on success, the failure on error.
    pub detail: String,
}

/// Configured `[mcp.servers.<name>]` entries, enabled ones only.
fn config_specs(data_dir: &Path) -> Vec<McpServerSpec> {
    let raw = std::fs::read_to_string(data_dir.join("config.toml")).unwrap_or_default();
    let cfg: Result<pantheon_api::config::Config, _> = toml::from_str(&raw);
    let servers = cfg
        .map(|c| {
            c.mcp
                .map(|m| m.servers.into_iter().collect::<Vec<_>>())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    servers
        .into_iter()
        .filter(|(_, e)| e.enabled)
        .filter_map(|(name, e)| McpServerSpec::from_entry(&name, &e))
        .collect()
}

/// Resolve `env:NAME` secret refs against the process environment (the
/// manager's resolver falls back to the same). Returns the resolved env
/// or the first missing variable name.
fn resolve_env(env: &HashMap<String, String>) -> Result<HashMap<String, String>, String> {
    let mut out = HashMap::new();
    for (k, v) in env {
        match v.strip_prefix("env:") {
            Some(name) => match std::env::var(name) {
                Ok(val) => {
                    out.insert(k.clone(), val);
                }
                Err(_) => return Err(name.to_string()),
            },
            None => {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    Ok(out)
}

/// Split an http(s) URL into (host, port) for the TCP reachability probe.
fn url_host_port(url: &str) -> Option<(String, u16)> {
    let after_scheme = url.split("://").nth(1)?;
    let host_port = after_scheme.split('/').next()?;
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().ok()?),
        None => {
            let default = if url.starts_with("https") { 443 } else { 80 };
            (host_port.to_string(), default)
        }
    };
    if host.is_empty() {
        return None;
    }
    Some((host, port))
}

fn probe_stdio(spec: &McpServerSpec, timeout: Duration) -> McpBootHealth {
    let base = McpBootHealth {
        name: spec.name.clone(),
        transport: spec.transport.as_str(),
        target: spec.target(),
        ok: false,
        detail: String::new(),
    };
    let command = match &spec.command {
        Some(c) => c.clone(),
        None => {
            return McpBootHealth {
                detail: "stdio server has no command".to_string(),
                ..base
            }
        }
    };
    let env = match resolve_env(&spec.env) {
        Ok(e) => e,
        Err(missing) => {
            return McpBootHealth {
                detail: format!("secret env {missing} is not set"),
                ..base
            }
        }
    };
    let cfg = McpServerConfig::new(&spec.name, command)
        .with_args(spec.args.clone())
        .with_env(env)
        .with_timeout(timeout);
    // `None` policy: the manager gates spawns through the capability
    // policy at session time; the boot probe mirrors the pre-policy
    // ungated spawn for servers the operator already approved.
    match McpClient::connect(&cfg, None) {
        Ok(client) => {
            let version = if client.negotiated_version.is_empty() {
                "handshake ok".to_string()
            } else {
                format!("handshake ok (protocol {})", client.negotiated_version)
            };
            McpBootHealth {
                ok: true,
                detail: version,
                ..base
            }
        }
        Err(e) => McpBootHealth {
            detail: format!("{e:?}"),
            ..base
        },
    }
}

fn probe_remote(spec: &McpServerSpec) -> McpBootHealth {
    let base = McpBootHealth {
        name: spec.name.clone(),
        transport: spec.transport.as_str(),
        target: spec.target(),
        ok: false,
        detail: String::new(),
    };
    let url = match &spec.url {
        Some(u) => u.clone(),
        None => {
            return McpBootHealth {
                detail: format!("{} server has no url", spec.transport.as_str()),
                ..base
            }
        }
    };
    // A full MCP HTTP handshake at boot is overkill; TCP reachability
    // proves the server is alive. The session-time manager does the real
    // handshake before first use.
    match url_host_port(&url).and_then(|(h, p)| {
        std::net::TcpStream::connect_timeout(
            &format!("{h}:{p}").parse().ok()?,
            Duration::from_secs(5),
        )
        .ok()
        .map(|_| (h, p))
    }) {
        Some(_) => McpBootHealth {
            ok: true,
            detail: "tcp reachable (handshake deferred to first use)".to_string(),
            ..base
        },
        None => McpBootHealth {
            detail: format!("cannot reach {url}"),
            ..base
        },
    }
}

/// Handshake every enabled, consented MCP server with a timeout.
/// `per_server_timeout` caps the stdio handshake (the config's own
/// `timeout_secs` still applies below the cap).
pub fn check(data_dir: &Path, per_server_timeout: Duration) -> Vec<McpBootHealth> {
    let approved = pantheon_api::approval::ApprovalStore::open(&data_dir.join("mcp")).names();
    let mut out = Vec::new();
    for spec in config_specs(data_dir) {
        // Consent gate: first-party bundled servers probe freely; anything
        // else needs an approval record. Never spawn a third-party server
        // the operator has not approved.
        let consented = pantheon_mcp::bundled::is_bundled(&spec.name)
            || approved.iter().any(|a| a == &spec.name);
        if !consented {
            out.push(McpBootHealth {
                name: spec.name.clone(),
                transport: spec.transport.as_str(),
                target: spec.target(),
                ok: false,
                detail: "not approved - skipped at boot (approve in the dashboard)".to_string(),
            });
            continue;
        }
        let timeout = spec
            .timeout
            .min(per_server_timeout)
            .max(Duration::from_secs(1));
        let health = match spec.transport {
            McpTransport::Stdio => probe_stdio(&spec, timeout),
            McpTransport::Sse | McpTransport::Http => probe_remote(&spec),
        };
        out.push(health);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Run [`check`] and log one line per server, plus a summary. Returns the
/// results for callers that want them. Never panics, never exits: a broken
/// MCP server must not take the gateway down with it.
pub fn check_and_log(data_dir: &Path, per_server_timeout: Duration) -> Vec<McpBootHealth> {
    let results = check(data_dir, per_server_timeout);
    if results.is_empty() {
        eprintln!("mcp boot: no enabled MCP servers configured");
        return results;
    }
    let mut bad = 0;
    for r in &results {
        if r.ok {
            eprintln!("mcp boot: {} ok ({})", r.name, r.detail);
        } else {
            bad += 1;
            eprintln!("mcp boot: {} UNHEALTHY: {}", r.name, r.detail);
        }
    }
    if bad > 0 {
        eprintln!(
            "mcp boot: {bad} of {} servers unhealthy - they will fail at tool-call time; fix or disable them in [mcp.servers]",
            results.len()
        );
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_host_port_parses() {
        assert_eq!(
            url_host_port("https://example.com/mcp"),
            Some(("example.com".to_string(), 443))
        );
        assert_eq!(
            url_host_port("http://localhost:8000/sse"),
            Some(("localhost".to_string(), 8000))
        );
        assert_eq!(url_host_port("not a url"), None);
    }

    #[test]
    fn resolve_env_missing_reports_name() {
        let mut env = HashMap::new();
        env.insert(
            "TOKEN".to_string(),
            "env:PANTHEON_TEST_DEFINITELY_UNSET".to_string(),
        );
        let err = resolve_env(&env).unwrap_err();
        assert_eq!(err, "PANTHEON_TEST_DEFINITELY_UNSET");
    }

    #[test]
    fn check_empty_config_is_empty() {
        let d = std::env::temp_dir().join("pantheon-mcp-boot-test-empty");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("config.toml"), "[mcp]\n").unwrap();
        let out = check(&d, Duration::from_secs(2));
        assert!(out.is_empty());
    }

    #[test]
    fn check_reports_broken_and_unapproved() {
        let d = std::env::temp_dir().join("pantheon-mcp-boot-test-probe");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        // "broken" is bundled-approved? No - use a name the bundled check
        // will not know, but approve it via the approval store dir so the
        // probe actually runs and fails on the bogus command.
        std::fs::write(
            d.join("config.toml"),
            "[mcp.servers.broken]\nenabled = true\ncommand = \"pantheon-definitely-not-a-real-binary-xyz\"\n",
        )
        .unwrap();
        let mcp_dir = d.join("mcp");
        std::fs::create_dir_all(&mcp_dir).unwrap();
        std::fs::write(
            mcp_dir.join(".approvals.json"),
            r#"{"broken":{"plugin":"broken","version":"1","dir_hash":"x","approved_at_ms":1}}"#,
        )
        .unwrap();
        let out = check(&d, Duration::from_secs(5));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "broken");
        assert_eq!(out[0].transport, "stdio");
        assert!(!out[0].ok, "bogus command should not handshake");
        assert!(!out[0].detail.is_empty());
    }

    #[test]
    fn check_skips_unapproved_third_party() {
        let d = std::env::temp_dir().join("pantheon-mcp-boot-test-unapproved");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("config.toml"),
            "[mcp.servers.sneaky]\nenabled = true\ncommand = \"echo\"\n",
        )
        .unwrap();
        // No approval record: the server must never be spawned.
        let out = check(&d, Duration::from_secs(5));
        assert_eq!(out.len(), 1);
        assert!(!out[0].ok);
        assert!(out[0].detail.contains("not approved"));
    }
}
