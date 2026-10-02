//! `pantheon nightly` and the TUI `/nightly` command.
//!
//! The unified self-improvement pass: one ledger scan feeds signal
//! proposals (skill/persona/memory lesson) and memory promotion, with
//! eval-gating, replay validation (strict improvement on held-out
//! tasks), human approval for skills/personas, and a JSONL audit trail.
//!
//! All LLM-backed steps resolve through auxiliary slots - the pass's own
//! `[nightly.model]` pin when configured, else proposal refinement
//! through `AuxiliaryKind::Reflection` and memory distillation through
//! `AuxiliaryKind::Consolidation` - never the chat model directly.
//! `run_one_pass` builds the model policy from the on-disk config, so the
//! CLI, the TUI worker thread, and the scheduler share one routing path.
//!
//! The pass is **off by default** and does nothing unless enabled: an
//! explicit `enabled` flag wins, otherwise a `[nightly.model]` pin (or
//! the `PANTHEON_NIGHTLY_PROVIDER` / `PANTHEON_NIGHTLY_MODEL` env
//! overrides) implies on - see [`pantheon_api::config::nightly_enabled`].
//! With no `[nightly]` section, legacy `[reflect]` / `[consolidation]`
//! With no `[nightly]` section, legacy `[reflect]` / `[consolidation]`
//! opt-ins are honored as a deprecated fallback.
//!
//! The old `pantheon reflect` / `pantheon consolidate` commands remain as
//! thin compatibility shims over this module.

use pantheon_api::capability::Policy;
use pantheon_api::model::{AuxiliaryKind, DefaultModel};
use pantheon_nightly::{
    decide as nightly_decide, load_escalated, load_pending, run_pass, CompositeReplayRunner,
    Escalation, NightlyDeps, NightlyEvent, NightlyLlm, NightlyState, PassResult, Proposal,
    RepairTargets, ReplayRunner, ReplayStore, ReplayTask, SubprocessEvalRunner,
};
use std::path::Path;
use std::time::Duration;

