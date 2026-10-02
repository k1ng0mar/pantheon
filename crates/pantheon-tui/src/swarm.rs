//! `pantheon swarm` - spawn a real headless multi-agent run.
//!
//! Spawning is wired: each agent gets its own in-process `Session` (run
//! sequentially, never threaded - the codebase documents SQLite multi-writer
//! hangs with two live Sessions), attaches its resolved agent profile,
//! and drives one `chat_turn` under `AgentMode::Build`. The manifest at
//! `<data_dir>/swarms/<swarm_id>.json` is written with status `"running"`
//! before any turn starts, so `swarm status`/`swarm list` work mid-run,
//! then updated to `"complete"` / `"partial"` / `"failed"` after.
//!
//! Agent labels are `{swarm_id}:{role}:{i}` - the shape
//! `reconstruct_from_ledger` looks for when no manifest exists.

use pantheon_api::agent_profile::EffectiveProfile;
use pantheon_runtime::Supervisor;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Arg parsing result for a spawn invocation. Pure data, no process
/// effects - `parse_swarm_spawn` builds this; the CLI acts on it.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub n: usize,
    pub task: String,
    pub roles: Vec<String>,
    pub delivery: Option<String>,
}

/// One agent's finished turn: the transcript text, or the failure. An
/// error is never hidden - it renders under the agent's header.
#[derive(Debug)]
pub struct AgentOutcome {
    pub label: String,
    pub role: String,
    pub run_id: String,
    pub result: Result<String, String>,
}

/// Durable swarm manifest. Written at spawn time under
/// `<data_dir>/swarms/<swarm_id>.json` so `swarm list` / `swarm status`
/// work across CLI processes. Per-agent liveness still comes from the
/// ledger (each agent owns a run); the manifest is the index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmRecord {
    pub swarm_id: String,
    pub task: String,
    pub roles: Vec<String>,
    pub agents: Vec<SwarmAgent>,
    pub delivery: Option<String>,
    pub created_ms: u64,
    /// `"running"` while turns execute, then `"complete"` / `"partial"` /
    /// `"failed"`. `#[serde(default)]` so pre-status manifests still parse.
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwarmAgent {
    pub label: String,
    pub run_id: String,
    pub role: String,
    /// `"running"` while the turn executes, then `"ok"` / `"error"`.
    /// `#[serde(default)]` so pre-status manifests still parse.
    #[serde(default)]
    pub status: String,
}

fn swarms_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("swarms")
}

/// Persist (or overwrite) a swarm manifest atomically: tmp file then
/// rename, matching the rest of the codebase.
pub fn write_swarm_manifest(data_dir: &Path, rec: &SwarmRecord) -> Result<(), String> {
    let dir = swarms_dir(data_dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("swarm dir: {e}"))?;
    let path = dir.join(format!("{}.json", rec.swarm_id));
    let tmp = dir.join(format!("{}.json.tmp", rec.swarm_id));
    let raw = serde_json::to_string_pretty(rec).map_err(|e| format!("serialize swarm: {e}"))?;
    std::fs::write(&tmp, raw)
        .and_then(|_| std::fs::rename(&tmp, &path))
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

pub fn load_all_swarms(data_dir: &Path) -> Vec<SwarmRecord> {
    let dir = swarms_dir(data_dir);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&p) else {
            continue;
        };
        if let Ok(rec) = serde_json::from_str::<SwarmRecord>(&raw) {
            out.push(rec);
        }
    }
    out.sort_by(|a, b| {
        b.created_ms
            .cmp(&a.created_ms)
            .then(b.swarm_id.cmp(&a.swarm_id))
    });
    out
}

