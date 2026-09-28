//! `pantheon reflect` and `/reflect` — manual reflection passes, the
//! on/off toggle, status, history, and pending-proposal approvals.
//!
//! All LLM-backed reflection steps resolve through the
//! `AuxiliaryKind::Reflection` slot of the run's model policy — never the
//! chat model directly. `run_one_pass` builds that policy from the
//! on-disk config, so the CLI, the TUI worker thread, and the scheduler
//! share one routing path.

use std::path::Path;

/// Run one bounded reflection pass: build the model policy (so any LLM
/// step resolves through the Reflection aux slot), eval-gate
/// skill/persona proposals with the subprocess runner, auto-apply memory
/// lessons, persist pending proposals, and append audit records.
///
/// `dry_run` generates proposals without applying anything or writing
/// audit records. No `LlmRefiner` is wired in v1 — the deterministic
/// templates are the whole pipeline — but when one is, it receives the
/// resolved Reflection aux model, never the chat model.
pub fn run_one_pass(
    data_dir: &Path,
    dry_run: bool,
) -> Result<pantheon_reflect::PassOutput, String> {
    let file_cfg = crate::config::Config::load_or_report(data_dir);
    let policy = crate::config::build_model_policy(file_cfg.as_ref(), None, None);
    let cfg = crate::config::reflect_config(file_cfg.as_ref());
    let runner =
        pantheon_reflect::eval_gate::SubprocessEvalRunner::new(cfg.eval_timeout, cfg.max_evals);
    pantheon_reflect::run_pass(
        pantheon_reflect::PassInput {
            data_dir,
            config: cfg,
            lookback_ms: None,
            dry_run,
            model_policy: Some(&policy),
            // v1: no model-backed refiner. The seam exists and is
            // aux-routed; flipping `[reflect] enabled` today only
            // permits a refiner a future build wires in.
            llm: None,
        },
        &runner,
    )
    .map_err(|e| format!("reflection pass failed: {e}"))
}

/// Persist the reflection-loop toggle to the `[reflect]` config section.
/// Creates the section when absent; leaves every other key untouched.
pub fn persist_reflect_enabled(data_dir: &Path, enabled: bool) -> Result<(), String> {
    let mut cfg = crate::config::Config::load_or_report(data_dir).unwrap_or_default();
    let mut section = cfg
        .reflect
        .clone()
        .unwrap_or(crate::config::ReflectSection {
            enabled: false,
            auto_turns: crate::config::DEFAULT_REFLECT_AUTO_TURNS,
            max_proposals: crate::config::DEFAULT_REFLECT_MAX_PROPOSALS,
            provider: None,
            model: None,
            api_key_env: None,
        });
    section.enabled = enabled;
    cfg.reflect = Some(section);
    cfg.save(data_dir)
        .map_err(|e| format!("save config: {}", e.cause))
}

/// Current toggle state: `(enabled, auto_turns)`.
pub fn reflect_state(data_dir: &Path) -> (bool, u32) {
    let cfg = crate::config::Config::load_or_report(data_dir);
    match cfg.as_ref().and_then(|c| c.reflect.as_ref()) {
        Some(r) => (r.enabled, r.auto_turns),
        None => (false, crate::config::DEFAULT_REFLECT_AUTO_TURNS),
    }
}

/// One-line status for `/reflect status` / `pantheon reflect status`.
pub fn status_line(data_dir: &Path) -> String {
    let (enabled, auto_turns) = reflect_state(data_dir);
    let loop_line = if enabled {
        if auto_turns == 0 {
            "reflection loop: on (automatic passes disabled, auto_turns = 0)".to_string()
        } else {
            format!("reflection loop: on (automatic pass every {auto_turns} turns)")
        }
    } else {
        "reflection loop: off (manual passes only; /reflect on to enable)".to_string()
    };
    match pantheon_reflect::audit::last_run_summary(data_dir) {
        Some(summary) => format!("{loop_line}\n{summary}"),
        None => format!("{loop_line}\nno reflection passes yet"),
    }
}

/// Human summary of a finished pass for the CLI and the TUI status line.
pub fn summarize_pass(out: &pantheon_reflect::PassOutput) -> String {
    let mut s = format!(
        "reflection pass {}: {} proposed, {} lessons applied, {} awaiting approval, {} rejected by evals",
        out.pass_id,
        out.proposals.len(),
        out.applied.len(),
        out.pending.len(),
        out.rejected.len(),
    );
    for a in &out.applied {
        s.push_str(&format!("\n  applied: {}", a.describe()));
    }
    for (p, reason) in &out.rejected {
        s.push_str(&format!("\n  rejected: {} ({reason})", p.title));
    }
    s
}

/// Render pending proposals for approval review.
pub fn render_pending(pending: &[pantheon_reflect::PendingProposal]) -> String {
    if pending.is_empty() {
        return "no pending reflection proposals".to_string();
    }
    let mut s = String::from("pending reflection proposals:");
    for pp in pending {
        let p = &pp.proposal;
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
                "—".to_string()
            } else {
                p.provenance_runs.join(", ")
            },
        ));
    }
    s.push_str("\n\napprove with: pantheon reflect --approve <id>  (or y/n in the TUI)");
    s
}

/// `pantheon reflect [--dry-run] [on|off|status|log] [--approve ID] [--deny ID]`
pub fn cmd_reflect(args: &[String], data_dir: &Path) {
    let mut dry_run = false;
    let mut approve: Option<String> = None;
    let mut deny: Option<String> = None;
    let mut sub: Option<&str> = None;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
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
            _ => {}
        }
        i += 1;
    }

    if let Some(id) = approve {
        match pantheon_reflect::approve_pending(data_dir, &id, "cli") {
            Ok(outcome) => println!("approved {}: {}", id, outcome.describe()),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    if let Some(id) = deny {
        match pantheon_reflect::deny_pending(data_dir, &id, "cli") {
            Ok(()) => println!("denied {id}"),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    match sub {
        Some("on") => match persist_reflect_enabled(data_dir, true) {
            Ok(()) => println!("reflection loop: on"),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        },
        Some("off") => match persist_reflect_enabled(data_dir, false) {
            Ok(()) => println!("reflection loop: off"),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        },
        Some("status") => println!("{}", status_line(data_dir)),
        Some("log") => {
            let audit =
                pantheon_reflect::audit::ReflectAudit::open(&data_dir.join("reflect.jsonl"));
            match audit {
                Ok(a) => println!("{}", pantheon_reflect::audit::render_log(&a.read_all())),
                Err(e) => {
                    eprintln!("error: open audit log: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some("pending") => match pantheon_reflect::apply::load_pending(data_dir) {
            Ok(p) => println!("{}", render_pending(&p)),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        },
        Some(other) => {
            eprintln!("unknown reflect subcommand: {other}");
            eprintln!("usage: pantheon reflect [--dry-run] [on|off|status|log|pending] [--approve ID] [--deny ID]");
            std::process::exit(2);
        }
        None => {
            if dry_run {
                println!("dry run — nothing will be applied or recorded");
            }
            match run_one_pass(data_dir, dry_run) {
                Ok(out) => {
                    println!("{}", summarize_pass(&out));
                    if !out.pending.is_empty() {
                        println!();
                        println!("{}", render_pending(&out.pending));
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