/// Run one bounded nightly pass.
///
/// Builds the model policy from the on-disk config (so any LLM step
/// resolves through the Reflection/Consolidation aux slots), eval-gates
/// skill/persona proposals with the subprocess runner, replay-gates them
/// against the held-out task set, auto-applies memory lessons, queues
/// the rest for approval, and audits everything.
pub fn run_one_pass(data_dir: &Path, dry_run: bool) -> Result<PassResult, String> {
    let file_cfg = crate::config::Config::load_or_report(data_dir);
    // Master switch: the pipeline and its repair loop do nothing unless
    // the pass is enabled (explicit flag, model pin, or legacy opt-in).
    // This is the single entry every trigger funnels through - the CLI,
    // the TUI worker thread, and the scheduler - so one gate covers all.
    if !crate::config::nightly_pass_enabled(file_cfg.as_ref()) {
        return Err("nightly pass is disabled - enable it with a [nightly.model] pin, `enabled = true` under [nightly], or `/nightly on`"
            .to_string());
    }
    let mut cfg = crate::config::nightly_config(file_cfg.as_ref(), data_dir);
    cfg.dry_run = dry_run;
    let policy = crate::config::build_model_policy(file_cfg.as_ref(), None, None);

    let ledger = pantheon_storage::Ledger::open(&data_dir.join("ledger.db"))
        .map_err(|e| format!("opening ledger: {e}"))?;
    let backend = pantheon_memory::open_selected(data_dir)
        .map_err(|e| format!("opening memory backend: {e}"))?;
    let mem_policy = Policy::coder_with_memory();

    let eval_runner = SubprocessEvalRunner::new(cfg.eval_timeout, cfg.max_evals);
    // One runner for every replay strategy: tasks with their own exec
    // spec run on the built-in headless runner; the rest use the
    // configured headless-agent command, or fail loudly when none is
    // set (the gate rejects - "couldn't measure" is never a pass).
    let replay_runner =
        CompositeReplayRunner::new(cfg.replay_command.clone(), Duration::from_secs(300));
    let replay_runner: &dyn ReplayRunner = &replay_runner;

    // One LLM client per aux slot, each slot-guarded: refinement calls
    // can only hit the Reflection target, distillation only the
    // Consolidation target.
    let secrets = crate::config::chat_secrets(file_cfg.as_ref());
    let api_key = [
        "PANTHEON_NIGHTLY_API_KEY",
        "PANTHEON_CONSOLIDATION_API_KEY",
        "PANTHEON_API_KEY",
    ]
    .iter()
    .find_map(|env| secrets.inject(env).ok().flatten());

    struct Router {
        refine: pantheon_providers::DistillClient,
        distill: pantheon_providers::DistillClient,
        repair: pantheon_providers::DistillClient,
    }
    impl NightlyLlm for Router {
        fn refine_proposal(
            &self,
            model: &pantheon_api::model::AuxiliaryModel,
            draft: &Proposal,
        ) -> Result<String, String> {
            self.refine.refine_proposal(model, draft)
        }
        fn distill_memories(
            &self,
            model: &pantheon_api::model::AuxiliaryModel,
            texts: &[String],
        ) -> Result<Vec<String>, String> {
            self.distill.distill_memories(model, texts)
        }
        fn diagnose_repair(
            &self,
            model: &pantheon_api::model::AuxiliaryModel,
            prompt: &str,
        ) -> Result<String, String> {
            // The Repair slot, never Reflection/Consolidation: the
            // client's slot guard refuses any model that does not match
            // the Repair aux target it was built for.
            self.repair.diagnose_repair(model, prompt)
        }
    }
    fn client_for(
        policy: &pantheon_api::model::ModelPolicy,
        kind: AuxiliaryKind,
        api_key: Option<pantheon_secrets::SecretValue>,
    ) -> pantheon_providers::DistillClient {
        let aux = policy.auxiliary(&kind);
        let timeout_secs = aux.map(|a| a.timeout_secs).unwrap_or(60);
        let target: DefaultModel = aux
            .map(|a| DefaultModel {
                provider: a.provider.clone(),
                model: a.model.clone(),
            })
            .unwrap_or_else(|| policy.default.clone());
        pantheon_providers::DistillClient::new(target, api_key).with_timeout_secs(timeout_secs)
    }
    let router = Router {
        refine: client_for(&policy, AuxiliaryKind::Reflection, api_key.clone()),
        distill: client_for(&policy, AuxiliaryKind::Consolidation, api_key.clone()),
        repair: client_for(&policy, AuxiliaryKind::Repair, api_key),
    };

    // Repair targets: production adapters over the MCP manager, the
    // schedule store + run history, and the tool registry. Declared
    // before `deps` so the `&mut` borrows in `RepairTargets` live long
    // enough. The manager is configured, not connected: no servers are
    // launched until the repair phase (or a tool probe) needs them.
    let mcp_manager = std::sync::Arc::new(crate::mcp::manager_for(data_dir));
    let mut mcp_adapter =
        crate::nightly_repair::McpRepairAdapter::new(mcp_manager.clone(), data_dir.to_path_buf());
    let mut sched_adapter =
        crate::nightly_repair::ScheduleRepairAdapter::new(data_dir.to_path_buf());
    let mut tool_adapter = crate::nightly_repair::ToolRepairAdapter::new(
        data_dir.to_path_buf(),
        cfg.tool_probe_allowlist.clone(),
        Some(mcp_manager),
    );

    let mut deps = NightlyDeps {
        ledger: &ledger,
        backend: backend.as_ref(),
        capability_policy: &mem_policy,
        model_policy: &policy,
        eval_runner: &eval_runner,
        replay_runner,
        llm: Some(&router),
        repair: Some(RepairTargets {
            mcp: Some(&mut mcp_adapter),
            schedule: Some(&mut sched_adapter),
            tools: Some(&mut tool_adapter),
        }),
    };
    run_pass(&cfg, &mut deps).map_err(|e| format!("nightly pass failed: {}: {}", e.code, e.cause))
}

