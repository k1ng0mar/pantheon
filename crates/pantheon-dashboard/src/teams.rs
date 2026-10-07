//! Team templates: named multi-agent rosters a user can launch as a swarm.
//!
//! A team is genuinely composed of experts: every member carries an
//! `expert_id` referencing the Experts gallery, and the member's display
//! identity (name, avatar color, icon) is derived from the linked expert
//! at read time - the expert is the single source of truth. Members keep
//! a team-specific `role` line (their job within that team) and the agent
//! profile they run as. Storage is one JSON file per team under
//! `<data_dir>/teams/<id>.json` (see [`crate::templates`]).
//!
//! - `GET /api/teams` → `{"teams": [...]}` (members enriched with their
//!   expert's display identity, plus `member_count`).
//! - `POST /api/teams` ← a [`Team`] → 201 (dangling `expert_id` → 400).
//! - `GET /api/teams/:id` → one team, 404 when missing.
//! - `PUT /api/teams/:id` ← a [`Team`] → replaced (the URL id wins), 404
//!   when missing, dangling `expert_id` → 400.
//! - `DELETE /api/teams/:id` → 404 when missing.
//! - `POST /api/teams/:id/use` ← `{"task": "...", "judge": true}` → spawns
//!   a swarm through the existing orchestrator exactly like
//!   `swarm::create` profiles mode, and answers like it:
//!   `{ok, swarm_id, id, run_id, agents, status_view}`. `run_id` is the
//!   first agent's run id - a swarm is a fan-out, not a single session,
//!   so session-style screens load that run. `task` overrides the team's
//!   `brief_template` when non-empty; `judge` defaults to true.
//!   A member whose `expert_id` no longer resolves (expert deleted after
//!   the team was written) fails closed: 400, nothing spawns.
//!
//! Seeding: the five bundled teams below are written on first access,
//! only when the gallery holds no team files - re-seeding never
//! overwrites user edits or user-added teams. The experts gallery is
//! seeded first, and every bundled member is verified against the bundled
//! expert list; a broken reference is a code bug, so seeding fails loudly
//! (500) instead of writing a team that can never launch.

use crate::templates;
use crate::{bad_json, body_json, created_json, err_json, json_ok, App};
use pantheon_gateway::http::{Request, Response};
use pantheon_runtime::swarm_exec::{AgentSpec, ExecutionPlan, PlanTopology, StageSpec, SwarmError};

/// Gallery directory name under the data dir.
const KIND: &str = "teams";

/// A team member: a reference to an expert in the Experts gallery plus
/// the member's job within this team. Display identity (name, color,
/// icon) is never stored here - it is derived from the linked expert at
/// read time (see [`Team::to_client_json`]), so a team is genuinely
/// composed of experts rather than carrying a stale copy of them.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TeamMember {
    /// Expert id (`experts/<id>`); must resolve at write time and at
    /// `use` time (fail closed on dangling references).
    pub expert_id: String,
    /// One-line job description within this team (roster UI).
    pub role: String,
    /// Agent profile ref (`[agents.<name>]`); `"default"` is the
    /// conventional default profile (see [`resolve_member_profile`]).
    pub profile: String,
}

/// How a team executes, as a primary label. A team may combine patterns
/// (Deep Research is fan-out → merge → review loop); the `stages` carry
/// the real flow, `topology` names the dominant one for the gallery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Topology {
    /// Stages run in order, each feeding the next.
    Pipeline,
    /// Members work in parallel, then results merge.
    ParallelMerge,
    /// Stages run in order with a bounded verify loop.
    ReviewLoop,
}

/// One step of a team's execution flow.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Stage {
    /// Stage name, unique within the team; `loop_back_to` targets it.
    pub name: String,
    /// Expert ids working this stage; must be a subset of the team's
    /// members. Several ids here means parallel work within the stage.
    pub members: Vec<String>,
    /// What this stage consumes, as a structured contract (e.g.
    /// `"claims[] as {statement, source_url}"`) - data, not prose dumps.
    pub input_contract: String,
    /// What this stage produces, as a structured contract (e.g.
    /// `"verdict {pass: bool, failed_claims[]}"`).
    pub output_contract: String,
    /// Optional review edge: on verification failure, redo this earlier
    /// stage. Must name an *earlier* stage - forward references are
    /// unexecutable, so they fail validation (the stage graph stays a
    /// DAG plus bounded backward review edges).
    pub loop_back_to: Option<String>,
}

