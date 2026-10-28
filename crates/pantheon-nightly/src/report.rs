//! Human-readable nightly report.
//!
//! One markdown page per pass at `<data_dir>/nightly/nightly-report.md`
//! (also returned by `run_pass`). Written for the operator who runs the
//! pass by hand and wants to see what happened without reading JSONL.

use crate::audit::NightlyEvent;
use crate::propose::Proposal;
use crate::repair_targets::{RepairOutcome, RepairReport};
use crate::state::NightlyState;

/// The report label for a repair outcome: exactly `ok` | `skipped` |
/// `failed`, never the Rust variant name (`Contained`, `SkippedDryRun`)
/// and never a vague word. A contained target was disabled/paused with
/// an escalation: from the operator's view that target failed.
fn repair_label(outcome: &RepairOutcome) -> &'static str {
    match outcome {
        RepairOutcome::Healthy | RepairOutcome::Repaired => "ok",
        RepairOutcome::SkippedDryRun => "skipped",
        RepairOutcome::Contained => "failed",
    }
}

pub fn render(
    state: &NightlyState,
    proposals: &[Proposal],
    repairs: &[RepairReport],
    events: &[NightlyEvent],
) -> String {
    let mut md = String::from("# Nightly report\n\n");
    md.push_str(&format!(
        "- Last run: {}\n- Outcome: {}{}\n- Runs scanned: {}\n- Signals: {}\n- Proposals: {}\n- Applied: {}\n- Pending approval: {}\n- Repairs fixed: {}\n- Repairs contained: {}\n- Dry run: {}\n\n",
        fmt_ms(state.last_run_ms),
        state.last_outcome,
        if state.last_outcome_reason.is_empty() {
            String::new()
        } else {
            format!(" ({})", state.last_outcome_reason)
        },
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
        let detail = if r.detail.is_empty() {
            match r.outcome {
                RepairOutcome::SkippedDryRun => "dry run: detected, not changed",
                _ => "(no detail)",
            }
        } else {
            r.detail.as_str()
        };
        md.push_str(&format!(
            "- `{}` - {}: {}\n",
            r.target,
            repair_label(&r.outcome),
            detail
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
    // A real UTC date, not "Ns since epoch": the old rendering forced
    // the operator to convert epoch seconds by hand to know when a
    // pass ran. Civil-from-days (Hinnant's algorithm), std-only.
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

// Small deterministic invariant tests only.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_ms_renders_utc_date_not_epoch_seconds() {
        assert_eq!(fmt_ms(0), "never");
        assert_eq!(fmt_ms(-5), "never");
        // 1790449200s == 2026-09-26 19:00:00 UTC.
        assert_eq!(fmt_ms(1_790_449_200_000), "2026-09-26 19:00:00 UTC");
        assert_eq!(fmt_ms(1_000), "1970-01-01 00:00:01 UTC");
    }

    /// Repair lines carry exactly ok / skipped / failed, never the
    /// Rust variant names and never a vague word.
    #[test]
    fn repair_labels_are_explicit() {
        assert_eq!(repair_label(&RepairOutcome::Healthy), "ok");
        assert_eq!(repair_label(&RepairOutcome::Repaired), "ok");
        assert_eq!(repair_label(&RepairOutcome::SkippedDryRun), "skipped");
        assert_eq!(repair_label(&RepairOutcome::Contained), "failed");
    }

    /// The header line names the pass outcome (with its reason), so
    /// the report never leaves the operator guessing whether the pass
    /// itself succeeded.
    #[test]
    fn report_header_names_outcome() {
        let mut state = NightlyState::default();
        state.last_run_ms = 1_790_449_200_000;
        state.last_outcome = "failed".to_string();
        state.last_outcome_reason = "ledger unreadable".to_string();
        let md = render(&state, &[], &[], &[]);
        assert!(md.contains("- Outcome: failed (ledger unreadable)"), "{md}");
        state.last_outcome = "ok".to_string();
        state.last_outcome_reason = String::new();
        let md = render(&state, &[], &[], &[]);
        assert!(md.contains("- Outcome: ok\n"), "{md}");
    }
}