/// Whether the nightly pass is enabled in config - the master switch.
/// Explicit `enabled` wins; absent, the `[nightly.model]` pin (or the
/// `PANTHEON_NIGHTLY_*` env overrides) implies on; with no `[nightly]`
/// section the legacy `[reflect]` / `[consolidation]` opt-ins are
/// honored as a deprecated fallback. Off by default.
pub fn nightly_enabled(data_dir: &Path) -> bool {
    let file_cfg = crate::config::Config::load_or_report(data_dir);
    crate::config::nightly_pass_enabled(file_cfg.as_ref())
}

/// Persist the nightly LLM toggle to the `[nightly]` config section.
/// Creates the section when absent; leaves every other key untouched.
/// Writes the explicit `Some(enabled)` flag - explicit always wins over
/// the model-pin rule (see [`pantheon_api::config::nightly_enabled`]).
pub fn persist_nightly_enabled(data_dir: &Path, enabled: bool) -> Result<(), String> {
    let mut cfg = crate::config::Config::load_or_report(data_dir).unwrap_or_default();
    let mut section = cfg.nightly.clone().unwrap_or_default();
    section.enabled = Some(enabled);
    cfg.nightly = Some(section);
    cfg.save(data_dir)
        .map_err(|e| format!("save config: {}", e.cause))
}

/// Current toggle state: `(pass_enabled, auto_turns)`.
pub fn nightly_state(data_dir: &Path) -> (bool, u32) {
    let cfg = crate::config::Config::load_or_report(data_dir);
    (
        nightly_enabled(data_dir),
        crate::config::nightly_auto_turns(cfg.as_ref()),
    )
}

/// Whether the on-disk config carries a `[nightly.model]` pin.
pub fn nightly_model_pin(data_dir: &Path) -> bool {
    crate::config::Config::load_or_report(data_dir)
        .as_ref()
        .and_then(|c| c.nightly.as_ref())
        .is_some_and(pantheon_api::config::nightly_model_pin_present)
}

/// Whether the nightly pass has any model pin in play: the
/// `[nightly.model]` table or the `PANTHEON_NIGHTLY_PROVIDER` /
/// `PANTHEON_NIGHTLY_MODEL` env overrides. Mirrors the pin half of
/// [`pantheon_api::config::nightly_enabled`] - used to decide whether
/// `/nightly on` should print pin guidance.
pub fn nightly_pin_present(data_dir: &Path) -> bool {
    nightly_model_pin(data_dir) || pantheon_api::config::nightly_env_pin_present()
}

/// Guidance for pinning a model to the nightly pass, printed by
/// `/nightly on` / `pantheon nightly on` when no `[nightly.model]` pin
/// is configured. An explicit `enabled = true` turns the pass on, but
/// without a pin the LLM steps resolve through the Reflection /
/// Consolidation auxiliary slots (chat-model fallback) - the guidance
/// shows how to pin a dedicated model instead.
pub fn pin_guidance() -> String {
    "note: no [nightly.model] pin - LLM steps use the Reflection/Consolidation aux slots.\n\
     to pin a model for the nightly pass:\n\
     \n  [nightly.model]\n  provider = \"openai\"\n  model = \"gpt-4o-mini\"\n  api_key_env = \"OPENAI_API_KEY\"\n\
     \n(`api_key_env` names the env var holding the key, never the key itself.\n\
     `provider = \"default\"` inherits [model].\n\
     Alternatively export PANTHEON_NIGHTLY_PROVIDER / PANTHEON_NIGHTLY_MODEL.)"
        .to_string()
}

