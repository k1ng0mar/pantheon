//! `pantheon consolidate` and the TUI `/consolidate` command.
//!
//! Manual consolidation passes over the session ledger: stage repeated
//! facts from closed sessions, weigh them (merging phrasing via the
//! consolidation distiller when enabled), and promote the durable ones
//! into long-term memory. Dry runs stage and weigh but never write.
//!
//! All LLM-backed steps resolve through the
//! `AuxiliaryKind::Consolidation` slot of the run's model policy —
//! never the chat model directly. `run_one_pass` builds that policy from
//! the on-disk config, so the CLI, the TUI worker thread, and the
//! scheduler share one routing path. With `[consolidation] enabled =
//! false` the pass still runs its deterministic stages; only the LLM
//! distill is gated off.
//!
//! `cmd_consolidate` is the `pantheon consolidate` entrypoint.
//! `run_one_pass` / `status_line` are the testable seams — the TUI and the
//! scheduler call them directly.

use pantheon_api::capability::Policy;
use pantheon_api::model::{AuxiliaryKind, DefaultModel};
use pantheon_consolidate::{now_ms, run_pass, PassInput};

/// Run one bounded pass: stage → weigh → promote.
///
/// With `dry_run = true` nothing is promoted and no state is written;
/// the stage/weigh markdown reports are still written so the operator can
/// see what the pass *would* do.
pub fn run_one_pass(
    data_dir: &std::path::Path,
    dry_run: bool,
) -> Result<pantheon_consolidate::ConsolidationReport, String> {
    let file_cfg = crate::config::Config::load_or_report(data_dir);
    let cfg = crate::config::consolidation_config(file_cfg.as_ref());
    // The Consolidation aux slot lives in the same policy the CLI builds
    // for every run: `config::auxiliaries()` appends it.
    let policy = crate::config::build_model_policy(file_cfg.as_ref(), None, None);
    // Prefer the consolidation-specific key; fall back to the shared one.
    let secrets = crate::config::chat_secrets(file_cfg.as_ref());
    let key = secrets
        .inject("PANTHEON_CONSOLIDATION_API_KEY")
        .ok()
        .flatten()
        .or_else(|| secrets.inject("PANTHEON_API_KEY").ok().flatten());
    let ledger = pantheon_storage::Ledger::open(&data_dir.join("ledger.db"))
        .map_err(|e| format!("opening ledger: {e}"))?;
    let backend = pantheon_memory::open_selected(data_dir)
        .map_err(|e| format!("opening memory backend: {e}"))?;
    let mem_policy = Policy::coder_with_memory();
    // The distill client targets the resolved Consolidation auxiliary
    // model — the same slot `resolve_distill` hands to the weigh phase,
    // so the trait-level guard inside `distill` always sees a match.
    let target: DefaultModel = policy
        .auxiliary(&AuxiliaryKind::Consolidation)
        .map(|a| DefaultModel {
            provider: a.provider.clone(),
            model: a.model.clone(),
        })
        .unwrap_or_else(|| policy.default.clone());
    let distiller = pantheon_providers::DistillClient::new(target, key);
    run_pass(PassInput {
        ledger: &ledger,
        backend: backend.as_ref(),
        policy: &mem_policy,
        config: &cfg,
        data_dir,
        dry_run,
        model_policy: Some(&policy),
        llm: Some(&distiller),
        now_ms: now_ms(),
    })
    .map_err(|e| format!("consolidation pass: {}: {}", e.code, e.cause))
}

/// Whether the operator enabled LLM-backed consolidation in config.
pub fn consolidation_enabled(data_dir: &std::path::Path) -> bool {
    let file_cfg = crate::config::Config::load_or_report(data_dir);
    crate::config::consolidation_config(file_cfg.as_ref()).enabled
}

/// Human-readable status: enabled state plus the last pass summary, so a
/// status check never needs a pass to have run recently.
pub fn status_line(data_dir: &std::path::Path) -> String {
    let enabled = consolidation_enabled(data_dir);
    match pantheon_consolidate::state::load(data_dir) {
        Ok(state) => pantheon_consolidate::report::status_text(data_dir, enabled, &state),
        Err(e) => format!(
            "# Consolidation status\n\nenabled: {}\nstate: unavailable ({e})\n",
            if enabled { "yes" } else { "no" }
        ),
    }
}

/// One-line summary of a finished pass, prefixed so transcript readers can
/// tell a dry run from the real thing.
pub fn summarize_pass(report: &pantheon_consolidate::ConsolidationReport, dry_run: bool) -> String {
    if dry_run {
        format!("(dry run) {}", report.summary_line())
    } else {
        report.summary_line()
    }
}

/// `pantheon consolidate [status] [--dry-run]`.
pub fn cmd_consolidate(args: &[String], data_dir: &std::path::Path) {
    // args[0] is the executable, args[1] is "consolidate".
    let rest = args.get(2..).unwrap_or(&[]);
    let (status, dry_run, unknown) = parse_args(rest);
    if let Some(u) = unknown {
        eprintln!("consolidate: unknown argument `{u}`");
        consolidate_usage();
        std::process::exit(2);
    }
    if status {
        println!("{}", status_line(data_dir));
        return;
    }
    match run_one_pass(data_dir, dry_run) {
        Ok(report) => {
            println!("{}", summarize_pass(&report, dry_run));
            if dry_run {
                println!("note: dry run — nothing promoted, state unchanged");
            }
            if dry_run && !consolidation_enabled(data_dir) {
                println!(
                    "note: [consolidation] enabled = false — this pass ran the deterministic stages only"
                );
            }
        }
        Err(e) => {
            eprintln!("consolidate: {e}");
            std::process::exit(1);
        }
    }
}

fn consolidate_usage() {
    eprintln!(
        "usage:\n  pantheon consolidate [--dry-run]\n  pantheon consolidate status [--dry-run]"
    );
}

fn parse_args(args: &[String]) -> (bool, bool, Option<String>) {
    let mut status = false;
    let mut dry_run = false;
    let mut unknown = None;
    for a in args {
        match a.as_str() {
            "status" => status = true,
            "--dry-run" | "-n" => dry_run = true,
            _ => {
                unknown = Some(a.clone());
                break;
            }
        }
    }
    (status, dry_run, unknown)
}

#[cfg(test)]
mod invariant_tests {
    use super::*;

    #[test]
    fn parse_args_flags() {
        let args = vec!["status".to_string(), "--dry-run".to_string()];
        let (status, dry_run, unknown) = parse_args(&args);
        assert!(status && dry_run && unknown.is_none());
    }

    #[test]
    fn parse_args_rejects_unknown() {
        let args = vec!["bogus".to_string()];
        let (_, _, unknown) = parse_args(&args);
        assert_eq!(unknown.as_deref(), Some("bogus"));
    }
}
