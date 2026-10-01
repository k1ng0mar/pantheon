//! Remote plugin import + registry search against the dashboard
//! HTTP API.
//!
//! Plugin install/approve/disable already exist as local `pantheon
//! plugins ...` CLI verbs (`terminal.rs`) and as dashboard endpoints
//! (`pantheon-dashboard/src/plugins.rs`), but the interactive TUI had no
//! `/plugins` slash command at all — only the splash footer's hint named
//! it. This module adds it.
//!
//! The import + registry endpoints (`POST /api/plugins/import`,
//! `GET /api/plugins/registry/search`) are still in flight on the
//! dashboard side, so every call is built against the contracted shapes
//! and fails with a one-line "backend unreachable / not found" note
//! rather than a stack trace:
//!
//! - `POST /api/plugins/import` {url|spec, ref?} → 200
//!   {name, kind, version, detected_capabilities, scan_report:
//!   {verdict, findings}, approval_required: true}
//! - `GET /api/plugins/registry/search?q=&source=clawhub` → 200
//!   {results: [{slug, name, description, version, kind}]}
//!
//! Verdict values are the scanner's own words; the command renders
//! `clean` as a green verdict, `suspicious` as amber with its findings
//! listed, and `malicious` as red with approval blocked. Anything else
//! is reported verbatim so an unmodeled verdict cannot silently pass.
//!
//! HTTP plumbing mirrors `swarm_remote.rs` (dashboard `[server]`
//! host/port, `X-Pantheon-Token` from `PANTHEON_SERVE_TOKEN`, ureq 2.x
//! without its `json` feature → `send_string` +
//! `serde_json::from_str`). It is deliberately duplicated here rather
//! than imported: that module's helpers are private and this one must
//! not reshape another agent's in-flight work.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::session::TuiState;

/// Dashboard `/api/*` auth: the token header the serve listener enforces.
const TOKEN_HEADER: &str = "X-Pantheon-Token";

/// What went wrong with an API call: the server never answered, or it
/// answered with an error status.
enum ApiErr {
    Unreachable(String),
    Status(u16, String),
}

/// Dashboard address from `[server]`. `None` when no port can be
/// resolved (port 0 = auto-pick; the bound port is unknowable here).
fn server_base(data_dir: &Path) -> Option<String> {
    let cfg = crate::config::Config::load_or_report(data_dir);
    let server = cfg.as_ref().and_then(|c| c.server.clone());
    let host = server
        .as_ref()
        .map(|s| s.host.clone())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let port = server.as_ref().map(|s| s.port).unwrap_or(18789);
    if port == 0 {
        return None;
    }
    let host = if host.trim().is_empty() {
        "127.0.0.1".to_string()
    } else {
        host
    };
    Some(format!("http://{host}:{port}"))
}

fn data_dir() -> PathBuf {
    crate::terminal::data_dir()
}

/// Percent-encode a query or path fragment (plugin slugs are plain, so
/// this is defensive only).
fn encode(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    for b in seg.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// One JSON API call. `body = None` sends a GET, otherwise POST with a
/// JSON body. (ureq is built without its `json` feature here, so this
/// goes through `send_string` + `serde_json::from_str` like the rest of
/// the crate.)
fn request(
    method: &str,
    base: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value, ApiErr> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .build();
    let url = format!("{base}{path}");
    let mut req = match method {
        "POST" => agent.post(&url),
        _ => agent.get(&url),
    };
    if let Ok(tok) = std::env::var("PANTHEON_SERVE_TOKEN") {
        if !tok.is_empty() {
            req = req.set(TOKEN_HEADER, &tok);
        }
    }
    let resp = match body {
        Some(b) => req
            .set("Content-Type", "application/json")
            .send_string(&b.to_string()),
        None => req.call(),
    };
    match resp {
        Ok(r) => {
            let status = r.status();
            let text = r.into_string().unwrap_or_default();
            serde_json::from_str::<serde_json::Value>(&text)
                .map_err(|e| ApiErr::Status(status, format!("bad JSON: {e}")))
        }
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            let short: String = body.chars().take(200).collect();
            Err(ApiErr::Status(code, short))
        }
        Err(ureq::Error::Transport(t)) => Err(ApiErr::Unreachable(t.to_string())),
    }
}

