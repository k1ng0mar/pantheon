//! Remote swarm controls + persona-file commands against the dashboard
//! HTTP API.
//!
//! The swarm backend (`POST /api/swarm`, `GET /api/swarm/status`,
//! `GET /api/swarm/transcript`, `POST /api/swarm/<id>/retry`) and the
//! persona-file endpoints (`GET/PUT /api/profiles/:name/files`) live in
//! the dashboard server (`pantheon serve` / `pantheon dashboard`), not in
//! the TUI process. This module reaches them over HTTP at the `[server]`
//! host/port the serve surface binds (default 127.0.0.1:18789), with the
//! `X-Pantheon-Token` header from `PANTHEON_SERVE_TOKEN` when set.
//!
//! The swarm routes have not landed yet (the swarm backend is in flight),
//! so every swarm call fails with a one-line "backend unreachable" note
//! rather than a stack trace. The TUI builds strictly against the API
//! contract the mobile client documents (see
//! `~/workspace/pantheon-mobile/lib/services/pantheon_api.dart`):
//!
//! - `POST /api/swarm` {task, mode: "count"|"profiles", subagent_count?,
//!   profiles?, judge} → 201 {swarm_id, run_id, agents:[...]}
//! - `GET /api/swarm/status?swarm=<id>` → {task, status, round,
//!   agents:[{name, status}...], verdict: {done, notes}|null}
//! - `GET /api/swarm/transcript?swarm=<id>&agent=<name>` → {transcript}
//! - `POST /api/swarm/<id>/retry` {feedback?} → 200 {swarm_id, round}
//!
//! Contract note: there is no list-all-swarms endpoint, so bare `/swarm`
//! reports only the swarms this TUI session spawned (`state.remote_swarms`).
//!
//! Persona-file commands additionally fall back to direct file
//! read/writes against the profile's declared paths when the server is
//! not running, so `/soul` works in a plain TUI session too. The same
//! safety rule applies on both paths: only declared paths are ever
//! touched, and content is capped at 256 KiB.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::session::TuiState;
use pantheon_runtime::session::Session;

/// Persona-file content cap: 256 KiB, mirroring the dashboard endpoint.
const MAX_FILE_BYTES: usize = 256 * 1024;
/// Lines of a persona file printed before truncation.
const MAX_PRINT_LINES: usize = 80;

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

/// Percent-encode a path segment (profile names and swarm ids are slugs,
/// so this is defensive only).
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

