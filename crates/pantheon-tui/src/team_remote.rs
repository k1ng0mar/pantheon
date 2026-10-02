//! `/team` - use a team of experts from the Teams gallery.
//!
//! Teams are named multi-agent rosters (dashboard `teams.rs`): every member
//! is an expert from the Experts gallery carrying a team-specific role, and
//! the member's display identity (name, color, icon) is derived from the
//! linked expert. The dashboard seeds five bundled teams on first access;
//! users add their own on the dashboard's Teams page.
//!
//! Verbs (all against the dashboard HTTP API, mirroring `swarm_remote.rs`
//! the TUI never spawns swarms itself):
//! - `/team` → list teams: id, name, expert count, one-line description.
//! - `/team <id>` → roster detail: each expert with its role in the team.
//! - `/team <id> <task...>` → launch the team on the task
//!   (`POST /api/teams/:id/use` {task, judge: true}); the reply names the
//!   swarm id and its agents so the user can follow it with `/swarm <id>`.
//!
//! HTTP plumbing deliberately duplicates `swarm_remote.rs` rather than
//! importing it: that module's helpers are private and this one must not
//! reshape another agent's in-flight work (same rationale as
//! `plugin_remote.rs`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::session::TuiState;

/// Dashboard `/api/*` auth: the token header the serve listener enforces.
const TOKEN_HEADER: &str = "X-Pantheon-Token";

const USAGE: &str = "usage: /team | /team <id> | /team <id> <task...>";

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

/// Percent-encode a path segment (team ids are slugs, so this is
/// defensive only).
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
/// JSON body. (ureq is built without its `json` feature here, so this goes
/// through `send_string` + `serde_json::from_str` like the rest of the
/// crate.)
fn request(
    method: &str,
    base: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<serde_json::Value, ApiErr> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
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
fn base_or_note(state: &mut TuiState) -> Option<String> {
    match server_base(&data_dir()) {
        Some(b) => Some(b),
        None => {
            state.add_status(
                "/team: no dashboard address ([server] host/port unset, port 0 = auto)".into(),
            );
            None
        }
    }
}

fn note_err(state: &mut TuiState, id: Option<&str>, err: ApiErr) {
    match err {
        ApiErr::Unreachable(t) => state.add_status(format!(
            "/team: dashboard unreachable ({t}) - is `pantheon serve` running?"
        )),
        ApiErr::Status(404, _) => match id {
            Some(id) => state.add_status(format!("/team: no team '{id}'")),
            None => state.add_status("/team: not found".into()),
        },
        ApiErr::Status(code, body) => {
            state.add_status(format!("/team: dashboard error {code}: {body}"))
        }
    }
}

/// One-line team summary for the list view. Pure: the JSON contract is
/// `to_client_json` (dashboard `teams.rs`), so the rendering contract is
/// testable without a running server.
fn team_summary_line(team: &serde_json::Value) -> String {
    let id = team.get("id").and_then(|v| v.as_str()).unwrap_or("?");
    let name = team.get("name").and_then(|v| v.as_str()).unwrap_or("?");
    let count = team
        .get("member_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let desc: String = team
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .chars()
        .take(72)
        .collect();
    if desc.is_empty() {
        format!("{id} - {name} ({count} experts)")
    } else {
        format!("{id} - {name} ({count} experts): {desc}")
    }
}

/// Roster lines for the detail view: `expert name - role`, unresolved
/// members (expert deleted after the team was written) flagged as such.
fn roster_lines(team: &serde_json::Value) -> Vec<String> {
    let members = team
        .get("members")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    members
        .iter()
        .map(|m| {
            let role: String = m
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .chars()
                .take(80)
                .collect();
            let expert_name = m
                .get("expert")
                .and_then(|e| e.get("name"))
                .and_then(|v| v.as_str());
            match expert_name {
                Some(name) if role.is_empty() => name.to_string(),
                Some(name) => format!("{name} - {role}"),
                None => {
                    let id = m.get("expert_id").and_then(|v| v.as_str()).unwrap_or("?");
                    format!("{id} (expert missing)")
                }
            }
        })
        .collect()
}

/// `/team`: list every team in the gallery.
fn list_teams(state: &mut TuiState, base: &str) {
    match request("GET", base, "/api/teams", None) {
        Ok(v) => {
            let teams = v
                .get("teams")
                .and_then(|t| t.as_array())
                .cloned()
                .unwrap_or_default();
            if teams.is_empty() {
                state.add_status(
                    "/team: no teams yet - add them on the dashboard's Teams page".into(),
                );
                return;
            }
            for t in &teams {
                state.add_status(team_summary_line(t));
            }
            state.add_status(format!(
                "{} team(s) - /team <id> for the roster, /team <id> <task> to launch",
                teams.len()
            ));
        }
        Err(e) => note_err(state, None, e),
    }
}

/// `/team <id>`: show the roster.
fn show_team(state: &mut TuiState, base: &str, id: &str) {
    match request("GET", base, &format!("/api/teams/{}", encode(id)), None) {
        Ok(team) => {
            let name = team.get("name").and_then(|v| v.as_str()).unwrap_or(id);
            state.add_status(format!("team {name}"));
            if let Some(desc) = team.get("description").and_then(|v| v.as_str()) {
                if !desc.trim().is_empty() {
                    state.add_status(desc.to_string());
                }
            }
            for line in roster_lines(&team) {
                state.add_status(format!("  {line}"));
            }
        }
        Err(e) => note_err(state, Some(id), e),
    }
}

/// `/team <id> <task...>`: launch the team on the task.
fn use_team(state: &mut TuiState, base: &str, id: &str, task: &str) {
    let body = serde_json::json!({ "task": task, "judge": true });
    match request(
        "POST",
        base,
        &format!("/api/teams/{}/use", encode(id)),
        Some(body),
    ) {
        Ok(v) => {
            let swarm_id = v
                .get("swarm_id")
                .and_then(|s| s.as_str())
                .or_else(|| v.get("id").and_then(|s| s.as_str()))
                .unwrap_or("?");
            let agents: Vec<String> = v
                .get("agents")
                .and_then(|a| a.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|a| {
                            a.get("name")
                                .and_then(|n| n.as_str())
                                .or_else(|| a.as_str())
                                .map(|s| s.to_string())
                        })
                        .collect()
                })
                .unwrap_or_default();
            if agents.is_empty() {
                state.add_status(format!("team '{id}' launched: swarm {swarm_id}"));
            } else {
                state.add_status(format!(
                    "team '{id}' launched: swarm {swarm_id} ({}: {})",
                    agents.len(),
                    agents.join(", ")
                ));
            }
            state.add_status(format!("follow it with /swarm {swarm_id}"));
        }
        Err(e) => note_err(state, Some(id), e),
    }
}