/// Resolve the dashboard base URL or print why the command cannot run.
fn base_or_note(state: &mut TuiState, cmd: &str) -> Option<String> {
    match server_base(&data_dir()) {
        Some(b) => Some(b),
        None => {
            state.add_status(format!(
                "{cmd}: no dashboard address ([server] host/port unset, port 0 = auto)"
            ));
            None
        }
    }
}

/// Render an API failure as status lines. The plugin import/search routes
/// are still landing, so "unreachable"/"not found" name the likely cause
/// plainly instead of dumping a stack trace.
fn render_err(state: &mut TuiState, what: &str, err: ApiErr) {
    match err {
        ApiErr::Unreachable(t) => state.add_status(format!(
            "{what}: dashboard unreachable ({t}) — is `pantheon serve` running?"
        )),
        ApiErr::Status(404, _) => state.add_status(format!(
            "{what}: endpoint not found (404) — the dashboard plugin API has not landed yet"
        )),
        ApiErr::Status(code, body) => {
            state.add_status(format!("{what}: server error {code}: {body}"));
        }
    }
}

// ---------------------------------------------------------------------------
// /plugins import
// ---------------------------------------------------------------------------

const IMPORT_USAGE: &str = "usage: /plugins import <url|clawhub:slug> [--ref <ref>]";

/// Parsed `/plugins import` invocation.
#[derive(Debug, PartialEq)]
struct ImportArgs {
    /// The import target verbatim: a GitHub URL or `clawhub:<slug>`.
    /// The server accepts both in the `url` body field.
    source: String,
    git_ref: Option<String>,
}

fn parse_import(rest: &str) -> Result<ImportArgs, String> {
    let mut toks = rest.split_whitespace().peekable();
    let target = toks.next().ok_or(IMPORT_USAGE.to_string())?;
    let mut git_ref: Option<String> = None;
    while let Some(t) = toks.next() {
        let flag = t
            .strip_prefix("--")
            .ok_or(format!("expected a --flag, got {t:?} ({IMPORT_USAGE})"))?;
        let (key, inline) = match flag.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (flag, None),
        };
        match key {
            "ref" => {
                let v = inline
                    .map(str::to_string)
                    .or_else(|| toks.next().map(str::to_string))
                    .ok_or(format!("--ref needs a value ({IMPORT_USAGE})"))?;
                if v.trim().is_empty() {
                    return Err(format!("--ref needs a value ({IMPORT_USAGE})"));
                }
                git_ref = Some(v);
            }
            _ => return Err(format!("unknown flag --{key} ({IMPORT_USAGE})")),
        }
    }
    if target.trim().is_empty() {
        return Err(IMPORT_USAGE.to_string());
    }
    Ok(ImportArgs {
        source: target.to_string(),
        git_ref,
    })
}

/// Request body for `POST /api/plugins/import`: `{url, ref?}`.
///
/// The server resolves `clawhub:<slug>` itself
/// (`plugin_import::parse_source_spec`), so a slug goes in `url` exactly
/// like a GitHub URL — there is no `{spec}` field on the endpoint.
fn import_body(args: &ImportArgs) -> serde_json::Value {
    let mut body = serde_json::Map::new();
    body.insert(
        "url".to_string(),
        serde_json::Value::String(args.source.clone()),
    );
    if let Some(r) = &args.git_ref {
        body.insert("ref".to_string(), serde_json::Value::String(r.clone()));
    }
    serde_json::Value::Object(body)
}