/// Resolve a full or prefix swarm id against stored manifests.
/// Exact match wins; a unique prefix resolves; several matches error.
fn resolve_swarm(data_dir: &Path, id: &str) -> Result<Option<SwarmRecord>, String> {
    let all = load_all_swarms(data_dir);
    if let Some(rec) = all.iter().find(|r| r.swarm_id == id) {
        return Ok(Some(rec.clone()));
    }
    let hits: Vec<&SwarmRecord> = all.iter().filter(|r| r.swarm_id.starts_with(id)).collect();
    match hits.len() {
        0 => Ok(None),
        1 => Ok(Some(hits[0].clone())),
        _ => {
            let mut msg = format!("ambiguous swarm id '{id}' matches:");
            for h in hits.iter().take(5) {
                msg.push_str(&format!("\n  {}", h.swarm_id));
            }
            Err(msg)
        }
    }
}

/// Parse a full argv into a `SpawnSpec`. Pure: returns `Err` messages,
/// never calls `process::exit` - the CLI maps `Err` to exit 2.
pub fn parse_swarm_spawn(args: &[String]) -> Result<SpawnSpec, String> {
    let n_str = args.get(2).ok_or_else(|| {
        "usage: pantheon swarm <N> \"<task>\" [--roles a,b] [--delivery telegram]".to_string()
    })?;
    let n: usize = n_str
        .parse()
        .map_err(|_| format!("error: expected a number, got {n_str}"))?;
    if n == 0 {
        return Err("error: swarm needs at least 1 agent".to_string());
    }
    if n > 20 {
        return Err("error: max 20 agents per swarm (caps at defaults)".to_string());
    }

    let rest = &args[3..];
    let mut task_parts: Vec<String> = Vec::new();
    let mut role_value: Option<String> = None;
    let mut delivery: Option<String> = None;
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--delivery" => {
                i += 1;
                if i < rest.len() {
                    delivery = Some(rest[i].clone());
                }
            }
            // `--roles` takes exactly one comma-separated value, so flags
            // may come before or after the task:
            // `swarm 5 --roles a,b "task"` and `swarm 5 "task" --roles a,b`
            // both parse. A multi-token form would be ambiguous with the
            // task, so it is not accepted.
            "--roles" => {
                i += 1;
                match rest.get(i) {
                    Some(v) if !v.starts_with("--") => {
                        role_value = Some(v.clone());
                    }
                    _ => {
                        return Err(
                            "error: --roles needs one comma-separated role list, e.g. --roles researcher,critic".to_string(),
                        );
                    }
                }
            }
            s if s.starts_with("--") => {
                return Err(format!("unknown flag: {s}"));
            }
            _ => {
                task_parts.push(rest[i].clone());
            }
        }
        i += 1;
    }

    let task = task_parts.join(" ");
    if task.is_empty() {
        return Err("error: need a task for the swarm".to_string());
    }

    let roles = if role_value.is_none() {
        (1..=n).map(|k| format!("agent-{k}")).collect()
    } else {
        let parsed: Vec<String> = role_value
            .as_deref()
            .unwrap_or("")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if parsed.len() != n {
            return Err(format!(
                "error: --roles gave {} role(s) but the swarm has {n} agent(s); pass one role per agent",
                parsed.len()
            ));
        }
        parsed
    };

    Ok(SpawnSpec {
        n,
        task,
        roles,
        delivery,
    })
}

/// Resolve each role against the config's profile registry. A role with no
/// matching profile is an `Err` naming the missing profile - never a
/// silent fallback to a default agent (the same hard-error convention the
/// TUI uses for a named `--agent`).
pub fn resolve_swarm_profiles(
    data_dir: &Path,
    roles: &[String],
) -> Result<Vec<EffectiveProfile>, String> {
    let cfg = crate::config::Config::load_or_report(data_dir);
    let mut out = Vec::with_capacity(roles.len());
    for role in roles {
        let Some(c) = cfg.as_ref() else {
            return Err(format!(
                "swarm role {role:?}: no config found; run `pantheon setup` and declare [agents.{role}]"
            ));
        };
        let effective = c
            .resolve_profile(Some(role))
            .map_err(|e| format!("swarm role {role:?}: {e}"))?;
        let Some(effective) = effective else {
            return Err(format!(
                "swarm role {role:?}: not declared in config; add [agents.{role}]"
            ));
        };
        out.push(effective);
    }
    Ok(out)
}