/// A launchable team template.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Team {
    /// Slug id (`[a-zA-Z0-9_-]+`); also the file name.
    pub id: String,
    pub name: String,
    pub description: String,
    /// Task brief used when `POST /api/teams/:id/use` gets no `task`
    /// override. Supports a `{{task}}` placeholder for callers that
    /// render it; otherwise used as-is.
    pub brief_template: String,
    pub members: Vec<TeamMember>,
    /// Primary execution pattern; `stages` carry the real flow.
    pub topology: Topology,
    /// The lead's expert id: the ONLY member that talks to the user.
    /// Must be one of `members`. The lead's run is the user-facing one
    /// at `use` time.
    pub lead_expert_id: String,
    /// Ordered execution flow; each stage hands structured data (not
    /// prose) to the next via its contracts.
    pub stages: Vec<Stage>,
}

impl Team {
    fn member_count(&self) -> usize {
        self.members.len()
    }

    fn to_json(&self) -> serde_json::Value {
        let mut v = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        v["member_count"] = serde_json::json!(self.member_count());
        v
    }

    /// Client-facing JSON: the stored shape plus `member_count`, with
    /// every member enriched by its expert's display identity:
    /// `{expert_id, role, profile, expert: {id, name, color, icon}}`.
    /// `expert` is `null` when the linked expert no longer exists (e.g.
    /// deleted after the team was written) - clients render those members
    /// as unresolved, and `use` refuses to launch them (fail closed).
    /// `lead` carries the lead expert's display identity (null when the
    /// lead's expert is gone); stages keep expert ids, which clients join
    /// against `members`.
    fn to_client_json(&self, data_dir: &std::path::Path) -> serde_json::Value {
        let mut v = self.to_json();
        let expert_json = |id: &str| {
            crate::experts::find_expert(data_dir, id).map(|e| {
                serde_json::json!({
                    "id": e.id,
                    "name": e.name,
                    "color": e.color,
                    "icon": e.icon,
                })
            })
        };
        let members: Vec<serde_json::Value> = self
            .members
            .iter()
            .map(|m| {
                serde_json::json!({
                    "expert_id": m.expert_id,
                    "role": m.role,
                    "profile": m.profile,
                    "expert": expert_json(&m.expert_id),
                })
            })
            .collect();
        v["members"] = serde_json::Value::Array(members);
        v["lead"] = expert_json(&self.lead_expert_id).unwrap_or(serde_json::Value::Null);
        v
    }
}