/// One-line summary of a scan finding.
fn finding_text(f: &serde_json::Value) -> String {
    if let Some(s) = f.as_str() {
        return s.to_string();
    }
    let severity = f
        .get("severity")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let message = f
        .get("message")
        .or_else(|| f.get("title"))
        .or_else(|| f.get("description"))
        .and_then(|m| m.as_str())
        .unwrap_or("(no detail)")
        .to_string();
    if severity.is_empty() {
        message
    } else {
        format!("[{severity}] {message}")
    }
}

/// Render the import response as status lines. Pure, so the verdict
/// mapping is unit-testable: clean → green, suspicious → amber with
/// findings, malicious → red with approval blocked, anything else
/// reported verbatim (never silently treated as clean).
fn import_lines(v: &serde_json::Value) -> Vec<String> {
    let mut lines = Vec::new();
    let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("?");
    let version = v
        .get("version")
        .and_then(|n| n.as_str())
        .unwrap_or("?")
        .to_string();
    let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("?");
    lines.push(format!("imported {name} v{version} ({kind})"));
    match v.get("detected_capabilities").and_then(|c| c.as_array()) {
        Some(caps) if !caps.is_empty() => {
            let caps: Vec<String> = caps
                .iter()
                .filter_map(|c| c.as_str().map(str::to_string))
                .collect();
            lines.push(format!("capabilities: {}", caps.join(", ")));
        }
        _ => lines.push("capabilities: none detected".to_string()),
    }
    let empty = serde_json::Map::new();
    let report = v
        .get("scan_report")
        .and_then(|r| r.as_object())
        .unwrap_or(&empty);
    let verdict = report
        .get("verdict")
        .and_then(|x| x.as_str())
        .unwrap_or("unknown");
    match verdict {
        "clean" => {
            lines.push("scan: clean (green)".to_string());
        }
        "suspicious" => {
            lines.push("scan: SUSPICIOUS (amber) — review before approving".to_string());
            list_findings(report, &mut lines);
        }
        "malicious" => {
            lines.push("scan: MALICIOUS (red) — approval is blocked".to_string());
            list_findings(report, &mut lines);
            lines.push("this plugin is quarantined; it cannot be approved or run".to_string());
        }
        other => {
            lines.push(format!(
                "scan verdict: {other} (unrecognized — treat with care)"
            ));
            list_findings(report, &mut lines);
        }
    }
    if v.get("approval_required")
        .and_then(|a| a.as_bool())
        .unwrap_or(false)
        && verdict != "malicious"
    {
        lines.push(
            "approval required before it can run — use `pantheon plugins approve <name>`"
                .to_string(),
        );
    }
    lines
}

/// Append the scan report's findings, if any, to `lines`.
fn list_findings(report: &serde_json::Map<String, serde_json::Value>, lines: &mut Vec<String>) {
    let findings = report
        .get("findings")
        .and_then(|f| f.as_array())
        .cloned()
        .unwrap_or_default();
    if findings.is_empty() {
        return;
    }
    lines.push("findings:".to_string());
    for f in &findings {
        lines.push(format!("  - {}", finding_text(f)));
    }
}

fn do_import(state: &mut TuiState, rest: &str) {
    let args = match parse_import(rest) {
        Ok(a) => a,
        Err(e) => {
            state.add_status(format!("/plugins import: {e}"));
            return;
        }
    };
    let base = match base_or_note(state, "/plugins import") {
        Some(b) => b,
        None => return,
    };
    match request(
        "POST",
        &base,
        "/api/plugins/import",
        Some(import_body(&args)),
    ) {
        Ok(v) => {
            for line in import_lines(&v) {
                state.add_status(line);
            }
        }
        Err(e) => render_err(state, "/plugins import", e),
    }
}

// ---------------------------------------------------------------------------
// /plugins search
// ---------------------------------------------------------------------------

const SEARCH_USAGE: &str = "usage: /plugins search <query>";

