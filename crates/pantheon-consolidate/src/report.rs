//! Human-readable phase reports: `<data_dir>/consolidation/<phase>/YYYY-MM-DD.md`.
//!
//! Every pass writes one Markdown file per phase (stage, weigh, promote)
//! plus a summary, so the operator can read what consolidation did — or,
//! under `--dry-run`, what it *would* do — without touching the store.

use crate::{ConsolidationReport, Weighed};

/// Phase names, in pass order.
pub const PHASES: [&str; 3] = ["stage", "weigh", "promote"];

fn day_stamp(ms: i64) -> String {
    // UTC calendar day; reports are operator-facing logs, not ledger
    // records, so local-tz precision is unnecessary.
    let days = ms.div_euclid(86_400_000);
    // Days since Unix epoch → civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", y + if m <= 2 { 1 } else { 0 }, m, d)
}

fn phase_path(data_dir: &std::path::Path, phase: &str, day: &str) -> std::path::PathBuf {
    data_dir
        .join("consolidation")
        .join(phase)
        .join(format!("{day}.md"))
}

fn write_phase(
    data_dir: &std::path::Path,
    phase: &str,
    day: &str,
    body: &str,
) -> Result<(), String> {
    let path = phase_path(data_dir, phase, day);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::fs::write(&path, body).map_err(|e| format!("write {}: {e}", path.display()))
}

/// Write the stage/weigh/promote reports for one pass. Best-effort: the
/// caller logs failures but never fails the pass over observability.
pub fn write_reports(
    data_dir: &std::path::Path,
    report: &ConsolidationReport,
    weighed: &[Weighed],
    at_ms: i64,
) -> Result<(), String> {
    let day = day_stamp(at_ms);
    let dry = if report.dry_run { " (DRY RUN)" } else { "" };
    let llm = if report.llm_used {
        "distill: auxiliary model (`AuxiliaryKind::Consolidation`)"
    } else {
        "distill: none (deterministic)"
    };

    let mut stage = format!("# Consolidation — stage{dry}\n\nDate: {day}\n\n");
    stage.push_str(&format!("Candidates staged: {}\n\n", report.staged));
    stage.push_str("| text | kind | sources |\n|---|---|---|\n");
    for w in weighed {
        stage.push_str(&format!(
            "| {} | {} | {} |\n",
            w.text
                .replace('|', "\\|")
                .chars()
                .take(80)
                .collect::<String>(),
            w.kind.label(),
            w.sources.len()
        ));
    }
    write_phase(data_dir, "stage", &day, &stage)?;

    let mut weigh = format!("# Consolidation — weigh{dry}\n\nDate: {day}\n\n{llm}\n\n");
    weigh.push_str("| text | kind | score | runs | promote | key |\n|---|---|---|---|---|---|\n");
    for w in weighed {
        weigh.push_str(&format!(
            "| {} | {} | {:.2} | {} | {} | `{}` |\n",
            w.text
                .replace('|', "\\|")
                .chars()
                .take(80)
                .collect::<String>(),
            w.kind.label(),
            w.score,
            w.distinct_runs,
            if w.promote { "yes" } else { "no" },
            w.key,
        ));
    }
    write_phase(data_dir, "weigh", &day, &weigh)?;

    let mut promote = format!("# Consolidation — promote{dry}\n\nDate: {day}\n\n");
    promote.push_str(&format!(
        "Promoted: {} · skipped as duplicates: {}\n\n",
        report.promoted, report.skipped_duplicate
    ));
    for w in weighed.iter().filter(|w| w.promote) {
        promote.push_str(&format!("- `{}` — {}\n", w.key, w.text));
        let runs: Vec<String> = {
            let mut r: Vec<&str> = w.sources.iter().map(|s| s.run_id.as_str()).collect();
            r.sort_unstable();
            r.dedup();
            r.into_iter().map(|s| format!("`{s}`")).collect()
        };
        promote.push_str(&format!("  provenance: {}\n", runs.join(", ")));
    }
    write_phase(data_dir, "promote", &day, &promote)?;

    Ok(())
}

/// Render a `--status` summary from state + the latest reports.
pub fn status_text(
    data_dir: &std::path::Path,
    enabled: bool,
    state: &crate::state::ConsolidationState,
) -> String {
    let mut out = String::from("# Consolidation status\n\n");
    out.push_str(&format!(
        "enabled: {}\n",
        if enabled { "yes" } else { "no" }
    ));
    out.push_str(&format!(
        "last run: {}\n",
        state
            .last_summary
            .as_deref()
            .unwrap_or("never (no completed pass yet)")
    ));
    if let (Some(staged), Some(promoted)) = (state.last_staged, state.last_promoted) {
        out.push_str(&format!(
            "last pass: staged {staged}, promoted {promoted}\n"
        ));
    }
    out.push_str(&format!(
        "facts promoted (all time): {}\n",
        state.promoted_keys.len()
    ));
    out.push_str(&format!(
        "\nreports: {}\n",
        data_dir.join("consolidation").display()
    ));
    out
}

#[cfg(test)]
mod invariant_tests {
    use super::*;

    #[test]
    fn day_stamp_known_date() {
        // 2026-09-28 00:00:00 UTC = 1790553600000 ms.
        assert_eq!(day_stamp(1_790_553_600_000), "2026-09-28");
    }

    #[test]
    fn phases_in_pass_order() {
        assert_eq!(PHASES, ["stage", "weigh", "promote"]);
    }

    #[test]
    fn status_marks_disabled() {
        let st = crate::state::ConsolidationState::default();
        let t = status_text(std::path::Path::new("/tmp"), false, &st);
        assert!(t.contains("enabled: no"));
        assert!(t.contains("never"));
    }
}