/// Next scheduled nightly run, if any: the earliest next fire among the
/// non-paused `pantheon schedule nightly` jobs (`__pantheon_nightly__`
/// task marker). Rendered as an RFC 3339 UTC timestamp. `None` when no
/// nightly job is scheduled (the loop can still fire on the auto-turns
/// trigger - see `/nightly status`).
pub fn next_nightly_run(data_dir: &Path) -> Option<String> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let mut next: Option<i64> = None;
    for stored in crate::schedule::load_jobs_public(data_dir) {
        if stored.job.task != crate::schedule::NIGHTLY_TASK_MARKER {
            continue;
        }
        if let Some(fire) = stored.job.next_fire_ms(now_ms, stored.last_run) {
            next = Some(next.map_or(fire, |n: i64| n.min(fire)));
        }
    }
    next.and_then(|ms| chrono::DateTime::from_timestamp_millis(ms).map(|t| t.to_rfc3339()))
}

/// Parsed `/nightly` subcommand (the TUI) / `pantheon nightly`
/// subcommand (the CLI). Pure over the argument string, so command
/// parsing is unit-testable without a TUI or a data dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NightlySub {
    /// Bare `/nightly`: run one manual pass now.
    Run { dry_run: bool },
    /// `/nightly on`: persist `enabled = true`.
    On,
    /// `/nightly off`: persist `enabled = false`.
    Off,
    /// `/nightly status`: the enable state, reason, pin, next run.
    Status,
    /// Anything else: usage error naming the input.
    Unknown(String),
}

impl NightlySub {
    pub fn parse(arg: &str) -> Self {
        match arg.trim() {
            "" => NightlySub::Run { dry_run: false },
            "--dry-run" | "-n" => NightlySub::Run { dry_run: true },
            "on" => NightlySub::On,
            "off" => NightlySub::Off,
            "status" => NightlySub::Status,
            other => NightlySub::Unknown(other.to_string()),
        }
    }
}

/// One-line status for `/nightly status` / `pantheon nightly status`.
pub fn status_line(data_dir: &Path) -> String {
    let cfg = crate::config::Config::load_or_report(data_dir);
    let (enabled, auto_turns) = nightly_state(data_dir);
    let loop_line = if auto_turns == 0 {
        "nightly pass: automatic passes disabled (auto_turns = 0)".to_string()
    } else {
        format!("nightly pass: automatic pass every {auto_turns} turns")
    };
    let reason = cfg
        .as_ref()
        .and_then(|c| c.nightly.as_ref())
        .map(pantheon_api::config::nightly_enabled_reason)
        .unwrap_or_else(|| {
            if enabled {
                "on via legacy [reflect]/[consolidation] (deprecated)"
            } else {
                // No `[nightly]` section: the default rule applies, so use
                // the same vocabulary as `nightly_enabled_reason`.
                "off (no flag, no model pin)"
            }
        });
    let state_line = if enabled {
        format!("nightly pass: on ({reason})")
    } else {
        format!(
            "nightly pass: off ({reason}) - enable with a [nightly.model] pin, `enabled = true`, or `/nightly on`"
        )
    };
    let schedule_line = match next_nightly_run(data_dir) {
        Some(next) => format!("next scheduled run: {next}"),
        None => "next scheduled run: none (`pantheon schedule nightly` to add one)".to_string(),
    };
    match NightlyState::load(data_dir) {
        Ok(state) if state.last_run_ms > 0 => format!(
            "{loop_line}\n{state_line}\n{schedule_line}\nlast pass: {} - {} proposed, {} applied, {} pending approval{}",
            state.last_run_ms,
            state.last_proposals,
            state.last_applied,
            state.last_pending,
            if state.last_dry_run { " (dry run)" } else { "" },
        ),
        _ => format!("{loop_line}\n{state_line}\n{schedule_line}\nno nightly passes yet"),
    }
}

