//! remote_group: swarm, swarm_remote, swarm_view, team_remote, plugin_remote in one file.

pub mod swarm {
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
    use std::path::Path;

    /// Shared with the dashboard orchestrator: both the CLI and the orchestrator
    /// read/write the same `<data_dir>/swarms/` dir. The CLI keeps its own
    /// `SwarmRecord` / `SwarmAgent` (field-identical to the orchestrator's
    /// [`pantheon_runtime::swarm_persist::SwarmManifest`]), but routes write and
    /// load through the shared helpers so the two cannot drift.
    pub use pantheon_runtime::swarm_persist::{load_all_manifests, swarms_dir, write_manifest};

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

    /// Persist (or overwrite) a CLI swarm manifest atomically. Delegates to the
    /// shared [`write_manifest`]: the CLI struct is field-identical to the
    /// orchestrator's shared manifest, so the one writer serves both.
    pub fn write_swarm_manifest(data_dir: &Path, rec: &SwarmRecord) -> Result<(), String> {
        let m = to_shared(rec);
        write_manifest(data_dir, &m)
    }

    /// Load every manifest in `swarms/` (both CLI- and orchestrator-written),
    /// newest-first. Delegates to the shared [`load_all_manifests`] so the two
    /// stores never drift, then re-parses as the CLI's `SwarmRecord`.
    pub fn load_all_swarms(data_dir: &Path) -> Vec<SwarmRecord> {
        let shared = load_all_manifests(data_dir);
        shared.into_iter().map(from_shared).collect()
    }

    /// Bridge to the shared manifest. `created_ms` is shared so both writers
    /// preserve ordering; `delivery` is the CLI's spawn-path field.
    fn to_shared(rec: &SwarmRecord) -> pantheon_runtime::swarm_persist::SwarmManifest {
        pantheon_runtime::swarm_persist::SwarmManifest {
            swarm_id: rec.swarm_id.clone(),
            task: rec.task.clone(),
            roles: rec.roles.clone(),
            agents: rec
                .agents
                .iter()
                .map(|a| pantheon_runtime::swarm_persist::SwarmAgentManifest {
                    label: a.label.clone(),
                    run_id: a.run_id.clone(),
                    role: a.role.clone(),
                    status: a.status.clone(),
                })
                .collect(),
            delivery: rec.delivery.clone(),
            created_ms: rec.created_ms,
            status: rec.status.clone(),
        }
    }