/// One JSON API call. `body = None` sends a GET, otherwise POST/PUT with
/// a JSON body. (ureq is built without its `json` feature here, so this
/// goes through `send_string` + `serde_json::from_str` like the rest of
/// the crate.)
fn request(
    method: &str,
    base: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value, ApiErr> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(10))
        .build();
    let url = format!("{base}{path}");
    let mut req = match method {
        "POST" => agent.post(&url),
        "PUT" => agent.put(&url),
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

/// True when the dashboard API answers at all (any status, even 404).
/// A transport failure means the server is not there.
fn api_reachable(base: &str) -> bool {
    !matches!(
        request("GET", base, "/api/swarm/status?swarm=__probe__", None),
        Err(ApiErr::Unreachable(_))
    )
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

/// Render an API failure as one status line. The swarm routes have not
/// landed yet, so "unreachable" names the likely cause plainly.
fn render_err(state: &mut TuiState, what: &str, backend: &str, err: ApiErr) {
    match err {
        ApiErr::Unreachable(t) => state.add_status(format!(
            "{what}: {backend} unreachable ({t}) - is `pantheon serve` running?"
        )),
        ApiErr::Status(404, _) => state.add_status(format!("{what}: not found (404)")),
        ApiErr::Status(code, body) => {
            state.add_status(format!("{what}: server error {code}: {body}"));
        }
    }
}

// ---------------------------------------------------------------------------
// /swarm
// ---------------------------------------------------------------------------

/// `POST /api/swarm` response we care about.
fn agent_names(v: &serde_json::Value) -> Vec<String> {
    v.get("agents")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    a.as_str()
                        .map(str::to_string)
                        .or_else(|| a.get("name").and_then(|n| n.as_str()).map(str::to_string))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Parsed `/swarm new` invocation.
#[derive(Debug, PartialEq)]
struct SpawnArgs {
    task: String,
    mode: String,
    count: Option<u32>,
    profiles: Option<Vec<String>>,
    judge: bool,
}

fn parse_spawn(rest: &str) -> Result<SpawnArgs, String> {
    const USAGE: &str =
        "usage: /swarm new <task> [--count N | --profiles a,b] [--judge|--no-judge]";
    let mut task_words: Vec<&str> = Vec::new();
    let mut count: Option<u32> = None;
    let mut profiles: Option<Vec<String>> = None;
    let mut judge = false;
    let mut flags_started = false;
    let mut toks = rest.split_whitespace().peekable();
    while let Some(t) = toks.next() {
        if let Some(flag) = t.strip_prefix("--") {
            flags_started = true;
            let (key, inline) = match flag.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (flag, None),
            };
            match key {
                "count" => {
                    let v = inline
                        .map(str::to_string)
                        .or_else(|| toks.next().map(str::to_string))
                        .ok_or("--count needs a number")?;
                    count = Some(
                        v.parse::<u32>()
                            .map_err(|_| "--count needs a positive number")?,
                    );
                    if count == Some(0) {
                        return Err("--count needs a positive number".into());
                    }
                }
                "profiles" => {
                    let v = inline
                        .map(str::to_string)
                        .or_else(|| toks.next().map(str::to_string))
                        .ok_or("--profiles needs a comma-separated list")?;
                    let list: Vec<String> = v
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect();
                    if list.is_empty() {
                        return Err("--profiles needs a comma-separated list".into());
                    }
                    profiles = Some(list);
                }
                "judge" => judge = true,
                "no-judge" => judge = false,
                _ => return Err(format!("unknown flag --{key} ({USAGE})")),
            }
        } else if flags_started {
            return Err(format!("task text must come before flags ({USAGE})"));
        } else {
            task_words.push(t);
        }
    }
    let task = task_words.join(" ");
    if task.is_empty() {
        return Err(USAGE.into());
    }
    if count.is_some() && profiles.is_some() {
        return Err("pick --count or --profiles, not both".into());
    }
    let mode = if profiles.is_some() {
        "profiles"
    } else {
        "count"
    };
    Ok(SpawnArgs {
        task,
        mode: mode.to_string(),
        count,
        profiles,
        judge,
    })
}

fn spawn_swarm(state: &mut TuiState, rest: &str) {
    let args = match parse_spawn(rest) {
        Ok(a) => a,
        Err(e) => {
            state.add_status(format!("/swarm new: {e}"));
            return;
        }
    };
    let base = match base_or_note(state, "/swarm new") {
        Some(b) => b,
        None => return,
    };
    let mut body = serde_json::json!({
        "task": args.task,
        "mode": args.mode,
        "judge": args.judge,
    });
    if let Some(n) = args.count {
        body["subagent_count"] = serde_json::json!(n);
    }
    if let Some(p) = &args.profiles {
        body["profiles"] = serde_json::json!(p);
    }
    match request("POST", &base, "/api/swarm", Some(body)) {
        Ok(v) => {
            let id = v
                .get("swarm_id")
                .and_then(|s| s.as_str())
                .unwrap_or("?")
                .to_string();
            let names = agent_names(&v);
            if !state.remote_swarms.contains(&id) {
                state.remote_swarms.push(id.clone());
            }
            let who = if names.is_empty() {
                String::new()
            } else {
                format!(" ({})", names.join(", "))
            };
            state.add_status(format!("swarm {id} started{who} - /swarm {id} to watch"));
        }
        Err(e) => render_err(state, "/swarm new", "swarm backend", e),
    }
}

/// One-line summary of a swarm's status for the list view.
fn status_line(id: &str, v: &serde_json::Value) -> String {
    let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("?");
    let round = v.get("round").and_then(|r| r.as_u64()).unwrap_or(0);
    let agents = v
        .get("agents")
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default();
    let done = agents
        .iter()
        .filter(|a| a.get("status").and_then(|s| s.as_str()) == Some("done"))
        .count();
    let task: String = v
        .get("task")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .chars()
        .take(60)
        .collect();
    format!(
        "{id} · {status} · round {round} · {done}/{} agents · {task}",
        agents.len()
    )
}

fn fetch_status(base: &str, id: &str) -> Result<serde_json::Value, ApiErr> {
    request(
        "GET",
        base,
        &format!("/api/swarm/status?swarm={}", encode(id)),
        None,
    )
}

fn list_swarms(state: &mut TuiState) {
    let base = match base_or_note(state, "/swarm") {
        Some(b) => b,
        None => return,
    };
    if state.remote_swarms.is_empty() {
        state.add_status(
            "no swarms spawned this session (the swarm API has no list-all endpoint yet)".into(),
        );
        return;
    }
    let ids = state.remote_swarms.clone();
    for id in &ids {
        match fetch_status(&base, id) {
            Ok(v) => state.add_status(status_line(id, &v)),
            Err(e) => {
                render_err(state, &format!("/swarm {id}"), "swarm backend", e);
                return;
            }
        }
    }
}

fn show_swarm(state: &mut TuiState, id: &str) {
    let base = match base_or_note(state, &format!("/swarm {id}")) {
        Some(b) => b,
        None => return,
    };
    match fetch_status(&base, id) {
        Ok(v) => {
            let task = v.get("task").and_then(|t| t.as_str()).unwrap_or("");
            let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("?");
            let round = v.get("round").and_then(|r| r.as_u64()).unwrap_or(0);
            state.add_status(format!("swarm {id}: {task}"));
            state.add_status(format!("status: {status} · round {round}"));
            let agents = v
                .get("agents")
                .and_then(|a| a.as_array())
                .cloned()
                .unwrap_or_default();
            if agents.is_empty() {
                state.add_status("agents: none yet".into());
            } else {
                state.add_status("agents:".into());
                for a in &agents {
                    let name = a.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                    let st = a.get("status").and_then(|s| s.as_str()).unwrap_or("?");
                    state.add_status(format!("  {name} - {st}"));
                }
            }
            match v.get("verdict") {
                Some(verdict) if !verdict.is_null() => {
                    let done = verdict
                        .get("done")
                        .and_then(|d| d.as_bool())
                        .unwrap_or(false);
                    let notes = verdict.get("notes").and_then(|n| n.as_str()).unwrap_or("");
                    state.add_status(format!("verdict: done={done} - {notes}"));
                }
                _ => state.add_status("verdict: none yet".into()),
            }
        }
        Err(e) => render_err(state, &format!("/swarm {id}"), "swarm backend", e),
    }
}

fn retry_swarm(state: &mut TuiState, rest: &str) {
    let mut parts = rest.splitn(2, char::is_whitespace);
    let id = parts.next().unwrap_or("").trim();
    let feedback = parts.next().map(str::trim).filter(|s| !s.is_empty());
    if id.is_empty() {
        state.add_status("usage: /swarm retry <id> [feedback]".into());
        return;
    }
    let base = match base_or_note(state, "/swarm retry") {
        Some(b) => b,
        None => return,
    };
    let mut body = serde_json::Map::new();
    if let Some(f) = feedback {
        body.insert(
            "feedback".to_string(),
            serde_json::Value::String(f.to_string()),
        );
    }
    match request(
        "POST",
        &base,
        &format!("/api/swarm/{}/retry", encode(id)),
        Some(serde_json::Value::Object(body)),
    ) {
        Ok(v) => {
            let round = v.get("round").and_then(|r| r.as_u64()).unwrap_or(0);
            state.add_status(format!("swarm {id} relaunched → round {round}"));
        }
        Err(e) => render_err(state, &format!("/swarm retry {id}"), "swarm backend", e),
    }
}

/// `/swarm` and its subcommands. `/swarm tree` keeps the old read-only
/// delegation-tree render; everything else is the remote swarm backend.
pub fn do_swarm(state: &mut TuiState, session: &Arc<Session>, cmd: &str) {
    let rest = cmd.strip_prefix("/swarm").unwrap_or("").trim();
    if rest == "tree" {
        crate::swarm_view::cmd_swarm(state, session);
        return;
    }
    if rest.is_empty() {
        list_swarms(state);
        return;
    }
    if rest == "new" || rest.starts_with("new ") {
        spawn_swarm(state, rest.strip_prefix("new").unwrap_or("").trim());
        return;
    }
    if rest == "retry" || rest.starts_with("retry ") {
        retry_swarm(state, rest.strip_prefix("retry").unwrap_or("").trim());
        return;
    }
    show_swarm(state, rest.split_whitespace().next().unwrap_or(rest));
}

// ---------------------------------------------------------------------------
// /collab (deprecated in favor of /swarm)
// ---------------------------------------------------------------------------

/// One-line deprecation note. Stored collaboration data is untouched
/// nothing here deletes from `collaboration.db`.
pub const COLLAB_DEPRECATION: &str =
    "/collab is deprecated - multi-profile tasks moved to the swarm system; use /swarm (see the Swarm page). Stored collaboration data is untouched.";

pub fn do_collab(state: &mut TuiState) {
    match server_base(&data_dir()) {
        Some(base) if api_reachable(&base) => {
            // The swarm backend answers: route through it.
            list_swarms_via(state, &base);
            state.add_status(COLLAB_DEPRECATION.into());
        }
        _ => {
            state.add_status(COLLAB_DEPRECATION.into());
        }
    }
}

fn list_swarms_via(state: &mut TuiState, base: &str) {
    if state.remote_swarms.is_empty() {
        state.add_status(
            "no swarms spawned this session (the swarm API has no list-all endpoint yet)".into(),
        );
        return;
    }
    let ids = state.remote_swarms.clone();
    for id in &ids {
        match fetch_status(base, id) {
            Ok(v) => state.add_status(status_line(id, &v)),
            Err(e) => {
                render_err(state, &format!("/swarm {id}"), "swarm backend", e);
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// /soul, /userfile, /agentsfile - persona files of the active profile
// ---------------------------------------------------------------------------

/// Persona-file slot: the API discriminator plus the config field and
/// the default filename the server creates when the profile declares no
/// path (mirrored here for the direct-file fallback).
fn file_slot(kind: &str) -> Option<(&'static str, &'static str)> {
    match kind {
        "soul" => Some(("soul_file", "SOUL.md")),
        "user" => Some(("user_file", "USER.md")),
        "agents" => Some(("agents_file", "AGENTS.md")),
        _ => None,
    }
}

/// The active profile name: the runtime's attached agent first, then the
/// TUI's cached variant (set at startup and by `/agent <name>`).
fn active_profile(state: &TuiState, session: &Arc<Session>) -> Option<String> {
    if let Some(a) = session.agent() {
        let name = a.profile().name.clone();
        if !name.is_empty() {
            return Some(name);
        }
    }
    let v = state.variant.trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

/// Declared persona-file paths for a profile: the *own* `[agents.<name>]`
/// table values, exactly what the dashboard `GET /api/profiles/:name/files`
/// endpoint reads - not the resolved inheritance chain. Keeping the two
/// paths identical means `/soul` prints the same thing whether the
/// server is running or the TUI reads the file directly.
fn declared_paths(name: &str) -> Result<BTreeMap<String, Option<String>>, String> {
    let cfg = crate::config::Config::load(&data_dir()).map_err(|e| e.cause)?;
    let table = cfg
        .agents
        .get(name)
        .ok_or_else(|| format!("no [agents.{name}] profile declared"))?;
    let pick = |v: &Option<String>| v.clone().filter(|s| !s.trim().is_empty());
    let mut out = BTreeMap::new();
    out.insert("soul".to_string(), pick(&table.soul_file));
    out.insert("user".to_string(), pick(&table.user_file));
    out.insert("agents".to_string(), pick(&table.agents_file));
    Ok(out)
}

fn print_file(state: &mut TuiState, label: &str, path: Option<&str>, content: &str) {
    let n = content.chars().count();
    state.add_status(format!(
        "{label} ({} · {n} chars):",
        path.unwrap_or("not declared")
    ));
    let lines: Vec<&str> = content.lines().collect();
    for line in lines.iter().take(MAX_PRINT_LINES) {
        state.add_status((*line).to_string());
    }
    if lines.len() > MAX_PRINT_LINES {
        state.add_status(format!(
            "... ({} more lines)",
            lines.len() - MAX_PRINT_LINES
        ));
    }
}

/// Read a persona file: HTTP first, direct declared-path read when the
/// server is not running.
fn read_persona(state: &mut TuiState, session: &Arc<Session>, kind: &str, label: &str) {
    let (field, _default_name) = file_slot(kind).unwrap();
    let name = match active_profile(state, session) {
        Some(n) => n,
        None => {
            state.add_status(format!("/{label}: no active agent profile"));
            return;
        }
    };
    // HTTP path.
    if let Some(base) = server_base(&data_dir()) {
        match request(
            "GET",
            &base,
            &format!("/api/profiles/{}/files", encode(&name)),
            None,
        ) {
            Ok(v) => {
                let empty = serde_json::Map::new();
                let slot = v.get(kind).and_then(|s| s.as_object()).unwrap_or(&empty);
                let path = slot.get("path").and_then(|p| p.as_str());
                let content = slot.get("content").and_then(|c| c.as_str()).unwrap_or("");
                print_file(state, label, path, content);
                return;
            }
            Err(ApiErr::Unreachable(_)) => {} // fall through to direct read
            Err(e) => {
                render_err(state, &format!("/{label}"), "dashboard", e);
                return;
            }
        }
    }
    // Direct-file fallback: only declared paths, never a guess.
    match declared_paths(&name) {
        Ok(paths) => match paths.get(kind).and_then(|p| p.as_deref()) {
            Some(p) => match std::fs::read_to_string(p) {
                Ok(c) => print_file(state, label, Some(p), &c),
                Err(e) => state.add_status(format!("/{label}: read {p}: {e}")),
            },
            None => state.add_status(format!("/{label}: no {field} declared for profile {name}")),
        },
        Err(e) => state.add_status(format!("/{label}: {e}")),
    }
}

/// Write a persona file: HTTP first, direct declared-path write when the
/// server is not running. When the profile declares no path, the file is
/// created under `<data_dir>/profiles/<name>/` and the profile field is
/// pointed at it - the same semantics as the dashboard endpoint.
fn write_persona(
    state: &mut TuiState,
    session: &Arc<Session>,
    kind: &str,
    label: &str,
    content: &str,
) {
    if content.len() > MAX_FILE_BYTES {
        state.add_status(format!("/{label}: content exceeds the 256 KiB cap"));
        return;
    }
    let (field, default_name) = file_slot(kind).unwrap();
    let name = match active_profile(state, session) {
        Some(n) => n,
        None => {
            state.add_status(format!("/{label}: no active agent profile"));
            return;
        }
    };
    // HTTP path.
    if let Some(base) = server_base(&data_dir()) {
        let body = serde_json::json!({ "file": kind, "content": content });
        match request(
            "PUT",
            &base,
            &format!("/api/profiles/{}/files", encode(&name)),
            Some(body),
        ) {
            Ok(v) => {
                let path = v
                    .get("path")
                    .and_then(|p| p.as_str())
                    .unwrap_or("(declared path)");
                state.add_status(format!("/{label}: wrote {path}"));
                return;
            }
            Err(ApiErr::Unreachable(_)) => {} // fall through to direct write
            Err(e) => {
                render_err(state, &format!("/{label}"), "dashboard", e);
                return;
            }
        }
    }
    // Direct-file fallback.
    let dd = data_dir();
    let target: PathBuf = match declared_paths(&name) {
        Ok(paths) => match paths.get(kind).and_then(|p| p.clone()) {
            Some(p) => PathBuf::from(p),
            None => dd.join("profiles").join(&name).join(default_name),
        },
        Err(e) => {
            state.add_status(format!("/{label}: {e}"));
            return;
        }
    };
    if let Some(parent) = target.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            state.add_status(format!("/{label}: create {}: {e}", parent.display()));
            return;
        }
    }
    if let Err(e) = std::fs::write(&target, content) {
        state.add_status(format!("/{label}: write {}: {e}", target.display()));
        return;
    }
    // The profile did not declare this file: point it at the new file
    // through the TUI's normal config write path.
    let mut cfg = match crate::config::Config::load(&dd) {
        Ok(c) => c,
        Err(e) => {
            state.add_status(format!(
                "/{label}: wrote {} but config reload failed: {}",
                target.display(),
                e.cause
            ));
            return;
        }
    };
    let mut pointed = false;
    if let Some(table) = cfg.agents.get_mut(&name) {
        let slot: &mut Option<String> = match field {
            "soul_file" => &mut table.soul_file,
            "user_file" => &mut table.user_file,
            "agents_file" => &mut table.agents_file,
            _ => unreachable!("file_slot covers soul/user/agents"),
        };
        if slot
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_none()
        {
            *slot = Some(target.display().to_string());
            pointed = true;
        }
    }
    if pointed {
        if let Err(e) = cfg.save(&dd) {
            state.add_status(format!(
                "/{label}: wrote {} but failed to point the profile at it: {}",
                target.display(),
                e.cause
            ));
            return;
        }
    }
    state.add_status(format!("/{label}: wrote {}", target.display()));
}

/// `/soul`, `/userfile`, `/agentsfile`: bare prints the file, `set
/// <text>` writes it. `label` is the command word for messages.
pub fn do_persona(
    state: &mut TuiState,
    session: &Arc<Session>,
    cmd: &str,
    kind: &str,
    label: &str,
) {
    let rest = cmd.strip_prefix(&format!("/{label}")).unwrap_or("").trim();
    if rest.is_empty() {
        read_persona(state, session, kind, label);
    } else if let Some(text) = rest.strip_prefix("set ") {
        write_persona(state, session, kind, label, text.trim());
    } else if rest == "set" {
        state.add_status(format!("usage: /{label} set <text>"));
    } else {
        state.add_status(format!("usage: /{label} | /{label} set <text>"));
    }
}

// ---------------------------------------------------------------------------
// /agent new <name> / /agents create <name> - profile creation
// ---------------------------------------------------------------------------

/// Parsed `/agent new` invocation: the name plus the three creation flags.
#[derive(Debug, PartialEq)]
struct NewArgs {
    name: String,
    display_name: Option<String>,
    inherits: Option<String>,
    policy: Option<String>,
}

fn parse_new(rest: &str) -> Result<NewArgs, String> {
    const USAGE: &str =
        "usage: /agent new <name> [--display-name NAME] [--inherits PARENT] [--policy POLICY]";
    let mut toks = rest.split_whitespace().peekable();
    let name = toks.next().ok_or(USAGE)?.to_string();
    let mut display_name: Option<String> = None;
    let mut inherits: Option<String> = None;
    let mut policy: Option<String> = None;
    while let Some(t) = toks.next() {
        let flag = t
            .strip_prefix("--")
            .ok_or(format!("expected a --flag, got {t:?} ({USAGE})"))?;
        let (key, inline) = match flag.split_once('=') {
            Some((k, v)) => (k, Some(v)),
            None => (flag, None),
        };
        let val = inline
            .map(str::to_string)
            .or_else(|| toks.next().map(str::to_string))
            .ok_or(format!("--{key} needs a value ({USAGE})"))?;
        match key {
            "display-name" => display_name = Some(val),
            "inherits" => inherits = Some(val),
            "policy" => policy = Some(val),
            _ => return Err(format!("unknown flag --{key} ({USAGE})")),
        }
    }
    Ok(NewArgs {
        name,
        display_name,
        inherits,
        policy,
    })
}

/// Create `[agents.<name>]` and persist it through the TUI's normal
/// config write path (`Config::load` → mutate → `Config::save`), the same
/// mechanism `/model` uses. The new table is validated through
/// `profile_registry()` before anything is written: an unknown parent,
/// bad policy, or malformed slug aborts with the registry's error.
pub fn do_agent_new(state: &mut TuiState, rest: &str) {
    let args = match parse_new(rest) {
        Ok(a) => a,
        Err(e) => {
            state.add_status(format!("/agent new: {e}"));
            return;
        }
    };
    let dd = data_dir();
    let mut cfg = crate::config::Config::load(&dd).unwrap_or_default();
    if cfg.agents.contains_key(&args.name) {
        state.add_status(format!(
            "/agent new: [agents.{}] already declared - switch with /agent {}",
            args.name, args.name
        ));
        return;
    }
    let profile = pantheon_api::agent_profile::AgentProfile {
        display_name: args.display_name,
        inherits: args.inherits,
        policy: args.policy,
        ..Default::default()
    };
    cfg.agents.insert(args.name.clone(), profile);
    // Validate the whole table set (name slug, policy spelling,
    // inheritance) before writing anything.
    if let Err(e) = cfg.profile_registry() {
        cfg.agents.remove(&args.name);
        state.add_status(format!("/agent new: {e}"));
        return;
    }
    match cfg.save(&dd) {
        Ok(()) => state.add_status(format!(
            "profile [agents.{}] created - switch with /agent {}",
            args.name, args.name
        )),
        Err(e) => state.add_status(format!("/agent new: save failed: {}", e.cause)),
    }
}