/// Dispatch `/team ...`.
pub fn do_team(state: &mut TuiState, cmd: &str) {
    let rest = cmd.strip_prefix("/team").unwrap_or("").trim();
    if rest.is_empty() {
        let Some(base) = base_or_note(state) else {
            return;
        };
        list_teams(state, &base);
        return;
    }
    let mut words = rest.split_whitespace();
    let id = words.next().unwrap_or("");
    if id.is_empty() {
        state.add_status(USAGE.into());
        return;
    }
    let task: String = words.collect::<Vec<_>>().join(" ");
    let Some(base) = base_or_note(state) else {
        return;
    };
    if task.is_empty() {
        show_team(state, &base, id);
    } else {
        use_team(state, &base, id, &task);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_team() -> serde_json::Value {
        serde_json::json!({
            "id": "deep-research",
            "name": "Deep Research",
            "description": "Fan-out research crew: four analysts gather, one synthesizes, one writes.",
            "member_count": 7,
            "members": [
                {"expert_id": "research-lead", "role": "Sets the research questions", "profile": "default",
                 "expert": {"id": "research-lead", "name": "Research Lead", "color": "#fff", "icon": "🔍"}},
                {"expert_id": "ghost", "role": "whatever", "profile": "default", "expert": serde_json::Value::Null},
            ],
        })
    }

    #[test]
    fn summary_line_has_id_name_count_and_desc() {
        let line = team_summary_line(&sample_team());
        assert!(line.contains("deep-research"), "{line}");
        assert!(line.contains("Deep Research"), "{line}");
        assert!(line.contains("7 experts"), "{line}");
        assert!(line.contains("Fan-out research crew"), "{line}");
    }

    #[test]
    fn summary_line_tolerates_missing_fields() {
        let line = team_summary_line(&serde_json::json!({}));
        assert!(line.contains('?'), "{line}");
    }

    #[test]
    fn roster_names_experts_with_roles() {
        let lines = roster_lines(&sample_team());
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("Research Lead"), "{}", lines[0]);
        assert!(
            lines[0].contains("Sets the research questions"),
            "{}",
            lines[0]
        );
        assert!(lines[1].contains("missing"), "{}", lines[1]);
    }
}