/// Render registry search results as status lines. Pure, so the
/// formatting is unit-testable. The contract returns
/// `{results: [{slug, name, description, version, kind}]}`.
fn search_lines(results: &[serde_json::Value]) -> Vec<String> {
    if results.is_empty() {
        return vec!["no plugins matched".to_string()];
    }
    let mut lines = Vec::new();
    for (i, r) in results.iter().enumerate() {
        let slug = r.get("slug").and_then(|s| s.as_str()).unwrap_or("?");
        let name = r.get("name").and_then(|n| n.as_str()).unwrap_or("?");
        let version = r
            .get("version")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string();
        let desc: String = r
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(100)
            .collect();
        let desc = if desc.is_empty() {
            "—".to_string()
        } else {
            desc
        };
        lines.push(format!("{}. {slug} — {name} v{version}: {desc}", i + 1));
    }
    lines.push("install one with: /plugins import clawhub:<slug>".to_string());
    lines
}

fn do_search(state: &mut TuiState, rest: &str) {
    let query = rest.trim();
    if query.is_empty() {
        state.add_status(SEARCH_USAGE.to_string());
        return;
    }
    let base = match base_or_note(state, "/plugins search") {
        Some(b) => b,
        None => return,
    };
    let path = format!(
        "/api/plugins/registry/search?q={}&source=clawhub",
        encode(query)
    );
    match request("GET", &base, &path, None) {
        Ok(v) => {
            let results = v
                .get("results")
                .and_then(|r| r.as_array())
                .cloned()
                .unwrap_or_default();
            for line in search_lines(&results) {
                state.add_status(line);
            }
        }
        Err(e) => render_err(state, "/plugins search", e),
    }
}

/// `/plugins import <url|clawhub:slug> [--ref <ref>]` and
/// `/plugins search <query>`. Anything else prints usage. Quarantine
/// notes: the local `pantheon plugins list` shows approval state from
/// the on-disk manifests, which carry no scan verdict; the dashboard
/// `GET /api/plugins` list shape likewise has no verdict field — per
/// the task, no scan-verdict display was added there.
pub fn do_plugins(state: &mut TuiState, cmd: &str) {
    let rest = cmd.strip_prefix("/plugins").unwrap_or("").trim();
    if let Some(r) = rest.strip_prefix("import") {
        if r.is_empty() || r.starts_with(char::is_whitespace) {
            let r = r.trim();
            if r.is_empty() {
                state.add_status(format!("/plugins: {IMPORT_USAGE}"));
            } else {
                do_import(state, r);
            }
            return;
        }
    }
    if let Some(r) = rest.strip_prefix("search") {
        if r.is_empty() || r.starts_with(char::is_whitespace) {
            do_search(state, r.trim());
            return;
        }
    }
    state.add_status(
        "usage: /plugins import <url|clawhub:slug> [--ref <ref>] | /plugins search <query>"
            .to_string(),
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// P1 #3: `POST /api/plugins/import` reads only `{url}` — the server
    /// resolves `clawhub:<slug>` itself (`plugin_import::parse_source_spec`)
    /// — so the TUI must send the slug as `url`. Sending `{spec}` made the
    /// server 400 with "url is required".
    #[test]
    fn clawhub_import_body_uses_url_key() {
        let args = parse_import("clawhub:owner/my-plugin").expect("parse");
        let body = import_body(&args);
        assert_eq!(
            body.get("url").and_then(|v| v.as_str()),
            Some("clawhub:owner/my-plugin")
        );
        assert!(
            body.get("spec").is_none(),
            "the server has no {{spec}} field; sending it 400s"
        );
    }

    #[test]
    fn github_import_body_uses_url_key_with_ref() {
        let args = parse_import("https://github.com/owner/repo --ref v1").expect("parse");
        let body = import_body(&args);
        assert_eq!(
            body.get("url").and_then(|v| v.as_str()),
            Some("https://github.com/owner/repo")
        );
        assert_eq!(body.get("ref").and_then(|v| v.as_str()), Some("v1"));
        assert!(body.get("spec").is_none());
    }
}