/// A member is only valid if its expert exists: a team genuinely
/// composed of experts cannot reference one that isn't there.
/// Structural validation: no data-dir access, so it is safe to re-run
/// at `use` time on a hand-edited team file. Covers the topology model:
/// the lead must be a member, stage members must be a subset of the
/// team, and review edges must point at earlier stages (the stage graph
/// stays a DAG plus bounded backward review edges - forward references
/// would be unexecutable).
fn validate_structure(t: &Team) -> Result<(), String> {
    if !templates::valid_slug(&t.id) {
        return Err(format!(
            "team id {:?} must be a slug: ASCII alphanumerics, '-', '_'",
            t.id
        ));
    }
    if t.name.trim().is_empty() {
        return Err("team name must not be empty".into());
    }
    if t.members.is_empty() || t.members.len() > 8 {
        return Err(format!(
            "team must have 1..=8 members (swarm cap), got {}",
            t.members.len()
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for m in &t.members {
        if m.expert_id.trim().is_empty() {
            return Err("member expert_id must not be empty".into());
        }
        if !templates::valid_slug(&m.expert_id) {
            return Err(format!(
                "member expert_id {:?} must be a slug: ASCII alphanumerics, '-', '_'",
                m.expert_id
            ));
        }
        if !seen.insert(m.expert_id.as_str()) {
            return Err(format!("duplicate member {:?}", m.expert_id));
        }
        if m.role.trim().is_empty() {
            return Err(format!("member {:?} role must not be empty", m.expert_id));
        }
        if m.profile.trim().is_empty() {
            return Err(format!(
                "member {:?} profile must not be empty",
                m.expert_id
            ));
        }
    }
    // The lead is the only member that talks to the user: it must be one
    // of the members.
    if !t.members.iter().any(|m| m.expert_id == t.lead_expert_id) {
        return Err(format!(
            "lead expert {:?} is not a team member",
            t.lead_expert_id
        ));
    }
    if t.stages.is_empty() {
        return Err("team must define at least one stage".into());
    }
    let mut stage_names = std::collections::HashSet::new();
    for (i, s) in t.stages.iter().enumerate() {
        if s.name.trim().is_empty() {
            return Err(format!("stage {} name must not be empty", i + 1));
        }
        if !stage_names.insert(s.name.as_str()) {
            return Err(format!("duplicate stage name {:?}", s.name));
        }
        if s.members.is_empty() {
            return Err(format!("stage {:?} must have at least one member", s.name));
        }
        for em in &s.members {
            if !t.members.iter().any(|m| &m.expert_id == em) {
                return Err(format!(
                    "stage {:?} references {:?}, which is not a team member",
                    s.name, em
                ));
            }
        }
        if s.input_contract.trim().is_empty() {
            return Err(format!(
                "stage {:?} input_contract must not be empty",
                s.name
            ));
        }
        if s.output_contract.trim().is_empty() {
            return Err(format!(
                "stage {:?} output_contract must not be empty",
                s.name
            ));
        }
        if let Some(target) = &s.loop_back_to {
            match t.stages[..i].iter().position(|p| &p.name == target) {
                Some(_) => {}
                None => {
                    if t.stages.iter().any(|p| &p.name == target) {
                        return Err(format!(
                            "stage {:?} loops back to {:?}, which is not an earlier stage (forward references are unexecutable)",
                            s.name, target
                        ));
                    }
                    return Err(format!(
                        "stage {:?} loops back to unknown stage {:?}",
                        s.name, target
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Full write-time validation: structure plus expert existence. A team
/// is genuinely composed of experts, so a member referencing one that
/// isn't in the gallery is rejected.
fn validate_team(data_dir: &std::path::Path, t: &Team) -> Result<(), String> {
    validate_structure(t)?;
    for m in &t.members {
        if crate::experts::find_expert(data_dir, &m.expert_id).is_none() {
            return Err(format!(
                "unknown expert {:?}: no such expert in the Experts gallery",
                m.expert_id
            ));
        }
    }
    Ok(())
}

/// Seed the gallery when it is empty. Idempotent by construction (see
/// [`templates::seed_if_empty`]).
///
/// The experts gallery seeds first: teams reference experts, so every
/// bundled member is verified against the bundled expert list before
/// anything is written. A dangling bundled reference is a code bug
/// fail loudly (500) rather than seeding a team that can never launch.
/// (An expert a user deletes later is a runtime condition, handled
/// per-team at `use` time - it must not break the gallery.)
fn ensure_seeded(data_dir: &std::path::Path) -> Result<(), Response> {
    crate::experts::ensure_seeded(data_dir)?;
    templates::seed_if_empty(data_dir, KIND, &bundled_teams(), |t| &t.id)
        .map_err(|e| err_json(500, "TEAMS", &e))?;
    let bundled = crate::experts::bundled_experts();
    let known: std::collections::HashSet<&str> = bundled.iter().map(|e| e.id.as_str()).collect();
    for team in bundled_teams() {
        for m in &team.members {
            if !known.contains(m.expert_id.as_str()) {
                return Err(err_json(
                    500,
                    "TEAMS",
                    &format!(
                        "bundled team {:?} references unknown expert {:?}",
                        team.id, m.expert_id
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn team_response(data_dir: &std::path::Path, team: &Team) -> Response {
    json_ok(team.to_client_json(data_dir))
}

/// `GET /api/teams`: every team, members enriched with expert identity,
/// plus `member_count`.
pub fn list(app: &App) -> Response {
    if let Err(r) = ensure_seeded(&app.data_dir) {
        return r;
    }
    let items = match templates::list_items::<Team>(&templates::store_dir(&app.data_dir, KIND)) {
        Ok(i) => i,
        Err(e) => return err_json(500, "TEAMS", &e),
    };
    json_ok(serde_json::json!({
        "teams": items.into_iter().map(|(_, t)| t.to_client_json(&app.data_dir)).collect::<Vec<_>>(),
    }))
}

fn parse_team_body(req: &Request) -> Result<Team, Response> {
    let body = body_json(req)?;
    serde_json::from_value::<Team>(body).map_err(|e| bad_json(&format!("invalid team body: {e}")))
}

/// `POST /api/teams`: create a team. 409 when the id is taken.
pub fn create(app: &App, req: &Request) -> Response {
    if let Err(r) = ensure_seeded(&app.data_dir) {
        return r;
    }
    let team = match parse_team_body(req) {
        Ok(t) => t,
        Err(r) => return r,
    };
    if let Err(e) = validate_team(&app.data_dir, &team) {
        return bad_json(&e);
    }
    let path = match templates::item_path(&app.data_dir, KIND, &team.id) {
        Some(p) => p,
        None => return bad_json("team id must be a slug"),
    };
    if path.exists() {
        return err_json(409, "TEAMS", &format!("team {:?} already exists", team.id));
    }
    if let Err(e) = templates::write_item(&path, &team) {
        return err_json(500, "TEAMS", &e);
    }
    created_json(team.to_client_json(&app.data_dir))
}

/// `GET /api/teams/:id`: one team, 404 when missing.
pub fn get(app: &App, id: &str) -> Response {
    if let Err(r) = ensure_seeded(&app.data_dir) {
        return r;
    }
    let path = match templates::item_path(&app.data_dir, KIND, id) {
        Some(p) => p,
        None => return bad_json("team id must be a slug"),
    };
    match templates::read_item::<Team>(&path) {
        Ok(t) => team_response(&app.data_dir, &t),
        Err(_) => err_json(404, "TEAMS", &format!("no team {id:?}")),
    }
}

/// `PUT /api/teams/:id`: replace the team. The URL id is authoritative
/// a body id that disagrees is overwritten. 404 when missing.
pub fn update(app: &App, id: &str, req: &Request) -> Response {
    if let Err(r) = ensure_seeded(&app.data_dir) {
        return r;
    }
    let path = match templates::item_path(&app.data_dir, KIND, id) {
        Some(p) => p,
        None => return bad_json("team id must be a slug"),
    };
    if !path.exists() {
        return err_json(404, "TEAMS", &format!("no team {id:?}"));
    }
    let mut team = match parse_team_body(req) {
        Ok(t) => t,
        Err(r) => return r,
    };
    team.id = id.to_string();
    if let Err(e) = validate_team(&app.data_dir, &team) {
        return bad_json(&e);
    }
    if let Err(e) = templates::write_item(&path, &team) {
        return err_json(500, "TEAMS", &e);
    }
    team_response(&app.data_dir, &team)
}

/// `DELETE /api/teams/:id`: 404 when missing.
pub fn delete(app: &App, id: &str) -> Response {
    if let Err(r) = ensure_seeded(&app.data_dir) {
        return r;
    }
    let path = match templates::item_path(&app.data_dir, KIND, id) {
        Some(p) => p,
        None => return bad_json("team id must be a slug"),
    };
    match templates::delete_item(&path) {
        Ok(true) => json_ok(serde_json::json!({"ok": true, "deleted": id})),
        Ok(false) => err_json(404, "TEAMS", &format!("no team {id:?}")),
        Err(e) => err_json(500, "TEAMS", &e),
    }
}

// ---------------------------------------------------------------------------
// POST /api/teams/:id/use - profile resolution and swarm launch
// ---------------------------------------------------------------------------

/// Resolve a member's profile ref to the concrete profile the swarm
/// agent should run as.
///
/// `"default"` is the conventional default profile name
/// (`pantheon_api::agent_profile::DEFAULT_PROFILE` - "the identity used
/// when no profile is selected"). It resolves exactly like
/// `Config::resolve_profile` does: the config's active `agent` (the same
/// field `profiles.rs` reads as the default profile) wins, else the
/// literal `"default"`. The resolved name must be declared in `[agents]`,
/// except in the fully-anonymous case (no active agent and an empty or
/// missing `[agents]` table), where `"default"` passes through
/// untouched: swarm count mode already runs such turns without `--agent`
/// (see `swarm::agent_flag_for`), so the bundled teams stay usable on
/// installs that predate profiles.
///
/// Any other name must be declared in `[agents]`, exactly like
/// `swarm::create` profiles mode - unknown → Err naming the bad profile,
/// and nothing spawns (fail closed; a typo must never silently run the
/// wrong identity).
fn resolve_member_profile(
    cfg: Option<&pantheon_api::config::Config>,
    profile: &str,
) -> Result<String, String> {
    let profile = profile.trim();
    if profile.is_empty() {
        return Err("member profile must not be empty".into());
    }
    let default_name = pantheon_api::agent_profile::DEFAULT_PROFILE;
    if profile != default_name {
        let known = cfg
            .as_ref()
            .map(|c| c.agents.contains_key(profile))
            .unwrap_or(false);
        return if known {
            Ok(profile.to_string())
        } else {
            Err(format!(
                "unknown profile \"{profile}\": not declared in [agents]"
            ))
        };
    }
    // "default": the active `agent` wins (profiles.rs's default-profile
    // field); it must be declared - an active name pointing nowhere is a
    // broken config, fail closed on it.
    let active = cfg
        .as_ref()
        .and_then(|c| c.agent.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(name) = active {
        let known = cfg
            .as_ref()
            .map(|c| c.agents.contains_key(name))
            .unwrap_or(false);
        return if known {
            Ok(name.to_string())
        } else {
            Err(format!(
                "unknown profile \"{name}\": the active `agent` is not declared in [agents]"
            ))
        };
    }
    if cfg
        .as_ref()
        .map(|c| c.agents.contains_key(default_name))
        .unwrap_or(false)
    {
        return Ok(default_name.to_string());
    }
    // Anonymous install: nothing declared at all. Pass "default" through;
    // the turn child degrades to no `--agent` instead of failing.
    Ok(default_name.to_string())
}

fn swarm_err(e: SwarmError) -> Response {
    err_json(e.http_status, &e.code, &e.message)
}

/// `POST /api/teams/:id/use` ← `{"task": "...", "judge": true}`.
///
/// Fail-closed validation happens before anything spawns: structural
/// validation first (lead/stages survive hand-edits), then every member
/// must resolve to a real expert, then every profile must resolve. Any
/// failure is a 400 and nothing spawns.
///
/// The team's stages become an [`ExecutionPlan`] and execution goes
/// through the swarm orchestrator's staged path (`create_staged`), which
/// mechanically runs the topology: stages in order, handoff inputs
/// between them, bounded review loops, member runs never user-facing.
/// The response mirrors `swarm::create`'s shape, plus `run_id` - the
/// lead's coordination run, the only user-facing run.
pub fn use_team(app: &App, id: &str, req: &Request) -> Response {
    if let Err(r) = ensure_seeded(&app.data_dir) {
        return r;
    }
    let path = match templates::item_path(&app.data_dir, KIND, id) {
        Some(p) => p,
        None => return bad_json("team id must be a slug"),
    };
    let team: Team = match templates::read_item(&path) {
        Ok(t) => t,
        Err(_) => return err_json(404, "TEAMS", &format!("no team {id:?}")),
    };
    let body = match body_json(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let task_override = body
        .get("task")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .unwrap_or("");
    let base_task = if task_override.is_empty() {
        team.brief_template.trim()
    } else {
        task_override
    };
    if base_task.is_empty() {
        return bad_json("field \"task\" is required (or set the team's brief_template)");
    }
    let judge = body.get("judge").and_then(|v| v.as_bool()).unwrap_or(true);
    if team.members.is_empty() || team.members.len() > 8 {
        return bad_json("team must have 1..=8 members to launch");
    }
    if let Err(e) = validate_structure(&team) {
        return bad_json(&e);
    }
    let cfg = crate::session_factory::load_config(&app.data_dir)
        .ok()
        .flatten();
    let mut resolved: Vec<crate::experts::Expert> = Vec::with_capacity(team.members.len());
    for m in &team.members {
        match crate::experts::find_expert(&app.data_dir, &m.expert_id) {
            Some(e) => resolved.push(e),
            None => {
                return bad_json(&format!(
                    "team {:?} references unknown expert {:?}: the expert was deleted or never existed; nothing was launched",
                    team.id, m.expert_id
                ))
            }
        };
    }
    let expert_by_id: std::collections::HashMap<&str, &crate::experts::Expert> =
        resolved.iter().map(|e| (e.id.as_str(), e)).collect();
    // Resolve every member's profile before building the plan.
    let mut profile_by_id: std::collections::HashMap<&str, String> =
        std::collections::HashMap::with_capacity(team.members.len());
    for (m, e) in team.members.iter().zip(resolved.iter()) {
        match resolve_member_profile(cfg.as_ref(), &m.profile) {
            Ok(p) => {
                profile_by_id.insert(e.id.as_str(), p);
            }
            Err(e) => return bad_json(&e),
        }
    }
    let lead = match expert_by_id.get(team.lead_expert_id.as_str()) {
        Some(e) => *e,
        None => return bad_json("team lead does not resolve to an expert"),
    };
    let lead_profile = profile_by_id
        .get(team.lead_expert_id.as_str())
        .cloned()
        .unwrap_or_default();
    let topology = match team.topology {
        Topology::Pipeline => PlanTopology::Pipeline,
        Topology::ParallelMerge => PlanTopology::ParallelMerge,
        Topology::ReviewLoop => PlanTopology::ReviewLoop,
    };
    let mut stages = Vec::with_capacity(team.stages.len());
    for s in &team.stages {
        let mut members = Vec::with_capacity(s.members.len());
        for member_id in &s.members {
            match expert_by_id.get(member_id.as_str()) {
                Some(e) => members.push(AgentSpec {
                    name: e.name.clone(),
                    profile: profile_by_id
                        .get(member_id.as_str())
                        .cloned()
                        .unwrap_or_default(),
                }),
                None => {
                    return bad_json(&format!(
                        "stage {:?} references unknown member expert {:?}",
                        s.name, member_id
                    ))
                }
            }
        }
        let loop_back_to = match &s.loop_back_to {
            Some(name) => match team.stages.iter().position(|o| &o.name == name) {
                Some(i) => Some(i),
                None => {
                    return bad_json(&format!(
                        "stage {:?} loops back to unknown stage {name:?}",
                        s.name
                    ))
                }
            },
            None => None,
        };
        stages.push(StageSpec {
            name: s.name.clone(),
            members,
            input_contract: s.input_contract.clone(),
            output_contract: s.output_contract.clone(),
            loop_back_to,
        });
    }
    let plan = ExecutionPlan {
        team_name: team.name.clone(),
        topology,
        stages,
        lead_name: lead.name.clone(),
        lead_profile,
        max_review_iterations: 3,
    };
    match app.swarm.create_staged(base_task, plan, judge) {
        Ok(created) => {
            // The lead's coordination run is the user-facing run.
            let run_id = created.status.lead_run_id.clone().unwrap_or_default();
            let agent_names: Vec<&str> = created
                .status
                .agents
                .iter()
                .map(|a| a.name.as_str())
                .collect();
            let mut out = serde_json::json!({
                "ok": true,
                "swarm_id": created.id,
                "id": created.id,
                "run_id": run_id,
                "agents": agent_names,
            });
            out["status_view"] =
                serde_json::to_value(&created.status).unwrap_or(serde_json::Value::Null);
            created_json(out)
        }
        Err(e) => swarm_err(e),
    }
}

// ---------------------------------------------------------------------------
// Bundled teams (seeded on first access)
// ---------------------------------------------------------------------------

fn member(expert_id: &str, role: &str) -> TeamMember {
    TeamMember {
        expert_id: expert_id.to_string(),
        role: role.to_string(),
        // Bundled members run as the conventional default profile; the
        // concrete identity resolves at `use` time (see
        // `resolve_member_profile`). Never validated at seed time - the
        // user's config is unknown until then.
        profile: pantheon_api::agent_profile::DEFAULT_PROFILE.to_string(),
    }
}

/// Bundled-team stage builder. `loop_back_to` names an earlier stage
/// (backward review edge); `None` for plain forward stages.
fn stage(
    name: &str,
    members: &[&str],
    input_contract: &str,
    output_contract: &str,
    loop_back_to: Option<&str>,
) -> Stage {
    Stage {
        name: name.to_string(),
        members: members.iter().map(|s| s.to_string()).collect(),
        input_contract: input_contract.to_string(),
        output_contract: output_contract.to_string(),
        loop_back_to: loop_back_to.map(|s| s.to_string()),
    }
}

/// The five bundled teams. Every member references a real expert id;
/// rosters are Umar's spec - teams are genuinely composed of experts.
fn bundled_teams() -> Vec<Team> {
    vec![
        Team {
            id: "deep-research".into(),
            name: "Deep Research".into(),
            description: "Fan-out research crew: four analysts gather, one synthesizes, one writes.".into(),
            brief_template: "Research brief: {{task}}\n\nInvestigate the topic above thoroughly. Four analysts gather findings in parallel, a synthesis analyst merges them into one verified picture, and a report writer turns it into the final report. Every substantive claim must cite a source. Flag disagreements between sources explicitly, and end with a short list of open questions the evidence could not settle.".into(),
            members: vec![
                member("research-lead", "Sets the research questions, runs the crew, answers the user"),
                member("researcher", "Finds primary sources and original material"),
                member("source-analyst", "Gathers evidence and verifies every source before anyone trusts it"),
                member("literature-analyst", "Sniffs out AI-generated slop and hollow writing in sources and drafts"),
                member("market-analyst", "Supplies prices, volumes, and incentives"),
                member("synthesis-analyst", "Merges the four streams into one verified synthesis"),
                member("report-writer", "Turns the synthesis into the final report"),
            ],
            topology: Topology::ParallelMerge,
            lead_expert_id: "research-lead".into(),
            stages: vec![
                stage(
                    "gather",
                    &[
                        "researcher",
                        "source-analyst",
                        "literature-analyst",
                        "market-analyst",
                    ],
                    "research questions + scope",
                    "contributions[] as {member, findings[] as {finding, evidence, source_url, confidence}}; source-analyst's contribution additionally carries {sources[]: {url, credibility, flags[]}, verification_verdict}",
                    None,
                ),
                stage(
                    "synthesize",
                    &["synthesis-analyst"],
                    "contributions[] (four streams)",
                    "synthesis with claims[] as {statement, supporting_findings[], confidence}",
                    None,
                ),
                stage(
                    "report",
                    &["report-writer"],
                    "synthesis with claims[]",
                    "final report (markdown)",
                    None,
                ),
            ],
        },
        Team {
            id: "pitch-deck".into(),
            name: "Pitch Deck".into(),
            description: "Turns a raw idea into a tight, persuasive deck.".into(),
            brief_template: "Pitch brief: {{task}}\n\nBuild a persuasive pitch deck for the idea above. Keep it under 12 slides. Every slide earns its place: one idea per slide, concrete numbers over adjectives, and a clear ask at the end.".into(),
            members: vec![
                member("business-strategist", "Owns the story arc: problem, insight, solution, ask"),
                member("product-planner", "Keeps the deck to the smallest shippable story"),
                member("data-analyst", "Supplies the numbers that earn each slide"),
                member("copywriter", "Writes sharp slide copy with no filler"),
                member("slide-designer", "Designs the slide layouts"),
                member("data-visualizer", "Builds the charts and visual data"),
                member("evidence-reviewer", "Checks every claim against its source"),
            ],
            topology: Topology::Pipeline,
            lead_expert_id: "business-strategist".into(),
            stages: vec![
                stage(
                    "positioning",
                    &["business-strategist"],
                    "raw idea",
                    "{thesis, audience, ask}",
                    None,
                ),
                stage(
                    "narrative",
                    &["product-planner"],
                    "{thesis, audience, ask}",
                    "slide_outline[] as {slide_n, one_idea}",
                    None,
                ),
                stage(
                    "numbers",
                    &["data-analyst"],
                    "slide_outline[]",
                    "verified_figures[] as {figure, source}",
                    None,
                ),
                stage(
                    "copy",
                    &["copywriter"],
                    "slide_outline[] + verified_figures[]",
                    "slide_copy[] as {slide_n, headline, body}",
                    None,
                ),
                stage(
                    "design",
                    &["slide-designer", "data-visualizer"],
                    "slide_copy[]",
                    "deck spec (layout per slide + chart specs)",
                    None,
                ),
                stage(
                    "verify",
                    &["evidence-reviewer"],
                    "deck spec + verified_figures[]",
                    "verdict {pass: bool, failed_claims[]}",
                    Some("numbers"),
                ),
            ],
        },
        Team {
            id: "data-analysis".into(),
            name: "Data Analysis".into(),
            description: "From raw data to decisions.".into(),
            brief_template: "Analysis brief: {{task}}\n\nAnalyze the data described above. Start by validating data quality and stating your assumptions. Show your working, quantify uncertainty, and finish with the decision the numbers support.".into(),
            members: vec![
                member("data-analyst", "Runs the analysis and tests the hypotheses"),
                member("sql-analyst", "Pulls and shapes the data"),
                member("report-writer", "Writes up the findings and the decision they support"),
                member("evidence-reviewer", "Checks every claim against its source"),
            ],
            topology: Topology::ReviewLoop,
            lead_expert_id: "data-analyst".into(),
            stages: vec![
                stage(
                    "extract",
                    &["sql-analyst"],
                    "data description + access",
                    "dataset profile {schema, row_count, quality_issues[], fixes_applied[]}",
                    None,
                ),
                stage(
                    "analysis",
                    &["data-analyst"],
                    "dataset profile",
                    "findings[] as {finding, evidence, uncertainty}",
                    None,
                ),
                stage(
                    "report",
                    &["report-writer"],
                    "findings[]",
                    "report draft (markdown)",
                    None,
                ),
                stage(
                    "verify",
                    &["evidence-reviewer"],
                    "report draft + findings[]",
                    "verdict {pass: bool, failed_claims[]}",
                    Some("analysis"),
                ),
            ],
        },
        Team {
            id: "code-review".into(),
            name: "Code Review".into(),
            description: "A thorough review crew for any changeset.".into(),
            brief_template: "Review brief: {{task}}\n\nReview the code described above. Report issues ordered by severity with file and line references. Distinguish must-fix defects from style nits, and suggest concrete fixes rather than vague advice.".into(),
            members: vec![
                member("code-reviewer", "Reviews logic, readability, and API design"),
                member("security-specialist", "Hunts for injection, auth, and data-exposure issues"),
                member("test-engineer", "Checks test coverage and edge cases"),
            ],
            topology: Topology::ParallelMerge,
            lead_expert_id: "code-reviewer".into(),
            stages: vec![
                stage(
                    "review",
                    &["code-reviewer", "security-specialist", "test-engineer"],
                    "changeset (diff + context)",
                    "findings[] as {file, line, severity, issue, fix}",
                    None,
                ),
                stage(
                    "merge",
                    &["code-reviewer"],
                    "findings[]",
                    "merged review {deduped, ordered by severity, must_fix[] vs nits[]}",
                    None,
                ),
            ],
        },
        Team {
            id: "job-hunt".into(),
            name: "Job Hunt".into(),
            description: "Finds openings and writes applications that land.".into(),
            brief_template: "Job hunt brief for: {{task}}\n\nFind matching openings posted in the last two weeks, from verifiable companies only. For each: role, company, why it fits, and a tailored application draft.".into(),
            members: vec![
                member("research-analyst", "Finds roles matching the candidate's profile"),
                member("copywriter", "Tailors CVs and cover letters per role"),
                member("evidence-reviewer", "Checks every claim against the CV"),
            ],
            topology: Topology::ReviewLoop,
            lead_expert_id: "research-analyst".into(),
            stages: vec![
                stage(
                    "opportunities",
                    &["research-analyst"],
                    "candidate profile",
                    "shortlist[] as {role, company, url, posted_date, why_fits} - only posted within the last two weeks, verifiable companies",
                    None,
                ),
                stage(
                    "applications",
                    &["copywriter"],
                    "shortlist[] + CV",
                    "drafts[] as {role, company, tailored_bullets[], cover_note}",
                    None,
                ),
                stage(
                    "verify",
                    &["evidence-reviewer"],
                    "drafts[] + CV",
                    "verdict {pass: bool, failed_claims[]} - every claim must trace to the CV, no invented facts",
                    Some("applications"),
                ),
            ],
        },
    ]
}

// ---------------------------------------------------------------------------
// Tests (deterministic: tempdir, ScriptedWorker, no network/subprocesses)
// ---------------------------------------------------------------------------
