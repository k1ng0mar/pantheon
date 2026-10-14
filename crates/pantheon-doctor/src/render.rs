//! Output renderers for a [`DiagnosticReport`].
//!
//! Three shapes, one canonical option each:
//!
//! - concise (default): failures and warnings only, grouped by
//!   subsystem in canonical order, most severe first. Never a line per
//!   successful check.
//! - verbose (`--verbose`): every finding, including passed / skipped /
//!   not-applicable, with evidence and per-check timings.
//! - JSON (`--json`): stdout contains ONLY valid JSON; progress and
//!   incidental diagnostics go to stderr (the orchestrator's progress
//!   sink), never stdout.
//!
//! Exit codes (decided here, so CLI and tests cannot drift):
//!
//! - 0: no errors/warnings requiring attention (healthy)
//! - 1: findings indicate degraded or unhealthy
//! - 2: invalid arguments or config (decided by arg parsing, not here)
//! - 3: diagnostics could not complete reliably (unknown)

use crate::finding::{CheckStatus, DiagnosticReport, Finding, OverallHealth, Severity, Subsystem};

/// Exit code for a finished diagnostics run.
pub fn exit_code(report: &DiagnosticReport) -> i32 {
    match report.overall {
        OverallHealth::Healthy => 0,
        OverallHealth::Degraded | OverallHealth::Unhealthy => 1,
        OverallHealth::Unknown => 3,
    }
}

fn severity_glyph(sev: Severity) -> &'static str {
    match sev {
        Severity::Critical => "CRIT",
        Severity::Error => "FAIL",
        Severity::Warning => "warn",
        Severity::Info => "info",
    }
}

/// Concise rendering: ONLY failures and warnings, grouped by subsystem
/// (canonical order), most severe first within each group.
pub fn render_concise(report: &DiagnosticReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("pantheon doctor: {}\n", report.summary));
    let actionable: Vec<&Finding> = report
        .findings
        .iter()
        .filter(|f| f.is_actionable())
        .collect();
    if actionable.is_empty() {
        out.push_str("no actionable findings\n");
    } else {
        for sub in Subsystem::ALL {
            let mut group: Vec<&Finding> = actionable
                .iter()
                .filter(|f| f.subsystem == sub)
                .copied()
                .collect();
            if group.is_empty() {
                continue;
            }
            // Most severe first; stable by check id within a severity.
            group.sort_by(|a, b| b.severity.cmp(&a.severity).then_with(|| a.id.cmp(&b.id)));
            out.push_str(&format!("\n[{}] {}\n", sub.id(), sub.title()));
            for f in group {
                out.push_str(&format!(
                    "  {} {}: {}\n",
                    severity_glyph(f.severity),
                    f.id,
                    f.summary
                ));
                if !f.impact.is_empty() {
                    out.push_str(&format!("       impact: {}\n", f.impact));
                }
                if !f.remediation.is_empty() {
                    out.push_str(&format!("       fix: {}\n", f.remediation));
                }
            }
        }
    }
    // Skipped / not-applicable findings are hidden above by design;
    // name the count so an "unknown" verdict is never a mystery.
    let hidden = report
        .findings
        .iter()
        .filter(|f| matches!(f.status, CheckStatus::Skipped | CheckStatus::NotApplicable))
        .count();
    if hidden > 0 {
        out.push_str(&format!(
            "\n{hidden} check(s) skipped or not applicable (see --verbose)\n"
        ));
    }
    out
}

/// Verbose rendering: every finding, with evidence and timings.
/// Narrow-terminal safe: no wide tables, wrapping left to the pager.
pub fn render_verbose(report: &DiagnosticReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("pantheon doctor: {}\n", report.summary));
    out.push_str(&format!(
        "generated: {} ({} findings, {:.1}s)\n",
        report.generated_at,
        report.findings.len(),
        report.duration_ns as f64 / 1_000_000_000.0,
    ));
    for sub in Subsystem::ALL {
        let group: Vec<&Finding> = report
            .findings
            .iter()
            .filter(|f| f.subsystem == sub)
            .collect();
        if group.is_empty() {
            continue;
        }
        out.push_str(&format!("\n[{}] {}\n", sub.id(), sub.title()));
        for f in group {
            let status = match f.status {
                CheckStatus::Passed => "pass",
                CheckStatus::Failed => severity_glyph(f.severity),
                CheckStatus::Skipped => "skip",
                CheckStatus::NotApplicable => "n/a ",
            };
            out.push_str(&format!("  [{status}] {}: {}\n", f.id, f.summary));
            if let Some(d) = &f.details {
                for line in d.lines() {
                    out.push_str(&format!("         | {line}\n"));
                }
            }
            if !f.impact.is_empty() {
                out.push_str(&format!("         impact: {}\n", f.impact));
            }
            if !f.remediation.is_empty() {
                out.push_str(&format!("         fix: {}\n", f.remediation));
            }
            out.push_str(&format!(
                "         ({:.1}ms)\n",
                f.duration_ns as f64 / 1_000_000.0
            ));
        }
    }
    out
}