    /// Bridge from the shared manifest to the CLI `SwarmRecord`.
    fn from_shared(m: pantheon_runtime::swarm_persist::SwarmManifest) -> SwarmRecord {
        SwarmRecord {
            swarm_id: m.swarm_id,
            task: m.task,
            roles: m.roles,
            agents: m
                .agents
                .into_iter()
                .map(|a| SwarmAgent {
                    label: a.label,
                    run_id: a.run_id,
                    role: a.role,
                    status: a.status,
                })
                .collect(),
            delivery: m.delivery,
            created_ms: m.created_ms,
            status: m.status,
        }
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
                    match rest.get(i) {
                        Some(v) if !v.starts_with("--") => {
                            delivery = Some(v.clone());
                        }
                        _ => {
                            return Err(
                                "error: --delivery needs a target, e.g. --delivery telegram"
                                    .to_string(),
                            );
                        }
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
        crate::config::apply_session_config(&session, file_cfg.as_ref());
        // The profile resolved again here: resolve_swarm_profiles already
        // validated every role, so a config that vanished in between is a
        // hard error rather than an anonymous turn.
        let cfg =
            file_cfg.ok_or_else(|| "no config found to resolve the agent profile".to_string())?;
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

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn created_ms_survives_the_shared_manifest_bridge() {
            let rec = SwarmRecord {
                swarm_id: "sw_test".to_string(),
                task: "t".to_string(),
                roles: vec!["r".to_string()],
                agents: vec![SwarmAgent {
                    label: "sw_test:r:1".to_string(),
                    run_id: "run_1".to_string(),
                    role: "r".to_string(),
                    status: "ok".to_string(),
                }],
                delivery: Some("telegram".to_string()),
                created_ms: 42,
                status: "complete".to_string(),
            };
            let shared = to_shared(&rec);
            assert_eq!(shared.created_ms, 42);
            assert_eq!(shared.delivery.as_deref(), Some("telegram"));
            let back = from_shared(shared);
            assert_eq!(back.created_ms, 42);
            assert_eq!(back.delivery.as_deref(), Some("telegram"));
        }

        fn spawn_argv(words: &[&str]) -> Vec<String> {
            std::iter::once("pantheon".to_string())
                .chain(std::iter::once("swarm".to_string()))
                .chain(words.iter().map(|w| w.to_string()))
                .collect()
        }

        #[test]
        fn delivery_without_a_value_is_an_error_not_a_silent_default() {
            // The old parse left `delivery` as None when --delivery was
            // the last token, so `swarm 2 task --delivery` silently ran
            // with session delivery instead of the channel the operator
            // thought they had chosen.
            let err =
                parse_swarm_spawn(&spawn_argv(&["2", "do the thing", "--delivery"])).unwrap_err();
            assert!(err.contains("--delivery needs"), "{err}");
            let err =
                parse_swarm_spawn(&spawn_argv(&["2", "do the thing", "--delivery", "--roles"]))
                    .unwrap_err();
            assert!(err.contains("--delivery needs"), "{err}");
            let spec = parse_swarm_spawn(&spawn_argv(&[
                "2",
                "do the thing",
                "--delivery",
                "telegram",
            ]))
            .expect("valued --delivery must parse");
            assert_eq!(spec.delivery.as_deref(), Some("telegram"));
        }
    }
}

pub mod swarm_remote {
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
        let port = crate::config::server_port(server.as_ref());
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
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .http_status_as_error(false)
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .new_agent();
        let url = format!("{base}{path}");
        let mut req = match method {
            "POST" => agent.post(&url),
            "PUT" => agent.put(&url),
            _ => agent.get(&url).force_send_body(),
        };
        if let Ok(tok) = std::env::var("PANTHEON_SERVE_TOKEN") {
            if !tok.is_empty() {
                req = req.header(TOKEN_HEADER, tok.as_str());
            }
        }
        // Statuses arrive as responses; >=400 becomes the short-body error
        // exactly like the old Error::Status arm.
        let resp = match body {
            Some(b) => req
                .header("Content-Type", "application/json")
                .send(b.to_string().as_str()),
            None => req.send_empty(),
        }
        .map_err(|e| ApiErr::Unreachable(e.to_string()))?;
        let status = resp.status().as_u16();
        let text = resp.into_body().read_to_string().unwrap_or_default();
        if status >= 400 {
            let short: String = text.chars().take(200).collect();
            return Err(ApiErr::Status(status, short));
        }
        serde_json::from_str::<serde_json::Value>(&text)
            .map_err(|e| ApiErr::Status(status, format!("bad JSON: {e}")))
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
                "no swarms spawned this session (the swarm API has no list-all endpoint yet)"
                    .into(),
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

    /// The id for a `/swarm <id>` show invocation: exactly one token.
    /// Extra words used to be silently dropped (only the first token was
    /// shown), so a typo like `/swarm abc status` looked up `abc` and
    /// never said the rest was ignored.
    pub fn show_id(rest: &str) -> Result<&str, String> {
        let mut words = rest.split_whitespace();
        let id = words.next().unwrap_or("");
        if id.is_empty() {
            return Err("usage: /swarm <id>".into());
        }
        if words.next().is_some() {
            return Err("usage: /swarm <id> (one id; got extra words - see /swarm tree, /swarm new <task>, /swarm retry <id>)".to_string());
        }
        Ok(id)
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
        match show_id(rest) {
            Ok(id) => show_swarm(state, id),
            Err(msg) => state.add_status(msg),
        }
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
                "no swarms spawned this session (the swarm API has no list-all endpoint yet)"
                    .into(),
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
                None => {
                    state.add_status(format!("/{label}: no {field} declared for profile {name}"))
                }
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

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn show_id_accepts_one_id_and_rejects_extra_words() {
            assert_eq!(show_id("abc-123"), Ok("abc-123"));
            assert!(show_id("").is_err());
            let err = show_id("abc-123 status").unwrap_err();
            assert!(err.contains("one id"), "{err}");
            assert!(err.contains("/swarm tree"), "{err}");
        }
    }
}

pub mod swarm_view {
    //! `/swarm` - an honest snapshot of this session's delegation tree.
    //!
    //! The ledger records `AgentSpawned { run_id, agent }` on the parent run and
    //! `AgentCompleted { run_id, agent }` when a delegation finishes. What the
    //! ledger does NOT record is a child run id or an explicit parent link, so a
    //! true nested tree is not reconstructible: `/swarm` renders the root plus
    //! its direct delegations, capped at a fixed depth, and says so plainly
    //! rather than fabricating relationships.
    //!
    //! The empty case renders exactly: `no subagents - this run is working solo`.