/// Human summary of a finished pass for the CLI and the TUI status line.
pub fn summarize_pass(out: &PassResult) -> String {
    let mut s = format!(
        "nightly pass: {} proposed, {} lessons applied, {} awaiting approval",
        out.proposals.len(),
        out.applied,
        out.pending,
    );
    let mut rejected = 0;
    for p in &out.proposals {
        match p.status {
            pantheon_nightly::ProposalStatus::EvalFailed
            | pantheon_nightly::ProposalStatus::ReplayFailed => {
                rejected += 1;
                s.push_str(&format!("\n  rejected: {} ({:?})", p.title, p.status));
            }
            _ => {}
        }
    }
    if rejected > 0 {
        s = format!("{s}\n{rejected} rejected by gates");
    }
    s
}

/// Render pending proposals for approval review.
pub fn render_pending(pending: &[Proposal]) -> String {
    if pending.is_empty() {
        return "no pending nightly proposals".to_string();
    }
    let mut s = String::from("pending nightly proposals:");
    for p in pending {
        s.push_str(&format!(
            "\n\n[{}] {} ({})\n  {}\n  evals: {}\n  from runs: {}",
            p.id,
            p.title,
            p.kind_name(),
            p.body.lines().next().unwrap_or(""),
            if p.eval_tags.is_empty() {
                "none".to_string()
            } else {
                p.eval_tags.join(", ")
            },
            if p.provenance_runs.is_empty() {
                " - ".to_string()
            } else {
                p.provenance_runs.join(", ")
            },
        ));
    }
    s.push_str("\n\napprove with: pantheon nightly --approve <id>  (or y/n in the TUI)");
    s
}

/// Render escalated nightly proposals: one block per escalation, with
/// the escalation id, kind, title, readable escalation time, and the
/// reason the pass gave up on it.
pub fn render_escalations(esc: &[Escalation]) -> String {
    if esc.is_empty() {
        return "no escalated nightly proposals".to_string();
    }
    let mut s = String::from("escalated nightly proposals:");
    for e in esc {
        let when = chrono::DateTime::from_timestamp_millis(e.at_ms)
            .map(|t| t.to_rfc3339())
            .unwrap_or_else(|| e.at_ms.to_string());
        s.push_str(&format!(
            "\n\n[{}] {} ({})\n  attempts: {}\n  escalated: {}\n  {}",
            e.id, e.title, e.kind, e.attempts, when, e.reason
        ));
    }
    s
}

/// Read the audit log (most recent last).
pub fn read_audit(data_dir: &Path) -> Result<Vec<NightlyEvent>, String> {
    let path = pantheon_nightly::audit_path(data_dir);
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut events = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let e: NightlyEvent =
            serde_json::from_str(line).map_err(|e| format!("parse audit line {}: {e}", i + 1))?;
        events.push(e);
    }
    Ok(events)
}

