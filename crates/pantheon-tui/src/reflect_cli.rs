//! `pantheon reflect` - compatibility shim over the unified nightly pass.
//!
//! The old standalone reflection pipeline is merged into
//! [`crate::nightly_cli`]: `pantheon reflect` now runs the full nightly
//! pass (single ledger scan → proposals → eval/replay gates → approval
//! queue → audit). Every function here keeps its old name and shape so
//! the TUI, the scheduler, and scripts keep working; new code should call
//! `nightly_cli` directly.

use std::path::Path;

/// Run one bounded pass. Backed by the unified nightly pipeline.
pub fn run_one_pass(
    data_dir: &Path,
    dry_run: bool,
) -> Result<pantheon_nightly::PassResult, String> {
    crate::nightly_cli::run_one_pass(data_dir, dry_run)
}

/// Persist the nightly LLM toggle. Writes `[nightly] enabled`; the old
/// `[reflect]` section is still read as a legacy fallback.
pub fn persist_reflect_enabled(data_dir: &Path, enabled: bool) -> Result<(), String> {
    crate::nightly_cli::persist_nightly_enabled(data_dir, enabled)
}

/// Current toggle state: `(llm_enabled, auto_turns)`.
pub fn reflect_state(data_dir: &Path) -> (bool, u32) {
    crate::nightly_cli::nightly_state(data_dir)
}

/// One-line status for `/reflect status` / `pantheon reflect status`.
pub fn status_line(data_dir: &Path) -> String {
    crate::nightly_cli::status_line(data_dir)
}

/// Human summary of a finished pass for the CLI and the TUI status line.
pub fn summarize_pass(out: &pantheon_nightly::PassResult) -> String {
    crate::nightly_cli::summarize_pass(out)
}

/// Render pending proposals for approval review.
pub fn render_pending(pending: &[pantheon_nightly::Proposal]) -> String {
    crate::nightly_cli::render_pending(pending)
}

/// `pantheon reflect [--dry-run] [on|off|status|log|pending] [--approve ID] [--deny ID]`
///
/// Compatibility alias for `pantheon nightly`: runs the unified pass.
pub fn cmd_reflect(args: &[String], data_dir: &Path) {
    eprintln!("note: `pantheon reflect` is now part of the unified `pantheon nightly` pass");
    // Rewrite argv[1] so the nightly parser sees a `nightly` command.
    let mut nightly_args = args.to_vec();
    if nightly_args.len() > 1 {
        nightly_args[1] = "nightly".to_string();
    }
    crate::nightly_cli::cmd_nightly(&nightly_args, data_dir)
}