/// Run the whole swarm: write the manifest as `"running"`, drive each
/// agent's turn sequentially via `run_turn(profile, run_id, task)`, then
/// update the manifest to the aggregate status and return the outcomes.
/// Never calls `process::exit`.
pub fn run_swarm_spawn(
    data_dir: &Path,
    spec: &SpawnSpec,
    profiles: &[EffectiveProfile],
    run_turn: &dyn Fn(&EffectiveProfile, &str, &str) -> Result<String, String>,
) -> Result<Vec<AgentOutcome>, String> {
    if profiles.len() != spec.n {
        return Err(format!(
            "internal error: {} profiles resolved for {} agents",
            profiles.len(),
            spec.n
        ));
    }
    let swarm_id = format!(
        "swarm-{}",
        pantheon_runtime::new_run_id()
            .strip_prefix("run_")
            .unwrap_or("unknown")
    );
    let created_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let agents: Vec<SwarmAgent> = (0..spec.n)
        .map(|i| {
            let role = &spec.roles[i];
            SwarmAgent {
                label: format!("{swarm_id}:{role}:{}", i + 1),
                run_id: pantheon_runtime::new_run_id(),
                role: role.clone(),
                status: "running".to_string(),
            }
        })
        .collect();
    let mut rec = SwarmRecord {
        swarm_id: swarm_id.clone(),
        task: spec.task.clone(),
        roles: spec.roles.clone(),
        agents,
        delivery: spec.delivery.clone(),
        created_ms,
        status: "running".to_string(),
    };
    write_swarm_manifest(data_dir, &rec)?;

    let mut outcomes = Vec::with_capacity(spec.n);
    for (agent, profile) in rec.agents.iter().zip(profiles.iter()) {
        let result = run_turn(profile, &agent.run_id, &spec.task);
        outcomes.push(AgentOutcome {
            label: agent.label.clone(),
            role: agent.role.clone(),
            run_id: agent.run_id.clone(),
            result,
        });
    }

    let status = swarm_status_of(&outcomes);
    for (agent, outcome) in rec.agents.iter_mut().zip(outcomes.iter()) {
        agent.status = if outcome.result.is_ok() {
            "ok"
        } else {
            "error"
        }
        .to_string();
    }
    rec.status = status.to_string();
    write_swarm_manifest(data_dir, &rec)?;
    Ok(outcomes)
}

/// Render the aggregate report: each agent's result under a
/// `=== <label> (<role>) ===` header. A failing agent's error prints
/// here verbatim - it is never hidden.
pub fn render_swarm_report(outcomes: &[AgentOutcome]) -> String {
    let mut s = String::new();
    for o in outcomes {
        s.push_str(&format!("=== {} ({}) ===\n", o.label, o.role));
        match &o.result {
            Ok(text) => {
                s.push_str(text);
                if !text.ends_with('\n') {
                    s.push('\n');
                }
            }
            Err(e) => {
                s.push_str(&format!("error: {e}\n"));
            }
        }
    }
    s
}

/// Aggregate outcome of the swarm: `"complete"` (all ok), `"partial"`
/// (some failed), `"failed"` (all failed).
pub fn swarm_status_of(outcomes: &[AgentOutcome]) -> &'static str {
    let ok = outcomes.iter().filter(|o| o.result.is_ok()).count();
    if !outcomes.is_empty() && ok == outcomes.len() {
        "complete"
    } else if ok == 0 {
        "failed"
    } else {
        "partial"
    }
}