    use pantheon_api::events::Event;
    use pantheon_storage::LedgerEntry;
    use std::sync::Arc;

    use crate::session::TuiState;
    use pantheon_runtime::session::Session;

    /// Maximum rendered delegations: the ledger records only direct
    // delegations, so this caps how many are listed, not a tree depth.
    pub const MAX_DELEGATIONS: usize = 5;

    /// One delegation seen in the ledger: profile name + completion state.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Delegation {
        pub agent: String,
        /// True once an `AgentCompleted` for this agent followed its spawn.
        pub done: bool,
    }

    /// Direct delegations of this run, in spawn order.
    ///
    /// An agent that was spawned more than once appears once, reflecting its
    /// latest state (done if any completion followed its latest spawn).
    pub fn delegations(entries: &[LedgerEntry]) -> Vec<Delegation> {
        let mut out: Vec<Delegation> = Vec::new();
        for e in entries {
            match &e.event {
                Event::AgentSpawned { agent, .. } => {
                    if let Some(d) = out.iter_mut().find(|d| d.agent == *agent) {
                        d.done = false;
                    } else {
                        out.push(Delegation {
                            agent: agent.clone(),
                            done: false,
                        });
                    }
                }
                Event::AgentCompleted { agent, .. } => {
                    if let Some(d) = out.iter_mut().find(|d| d.agent == *agent) {
                        d.done = true;
                    }
                }
                _ => {}
            }
        }
        out
    }

    /// Render the delegation tree as plain text, one node per line.
    ///
    /// The ledger carries no child run ids and no parent links, so only the
    /// root and its direct delegations are rendered; deeper nesting is not
    /// shown because it is not durably recorded.
    pub fn render_swarm(entries: &[LedgerEntry], run_id: &str) -> String {
        let dels = delegations(entries);
        if dels.is_empty() {
            return "no subagents - this run is working solo".to_string();
        }
        let short = run_id.chars().take(8).collect::<String>();
        let mut lines = vec![format!(
            "swarm {short} (running) - {} delegation{}",
            dels.len(),
            if dels.len() == 1 { "" } else { "s" }
        )];
        for (i, d) in dels.iter().enumerate().take(MAX_DELEGATIONS) {
            let last = i + 1 == dels.len().min(MAX_DELEGATIONS);
            let branch = if last { "└─" } else { "├─" };
            let status = if d.done { "done" } else { "running" };
            lines.push(format!("{branch} {} ({status})", d.agent));
        }
        if dels.len() > MAX_DELEGATIONS {
            lines.push(format!(
                "... {} more (delegation cap {})",
                dels.len() - MAX_DELEGATIONS,
                MAX_DELEGATIONS
            ));
        }
        lines.join("\n")
    }

    /// `/swarm` - show this session's delegation tree.
    pub fn cmd_swarm(state: &mut TuiState, session: &Arc<Session>) {
        let entries = match session.supervisor.replay(&state.session_id) {
            Ok(e) => e,
            Err(e) => {
                state.add_status(format!("ledger replay failed: {e}"));
                return;
            }
        };
        for line in render_swarm(&entries, &state.session_id).lines() {
            state.add_status(line.to_string());
        }
    }
}

