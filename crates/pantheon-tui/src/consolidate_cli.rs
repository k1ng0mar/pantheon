//! `pantheon consolidate` — compatibility shim over the unified nightly pass.
//!
//! The old standalone consolidation pipeline is merged into
//! [`crate::nightly_cli`]: `pantheon consolidate` now runs the full
//! nightly pass (single ledger scan → memory promotion + proposals →
//! gates → audit). Every function here keeps its old name and shape so
//! the TUI, the scheduler, and scripts keep working; new code should
//! call `nightly_cli` directly.

use std::path::Path;

/// Run one bounded pass. Backed by the unified nightly pipeline.
pub fn run_one_pass(
    data_dir: &Path,
    dry_run: bool,
) -> Result<pantheon_nightly::PassResult, String> {
    crate::nightly_cli::run_one_pass(data_dir, dry_run)
}

/// Whether the operator enabled LLM-backed nightly steps in config.
pub fn consolidation_enabled(data_dir: &Path) -> bool {
    crate::nightly_cli::nightly_enabled(data_dir)
}

/// Human-readable status: enabled state plus the last pass summary.
pub fn status_line(data_dir: &Path) -> String {
    crate::nightly_cli::status_line(data_dir)
}

/// One-line summary of a finished pass, prefixed so transcript readers can
/// tell a dry run from the real thing.
pub fn summarize_pass(out: &pantheon_nightly::PassResult, dry_run: bool) -> String {
    if dry_run {
        format!("(dry run) {}", crate::nightly_cli::summarize_pass(out))
    } else {
        crate::nightly_cli::summarize_pass(out)
    }
}

/// `pantheon consolidate [status] [--dry-run]`.
///
/// Compatibility alias for `pantheon nightly`: runs the unified pass.
pub fn cmd_consolidate(args: &[String], data_dir: &Path) {
    eprintln!("note: `pantheon consolidate` is now part of the unified `pantheon nightly` pass");
    let mut nightly_args = args.to_vec();
    if nightly_args.len() > 1 {
        nightly_args[1] = "nightly".to_string();
    }
    crate::nightly_cli::cmd_nightly(&nightly_args, data_dir)
}