/// Drive one real headless turn for a swarm agent: fresh `Session` with the
/// profile's policy preset, the agent attached for identity, `Build` mode
/// (a CLI spawn has no parent session to inherit from), then `chat_turn`.
/// Mirrors `run_delivered_task`'s session construction.
fn swarm_turn(
    data_dir: &Path,
    profile: &EffectiveProfile,
    run_id: &str,
    task: &str,
) -> Result<String, String> {
    let file_cfg = crate::config::Config::load_or_report(data_dir);
    let model_policy = crate::config::build_model_policy(file_cfg.as_ref(), None, None);
    let policy = pantheon_runtime::session::policy_for_preset(&profile.policy.value)
        .map_err(|e| e.to_string())?;
    let secrets = crate::config::chat_secrets(file_cfg.as_ref());
    let session = pantheon_runtime::session::Session::new(
        data_dir.to_path_buf(),
        policy,
        model_policy,
        secrets,
    )
    .map_err(|e| format!("open session: {e}"))?;
    crate::config::apply_tool_enablement(&session, file_cfg.as_ref());
    // The profile resolved again here: resolve_swarm_profiles already
    // validated every role, so a config that vanished in between is a
    // hard error rather than an anonymous turn.
    let cfg = file_cfg.ok_or_else(|| "no config found to resolve the agent profile".to_string())?;
    let reg = cfg
        .profile_registry()
        .map_err(pantheon_runtime::profile_err)
        .map_err(|e| e.to_string())?;
    let agent = pantheon_runtime::AgentRuntime::new(
        session.supervisor.clone(),
        reg,
        profile.clone(),
        data_dir,
    )
    .map_err(|e| e.to_string())?;
    session
        .with_agent(agent)
        .map_err(|e| format!("attach agent: {e}"))?;
    session.set_mode(pantheon_api::mode::AgentMode::Build);
    let outcome = session
        .chat_turn(run_id, "", task)
        .map_err(|e| format!("run {run_id}: {e}"))?;
    Ok(crate::terminal::outcome_text(&outcome))
}

pub fn cmd_swarm(args: &[String], data_dir: &Path) {
    if args.len() < 3 {
        eprintln!(
            "usage: pantheon swarm <N> \"<task>\" [--roles a,b] [--delivery telegram|discord]"
        );
        eprintln!("       pantheon swarm status [<swarm_id>]");
        eprintln!("       pantheon swarm list");
        std::process::exit(2);
    }

    match args[2].as_str() {
        "status" | "list" => {
            handle_swarm_query(&args[2..], data_dir);
        }
        _ => {
            let spec = match parse_swarm_spawn(args) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(2);
                }
            };
            let delivery = spec.delivery.as_deref().unwrap_or("session");
            if !matches!(delivery, "session" | "telegram" | "discord") {
                eprintln!(
                    "unknown delivery target '{delivery}'; use session, telegram, or discord"
                );
                std::process::exit(2);
            }
            let profiles = match resolve_swarm_profiles(data_dir, &spec.roles) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(2);
                }
            };
            let outcomes =
                match run_swarm_spawn(data_dir, &spec, &profiles, &|profile, run_id, task| {
                    swarm_turn(data_dir, profile, run_id, task)
                }) {
                    Ok(o) => o,
                    Err(e) => {
                        eprintln!("{e}");
                        std::process::exit(1);
                    }
                };
            let report = render_swarm_report(&outcomes);
            print!("{report}");
            if delivery != "session" {
                if let Err(e) =
                    crate::gateway::enqueue_outbound(data_dir, delivery, &report, delivery)
                {
                    eprintln!("queue for {delivery}: {e}");
                    std::process::exit(1);
                }
                println!("queued aggregate report for {delivery}");
            }
            let ok = outcomes.iter().filter(|o| o.result.is_ok()).count();
            let status = swarm_status_of(&outcomes);
            let id = outcomes
                .first()
                .and_then(|o| o.label.split(':').next())
                .unwrap_or("?");
            println!("swarm {id}: {status} ({ok}/{} agents ok)", outcomes.len());
            std::process::exit(if status == "complete" { 0 } else { 1 });
        }
    }
}