/// Render the audit log as human-readable lines.
pub fn render_log(events: &[NightlyEvent]) -> String {
    if events.is_empty() {
        return "no nightly audit events".to_string();
    }
    events
        .iter()
        .map(|e| format!("{e:?}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Approve or deny a pending proposal by id. Returns whether the id was
/// found.
pub fn decide(data_dir: &Path, id: &str, approve: bool) -> Result<bool, String> {
    let file_cfg = crate::config::Config::load_or_report(data_dir);
    let cfg = crate::config::nightly_config(file_cfg.as_ref(), data_dir);
    nightly_decide(data_dir, &cfg, id, approve)
}

/// Manage the held-out replay task set: `add`, `list`, `remove`.
pub fn cmd_replay_tasks(args: &[String], data_dir: &Path) {
    let mut store = match ReplayStore::open(data_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    let sub = args.first().map(|s| s.as_str());
    match sub {
        Some("list") | None => {
            let tasks = store.list();
            if tasks.is_empty() {
                println!("no replay tasks configured");
                println!("add one with: pantheon nightly replay-tasks add --id <id> --name <name> --prompt <prompt> --check <tool_sequence|contains|judge> --arg <value> [--exec-cmd <cmd> --exec-arg <a>... --exec-env KEY=VALUE...]");
                return;
            }
            for t in tasks {
                let exec = t
                    .exec
                    .as_ref()
                    .map(|e| format!(" exec: {}", e.command))
                    .unwrap_or_else(|| " exec: none (needs [nightly] replay_command)".to_string());
                println!("- {}: {} [{:?}]{exec}", t.id, t.name, t.check);
            }
        }
        Some("add") => {
            let mut id = String::new();
            let mut name = String::new();
            let mut prompt = String::new();
            let mut check = String::new();
            let mut arg = String::new();
            let mut exec_cmd: Option<String> = None;
            let mut exec_args: Vec<String> = Vec::new();
            let mut exec_env: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "--id" => {
                        i += 1;
                        id = args.get(i).cloned().unwrap_or_default();
                    }
                    "--name" => {
                        i += 1;
                        name = args.get(i).cloned().unwrap_or_default();
                    }
                    "--prompt" => {
                        i += 1;
                        prompt = args.get(i).cloned().unwrap_or_default();
                    }
                    "--check" => {
                        i += 1;
                        check = args.get(i).cloned().unwrap_or_default();
                    }
                    "--arg" => {
                        i += 1;
                        if !arg.is_empty() {
                            arg.push(',');
                        }
                        arg.push_str(&args.get(i).cloned().unwrap_or_default());
                    }
                    // Self-contained execution for the built-in replay
                    // runner: `--exec-cmd sh --exec-arg -c --exec-arg
                    // "echo done"` runs the task headlessly; stdout is
                    // the scored transcript. Repeatable flags.
                    "--exec-cmd" => {
                        i += 1;
                        exec_cmd = args.get(i).cloned();
                    }
                    "--exec-arg" => {
                        i += 1;
                        exec_args.push(args.get(i).cloned().unwrap_or_default());
                    }
                    "--exec-env" => {
                        i += 1;
                        let kv = args.get(i).cloned().unwrap_or_default();
                        match kv.split_once('=') {
                            Some((k, v)) if !k.trim().is_empty() => {
                                exec_env.insert(k.to_string(), v.to_string());
                            }
                            _ => {
                                eprintln!("error: --exec-env needs KEY=VALUE");
                                std::process::exit(2);
                            }
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            let check = match check.as_str() {
                "tool_sequence" => pantheon_nightly::ReplayCheck::ToolSequence {
                    tools: arg
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                },
                "contains" => pantheon_nightly::ReplayCheck::Contains {
                    text: arg,
                    negate: false,
                },
                "judge" => pantheon_nightly::ReplayCheck::Judge { rubric: arg },
                _ => {
                    eprintln!("error: --check must be tool_sequence, contains, or judge");
                    std::process::exit(2);
                }
            };
            let task = ReplayTask {
                id: id.clone(),
                name,
                prompt,
                check,
                exec: exec_cmd.map(|command| pantheon_nightly::TaskExec {
                    command,
                    args: exec_args,
                    env: exec_env,
                }),
            };
            match store.save(task) {
                Ok(()) => println!("saved replay task '{id}'"),
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("remove") => {
            let id = args.get(1).cloned().unwrap_or_default();
            match store.delete(&id) {
                Ok(true) => println!("removed replay task '{id}'"),
                Ok(false) => {
                    eprintln!("error: no replay task '{id}'");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some(other) => {
            eprintln!("unknown replay-tasks subcommand: {other}");
            eprintln!("usage: pantheon nightly replay-tasks [list|add|remove]");
            std::process::exit(2);
        }
    }
}

/// `pantheon nightly --help`.
fn nightly_help() {
    eprintln!("usage: pantheon nightly [--dry-run] [on|off|status|log|pending|escalations|report|replay-tasks ...] [--approve ID] [--deny ID]");
    eprintln!(
        "  on | off                enable or disable the nightly pass (persists to [nightly])"
    );
    eprintln!("  status                   show the pass state and last run");
    eprintln!("  log                      show recent nightly history");
    eprintln!("  pending                  list proposals awaiting approval");
    eprintln!("  escalations              list escalated repairs");
    eprintln!("  report                   show the latest pass report");
    eprintln!("  replay-tasks [list|add|remove]   manage replayable tasks");
    eprintln!("  --dry-run                report what would happen without changing anything");
    eprintln!("  --approve ID | --deny ID  decide a pending proposal");
}

/// `pantheon nightly [--dry-run] [on|off|status|log|pending|escalations|report|replay-tasks ...] [--approve ID] [--deny ID]`
pub fn cmd_nightly(args: &[String], data_dir: &Path) {
    let mut dry_run = false;
    let mut approve: Option<String> = None;
    let mut deny: Option<String> = None;
    let mut sub: Option<&str> = None;
    let mut rest: Vec<String> = Vec::new();
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--help" | "-h" => {
                nightly_help();
                std::process::exit(0);
            }
            "--dry-run" => dry_run = true,
            "--approve" => {
                i += 1;
                if i < args.len() {
                    approve = Some(args[i].clone());
                }
            }
            "--deny" => {
                i += 1;
                if i < args.len() {
                    deny = Some(args[i].clone());
                }
            }
            s if sub.is_none() => sub = Some(s),
            s => rest.push(s.to_string()),
        }
        i += 1;
    }

    if let Some(id) = approve {
        match decide(data_dir, &id, true) {
            Ok(true) => println!("approved {id}"),
            Ok(false) => {
                eprintln!("error: no pending proposal '{id}'");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    if let Some(id) = deny {
        match decide(data_dir, &id, false) {
            Ok(true) => println!("denied {id}"),
            Ok(false) => {
                eprintln!("error: no pending proposal '{id}'");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    match sub {
        Some("on") => match persist_nightly_enabled(data_dir, true) {
            Ok(()) => {
                println!("nightly pass: on (persisted to [nightly])");
                if !nightly_pin_present(data_dir) {
                    println!("{}", pin_guidance());
                }
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        },
        Some("off") => match persist_nightly_enabled(data_dir, false) {
            Ok(()) => println!("nightly pass: off (persisted to [nightly])"),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        },
        Some("status") => println!("{}", status_line(data_dir)),
        Some("log") => match read_audit(data_dir) {
            Ok(events) => println!("{}", render_log(&events)),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        },
        Some("pending") => match load_pending(data_dir) {
            Ok(p) => println!("{}", render_pending(&p)),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        },
        Some("escalations") => match load_escalated(data_dir) {
            Ok(e) => println!("{}", render_escalations(&e)),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        },
        Some("replay-tasks") => cmd_replay_tasks(&rest, data_dir),
        Some("report") => {
            let path = data_dir.join("nightly").join("nightly-report.md");
            match std::fs::read_to_string(&path) {
                Ok(md) => println!("{md}"),
                Err(_) => {
                    eprintln!("no nightly report yet - run `pantheon nightly` first");
                    std::process::exit(1);
                }
            }
        }
        Some(other) => {
            eprintln!("unknown nightly subcommand: {other}");
            eprintln!("usage: pantheon nightly [--dry-run] [on|off|status|log|pending|escalations|report|replay-tasks ...] [--approve ID] [--deny ID]");
            std::process::exit(2);
        }
        None => {
            if dry_run {
                println!("dry run - nothing will be applied or queued; the pass is still audited and reported");
            }
            match run_one_pass(data_dir, dry_run) {
                Ok(out) => {
                    println!("{}", summarize_pass(&out));
                    let pending = load_pending(data_dir).unwrap_or_default();
                    if !pending.is_empty() {
                        println!();
                        println!("{}", render_pending(&pending));
                    }
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
    }
}