pub mod team_remote {
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
        let port = crate::config::server_port(server.as_ref());
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
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(15)))
            .http_status_as_error(false)
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .new_agent();
        let url = format!("{base}{path}");
        let mut req = match method {
            "POST" => agent.post(&url),
            _ => agent.get(&url).force_send_body(),
        };
        if let Ok(tok) = std::env::var("PANTHEON_SERVE_TOKEN") {
            if !tok.is_empty() {
                req = req.header(TOKEN_HEADER, tok.as_str());
            }
        }
        // Statuses arrive as responses; >=400 becomes the short-body error
        // exactly like the old Error::Status arm.
        let resp = match body {
            Some(b) => req
                .header("Content-Type", "application/json")
                .send(b.to_string().as_str()),
            None => req.send_empty(),
        }
        .map_err(|e| ApiErr::Unreachable(e.to_string()))?;
        let status = resp.status().as_u16();
        let text = resp.into_body().read_to_string().unwrap_or_default();
        if status >= 400 {
            let short: String = text.chars().take(200).collect();
            return Err(ApiErr::Status(status, short));
        }
        serde_json::from_str::<serde_json::Value>(&text)
            .map_err(|e| ApiErr::Status(status, format!("bad JSON: {e}")))
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
}

pub mod plugin_remote {
    //! Remote plugin import + registry search against the dashboard
    //! HTTP API.
    //!
    //! Plugin install/approve/disable already exist as local `pantheon
    //! plugins ...` CLI verbs (`terminal.rs`) and as dashboard endpoints
    //! (`pantheon-dashboard/src/plugins.rs`), but the interactive TUI had no
    //! `/plugins` slash command at all - only the splash footer's hint named
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
        let port = crate::config::server_port(server.as_ref());
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
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .http_status_as_error(false)
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .new_agent();
        let url = format!("{base}{path}");
        let mut req = match method {
            "POST" => agent.post(&url),
            _ => agent.get(&url).force_send_body(),
        };
        if let Ok(tok) = std::env::var("PANTHEON_SERVE_TOKEN") {
            if !tok.is_empty() {
                req = req.header(TOKEN_HEADER, tok.as_str());
            }
        }
        // Statuses arrive as responses; >=400 becomes the short-body error
        // exactly like the old Error::Status arm.
        let resp = match body {
            Some(b) => req
                .header("Content-Type", "application/json")
                .send(b.to_string().as_str()),
            None => req.send_empty(),
        }
        .map_err(|e| ApiErr::Unreachable(e.to_string()))?;
        let status = resp.status().as_u16();
        let text = resp.into_body().read_to_string().unwrap_or_default();
        if status >= 400 {
            let short: String = text.chars().take(200).collect();
            return Err(ApiErr::Status(status, short));
        }
        serde_json::from_str::<serde_json::Value>(&text)
            .map_err(|e| ApiErr::Status(status, format!("bad JSON: {e}")))
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
                "{what}: dashboard unreachable ({t}) - is `pantheon serve` running?"
            )),
            ApiErr::Status(404, _) => state.add_status(format!(
                "{what}: endpoint not found (404) - the dashboard plugin API has not landed yet"
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
    /// like a GitHub URL - there is no `{spec}` field on the endpoint.
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
        // "Staged", not "imported": the first line used to claim the plugin
        // was imported even for a malicious scan verdict, before any
        // approval. Approval still blocks server-side, but the wording must
        // not imply a quarantined plugin is live.
        lines.push(format!(
            "staged {name} v{version} ({kind}), pending scan/approval"
        ));
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
                lines.push("scan: SUSPICIOUS (amber) - review before approving".to_string());
                list_findings(report, &mut lines);
            }
            "malicious" => {
                lines.push("scan: MALICIOUS (red) - approval is blocked".to_string());
                list_findings(report, &mut lines);
                lines.push("this plugin is quarantined; it cannot be approved or run".to_string());
            }
            other => {
                lines.push(format!(
                    "scan verdict: {other} (unrecognized - treat with care)"
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
                "approval required before it can run - use `pantheon plugins approve <name>`"
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
            lines.push(format!(" - {}", finding_text(f)));
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
                " - ".to_string()
            } else {
                desc
            };
            lines.push(format!("{}. {slug} - {name} v{version}: {desc}", i + 1));
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
    /// `GET /api/plugins` list shape likewise has no verdict field - per
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

        /// P1 #3: `POST /api/plugins/import` reads only `{url}` - the server
        /// resolves `clawhub:<slug>` itself (`plugin_import::parse_source_spec`)
        ///   so the TUI must send the slug as `url`. Sending `{spec}` made the
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
}