fn handle_swarm_query(args: &[String], data_dir: &Path) {
    let sup = match Supervisor::open(data_dir.to_path_buf()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("open runtime: {e}");
            std::process::exit(1);
        }
    };
    match args[0].as_str() {
        "list" => {
            let swarms = load_all_swarms(data_dir);
            if swarms.is_empty() {
                println!("no swarms recorded");
                return;
            }
            for s in &swarms {
                println!(
                    "{}  agents: {}  task: {}",
                    s.swarm_id,
                    s.agents.len(),
                    s.task.chars().take(80).collect::<String>()
                );
            }
        }
        "status" => {
            if args.len() < 2 {
                eprintln!("usage: pantheon swarm status <swarm_id>");
                std::process::exit(2);
            }
            let id = &args[1];
            let rec = match resolve_swarm(data_dir, id) {
                Ok(Some(r)) => Some(r),
                Ok(None) => reconstruct_from_ledger(&sup, id),
                Err(amb) => {
                    eprintln!("{amb}");
                    std::process::exit(1);
                }
            };
            let Some(rec) = rec else {
                eprintln!("unknown swarm '{id}' (see `pantheon swarm list`)");
                std::process::exit(1);
            };
            println!("SWARM {}", rec.swarm_id);
            println!("  task: {}", rec.task);
            println!("  roles: {}", rec.roles.join(", "));
            if let Some(d) = &rec.delivery {
                println!("  delivery: {d}");
            }
            println!("  agents: {}", rec.agents.len());
            for a in &rec.agents {
                let status = sup
                    .ledger_status(&a.run_id)
                    .unwrap_or(None)
                    .unwrap_or_else(|| "unknown".to_string());
                println!("    {}  {}  {}  {status}", a.label, a.run_id, a.role);
            }
        }
        _ => {
            // Guarded by cmd_swarm's status|list allow-list; fail loud
            // (not unreachable) so a new subcommand can't silently no-op.
            eprintln!("usage: pantheon swarm status [<swarm_id>]");
            eprintln!("       pantheon swarm list");
            std::process::exit(2);
        }
    }
}

/// Fallback for swarms spawned before manifests existed: scan the ledger
/// for `AgentSpawned` agents shaped `{swarm_id}:{role}:{i}`.
fn reconstruct_from_ledger(sup: &Supervisor, id: &str) -> Option<SwarmRecord> {
    let runs = sup.ledger_list_runs(2000).ok()?;
    let mut agents: Vec<SwarmAgent> = Vec::new();
    let mut task = String::new();
    for r in &runs {
        let entries = sup.replay(&r.0).ok()?;
        for e in &entries {
            match &e.event {
                pantheon_api::events::Event::AgentSpawned { agent, run_id, .. }
                    if agent.starts_with(id) =>
                {
                    let role = agent.split(':').nth(1).unwrap_or("agent").to_string();
                    if !agents.iter().any(|a: &SwarmAgent| a.run_id == *run_id) {
                        agents.push(SwarmAgent {
                            label: agent.clone(),
                            run_id: run_id.clone(),
                            role,
                            status: String::new(),
                        });
                    }
                }
                pantheon_api::events::Event::RunProgress { detail, .. }
                    if task.is_empty() && detail.contains(id) =>
                {
                    // Detail looks like "[swarm <id>] agent i (role) assigned: <task>".
                    if let Some(t) = detail.split("assigned: ").nth(1) {
                        task = t.to_string();
                    }
                }
                _ => {}
            }
        }
    }
    if agents.is_empty() {
        return None;
    }
    agents.sort_by(|a, b| a.label.cmp(&b.label));
    let swarm_id = agents[0].label.split(':').next().unwrap_or(id).to_string();
    let roles = agents.iter().map(|a| a.role.clone()).collect();
    Some(SwarmRecord {
        swarm_id,
        task,
        roles,
        agents,
        delivery: None,
        created_ms: 0,
        status: String::new(),
    })
}