/// JSON rendering: the report as JSON, nothing else. Serialization
/// failure is a hard error - a corrupt machine-readable report is
/// worse than none.
pub fn render_json(report: &DiagnosticReport) -> Result<String, String> {
    serde_json::to_string_pretty(report)
        .map_err(|e| format!("could not serialize the diagnostics report as JSON: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding::Finding;

    fn report_with(findings: Vec<Finding>) -> DiagnosticReport {
        let overall = crate::finding::aggregate(&findings);
        DiagnosticReport {
            schema: "pantheon.doctor/v1".to_string(),
            generated_at: "2026-10-14T12:00:00+01:00".to_string(),
            overall,
            summary: crate::finding::summarize(&findings),
            findings,
            duration_ns: 0,
        }
    }

    fn failed(id: &str, sub: Subsystem, sev: Severity) -> Finding {
        Finding::failed(id, sub, sev, "broken", "impact", "fix it")
    }

    #[test]
    fn exit_codes_match_overall() {
        assert_eq!(
            exit_code(&report_with(vec![Finding::passed(
                "a",
                Subsystem::Config,
                "ok"
            )])),
            0
        );
        assert_eq!(
            exit_code(&report_with(vec![failed(
                "a",
                Subsystem::Config,
                Severity::Warning
            )])),
            1
        );
        assert_eq!(
            exit_code(&report_with(vec![failed(
                "a",
                Subsystem::Config,
                Severity::Critical
            )])),
            1
        );
        let incomplete = Finding {
            incomplete: true,
            ..failed("a", Subsystem::Config, Severity::Error)
        };
        assert_eq!(exit_code(&report_with(vec![incomplete])), 3);
    }

    #[test]
    fn concise_hides_passing_checks() {
        let text = render_concise(&report_with(vec![
            Finding::passed("a", Subsystem::Config, "all good"),
            failed("b", Subsystem::Tools, Severity::Warning),
        ]));
        assert!(!text.contains("all good"), "passing check leaked: {text}");
        assert!(!text.contains("\"a\""), "passing check id leaked: {text}");
        assert!(text.contains("\"b\"") || text.contains("b:"), "{text}");
        assert!(text.contains("warn"), "{text}");
    }

    #[test]
    fn concise_groups_by_subsystem_severe_first() {
        let text = render_concise(&report_with(vec![
            failed("z.warn", Subsystem::Tools, Severity::Warning),
            failed("a.crit", Subsystem::Tools, Severity::Critical),
            failed("m.err", Subsystem::Config, Severity::Error),
        ]));
        let tools_pos = text.find("[tools]").expect("tools group");
        let config_pos = text.find("[config]").expect("config group");
        assert!(tools_pos < config_pos, "canonical order: {text}");
        let crit_pos = text.find("a.crit").expect("crit");
        let warn_pos = text.find("z.warn").expect("warn");
        assert!(crit_pos < warn_pos, "most severe first: {text}");
    }

    #[test]
    fn concise_names_hidden_skips() {
        let text = render_concise(&report_with(vec![
            Finding::skipped("a", Subsystem::Config, "not asked"),
            Finding::not_applicable("b", Subsystem::Tools, "n/a"),
        ]));
        assert!(
            text.contains("2 check(s) skipped or not applicable"),
            "{text}"
        );
        assert!(text.contains("see --verbose"), "{text}");
    }

    #[test]
    fn concise_clean_run_says_so() {
        let text = render_concise(&report_with(vec![Finding::passed(
            "a",
            Subsystem::Config,
            "ok",
        )]));
        assert!(text.contains("no actionable findings"), "{text}");
    }

    #[test]
    fn verbose_shows_everything_with_timings() {
        let text = render_verbose(&report_with(vec![
            Finding::passed("a", Subsystem::Config, "ok"),
            Finding::skipped("b", Subsystem::Tools, "not asked"),
            failed("c", Subsystem::Security, Severity::Error),
        ]));
        assert!(text.contains("pass"), "{text}");
        assert!(text.contains("skip"), "{text}");
        assert!(text.contains("FAIL"), "{text}");
        assert!(text.contains("ms)"), "{text}");
    }

    #[test]
    fn json_is_valid_and_complete() {
        let json = render_json(&report_with(vec![failed(
            "a",
            Subsystem::Config,
            Severity::Error,
        )]))
        .expect("serialize");
        let v: serde_json::Value = serde_json::from_str(&json).expect("stdout must be valid JSON");
        assert_eq!(v["overall"], "degraded");
        assert_eq!(v["findings"][0]["id"], "a");
        assert_eq!(v["findings"][0]["severity"], "error");
        assert_eq!(v["findings"][0]["status"], "failed");
    }
}
