//! Human-readable nightly report.
//!
//! One markdown page per pass at `<data_dir>/nightly/nightly-report.md`
//! (also returned by `run_pass`). Written for the operator who runs the
//! pass by hand and wants to see what happened without reading JSONL.

use crate::audit::NightlyEvent;
use crate::propose::Proposal;
use crate::repair_targets::{RepairOutcome, RepairReport};
use crate::state::NightlyState;

pub fn render(
    state: &NightlyState,
    proposals: &[Proposal],
    repairs: &[RepairReport],
    events: &[NightlyEvent],
) -> String {
    let mut md = String::from("# Nightly report\n\n");
    md.push_str(&format!(
        "- Last run: {}\n- Runs scanned: {}\n- Signals: {}\n- Proposals: {}\n- Applied: {}\n- Pending approval: {}\n- Repairs fixed: {}\n- Repairs contained: {}\n- Dry run: {}\n\n",
        fmt_ms(state.last_run_ms),
        state.last_runs_scanned,
        state.last_signals,
        state.last_proposals,
        state.last_applied,
        state.last_pending,
        state.last_repairs_fixed,
        state.last_repairs_contained,
        state.last_dry_run,
    ));

    md.push_str("## Proposals\n\n");
    if proposals.is_empty() {
        md.push_str("No proposals this pass.\n\n");
    }
    for p in proposals {
        md.push_str(&format!(
            "### {} - {}\n- Kind: {}\n- Status: {:?}\n- Provenance: {}\n\n{}\n\n",
            p.id,
            p.title,
            p.kind_name(),
            p.status,
            p.provenance_runs.join(", "),
            p.body,
        ));
    }

    md.push_str("## Repairs\n\n");
    let acted: Vec<&RepairReport> = repairs
        .iter()
        .filter(|r| r.outcome != RepairOutcome::Healthy)
        .collect();
    if acted.is_empty() {
        md.push_str("No broken targets this pass.\n\n");
    }
    for r in acted {
        md.push_str(&format!(
            "- `{}` - {:?}: {}\n",
            r.target, r.outcome, r.detail
        ));
    }
    md.push('\n');

    md.push_str("## Lifecycle\n\n");
    for e in events {
        md.push_str(&format!("- {e:?}\n"));
    }
    md
}

fn fmt_ms(ms: i64) -> String {
    if ms <= 0 {
        return "never".into();
    }
    // Render as a plain UTC timestamp; callers can localize.
    let secs = ms / 1000;
    format!("{secs}s since epoch")
}

// Small deterministic invariant tests only.
