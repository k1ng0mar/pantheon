//! The findings vocabulary: every diagnostic check speaks this, and every
//! renderer and the `/api/health` contract read from it. Checks never
//! print; they return [`Finding`]s.

use serde::{Deserialize, Serialize};

/// The six subsystems a health report covers, in canonical display order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Subsystem {
    Runtime,
    Providers,
    Memory,
    Tools,
    Security,
    Config,
}

impl Subsystem {
    /// Canonical order for grouped output.
    pub const ALL: [Subsystem; 6] = [
        Subsystem::Runtime,
        Subsystem::Providers,
        Subsystem::Memory,
        Subsystem::Tools,
        Subsystem::Security,
        Subsystem::Config,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Subsystem::Runtime => "runtime",
            Subsystem::Providers => "providers",
            Subsystem::Memory => "memory",
            Subsystem::Tools => "tools",
            Subsystem::Security => "security",
            Subsystem::Config => "config",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Subsystem::Runtime => "Runtime & execution",
            Subsystem::Providers => "Models & providers",
            Subsystem::Memory => "Memory & storage",
            Subsystem::Tools => "Tools & integrations",
            Subsystem::Security => "Security",
            Subsystem::Config => "Configuration",
        }
    }

    pub fn subtitle(self) -> &'static str {
        match self {
            Subsystem::Runtime => "Engine, lifecycle, recovery",
            Subsystem::Providers => "Authentication, connectivity, routing",
            Subsystem::Memory => "SQLite, migrations, retrieval",
            Subsystem::Tools => "Availability, dependencies, permissions",
            Subsystem::Security => "Policy, permissions, dependencies",
            Subsystem::Config => "Schema, paths, deprecated settings",
        }
    }

    /// Parse a `--check <subsystem>` argument. Accepts the canonical id
    /// and the few obvious aliases; anything else is a usage error.
    pub fn parse(s: &str) -> Option<Subsystem> {
        match s.to_ascii_lowercase().as_str() {
            "runtime" => Some(Subsystem::Runtime),
            "providers" | "provider" | "models" | "model" => Some(Subsystem::Providers),
            "memory" | "storage" => Some(Subsystem::Memory),
            "tools" | "integrations" => Some(Subsystem::Tools),
            "security" => Some(Subsystem::Security),
            "config" | "configuration" => Some(Subsystem::Config),
            _ => None,
        }
    }
}

/// How bad a finding is. The warning policy is explicit and identical
/// across every renderer:
///
/// - `Critical`: a runtime invariant failed (config unparseable, ledger
///   will not open). The install cannot work until this is fixed.
/// - `Error`: something is broken (missing API key, broken plugin). The
///   install runs, but the affected part does not.
/// - `Warning`: actionable but non-blocking (missing optional dependency,
///   idle gateway, overly broad secrets allowlist). Always shown in the
///   concise output and always forces a non-zero exit - a warning is a
///   promise that something needs attention, not a clean bill of health.
/// - `Info`: context, not action (nightly enabled, agent tables listed).
///
/// Ordering is least-to-most severe so "most severe first" is a reverse
/// sort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Error,
    Critical,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Error => "error",
            Severity::Critical => "critical",
        }
    }
}

/// What a check concluded about its subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    /// The check ran and the expectation held.
    Passed,
    /// The check ran and the expectation did not hold. [`Severity`]
    /// says how much that matters.
    Failed,
    /// The check did not run because a precondition was missing or it
    /// was not requested (opt-in network probe without `--ping`, an
    /// optional integration that is simply absent). A missing optional
    /// integration is `Skipped`, never a failure.
    Skipped,
    /// The check's subject does not exist in this install (driver check
    /// with the computer-use group off). Not a gap, just inapplicable.
    NotApplicable,
}

impl CheckStatus {
    pub fn label(self) -> &'static str {
        match self {
            CheckStatus::Passed => "passed",
            CheckStatus::Failed => "failed",
            CheckStatus::Skipped => "skipped",
            CheckStatus::NotApplicable => "not_applicable",
        }
    }
}

/// One structured diagnostic result. Built by checks, read by renderers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    /// Stable identifier, e.g. `memory.ledger.open`.
    pub id: String,
    pub subsystem: Subsystem,
    pub severity: Severity,
    pub status: CheckStatus,
    /// One line: what was checked and the verdict.
    pub summary: String,
    /// Verbose evidence (paths, counts, raw values). Shown in verbose
    /// output and the JSON report, never in concise output. Must never
    /// contain credential values, tokens, or secret env contents.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    /// What functionality is affected when this finding is failed.
    pub impact: String,
    /// The suggested next action.
    pub remediation: String,
    /// Wall time the check took, in nanoseconds. Stamped by the
    /// orchestrator, not the check.
    pub duration_ns: u64,
    /// Reserved for the phase-4 fix planner. Unused this pass.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix_id: Option<String>,
    /// True when the check itself errored or panicked instead of
    /// producing a real verdict. Forces overall health to `unknown`: a
    /// diagnosis that could not complete must never read as healthy.
    #[serde(default)]
    pub incomplete: bool,
}

impl Finding {
    pub fn passed(id: impl Into<String>, subsystem: Subsystem, summary: impl Into<String>) -> Self {
        Finding {
            id: id.into(),
            subsystem,
            severity: Severity::Info,
            status: CheckStatus::Passed,
            summary: summary.into(),
            details: None,
            impact: String::new(),
            remediation: String::new(),
            duration_ns: 0,
            fix_id: None,
            incomplete: false,
        }
    }

    pub fn failed(
        id: impl Into<String>,
        subsystem: Subsystem,
        severity: Severity,
        summary: impl Into<String>,
        impact: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Finding {
            id: id.into(),
            subsystem,
            severity,
            status: CheckStatus::Failed,
            summary: summary.into(),
            details: None,
            impact: impact.into(),
            remediation: remediation.into(),
            duration_ns: 0,
            fix_id: None,
            incomplete: false,
        }
    }

    pub fn skipped(
        id: impl Into<String>,
        subsystem: Subsystem,
        summary: impl Into<String>,
    ) -> Self {
        Finding {
            id: id.into(),
            subsystem,
            severity: Severity::Info,
            status: CheckStatus::Skipped,
            summary: summary.into(),
            details: None,
            impact: String::new(),
            remediation: String::new(),
            duration_ns: 0,
            fix_id: None,
            incomplete: false,
        }
    }

    pub fn not_applicable(
        id: impl Into<String>,
        subsystem: Subsystem,
        summary: impl Into<String>,
    ) -> Self {
        Finding {
            id: id.into(),
            subsystem,
            severity: Severity::Info,
            status: CheckStatus::NotApplicable,
            summary: summary.into(),
            details: None,
            impact: String::new(),
            remediation: String::new(),
            duration_ns: 0,
            fix_id: None,
            incomplete: false,
        }
    }

    pub fn with_details(mut self, details: impl Into<String>) -> Self {
        self.details = Some(details.into());
        self
    }

    pub fn with_remediation(mut self, remediation: impl Into<String>) -> Self {
        self.remediation = remediation.into();
        self
    }

    /// A finding is actionable when concise output must show it: any
    /// failed check, plus any warning-severity finding.
    pub fn is_actionable(&self) -> bool {
        self.status == CheckStatus::Failed || self.severity == Severity::Warning
    }
}

/// Overall install health, aggregated from findings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverallHealth {
    /// Required checks passed, no actionable warnings.
    Healthy,
    /// Operable but impaired: warnings or errors, no critical failure.
    Degraded,
    /// A critical runtime invariant failed.
    Unhealthy,
    /// Diagnostics could not complete reliably (a check errored or
    /// panicked). Never reported as healthy.
    Unknown,
}

impl OverallHealth {
    pub fn label(self) -> &'static str {
        match self {
            OverallHealth::Healthy => "healthy",
            OverallHealth::Degraded => "degraded",
            OverallHealth::Unhealthy => "unhealthy",
            OverallHealth::Unknown => "unknown",
        }
    }
}

/// Aggregate findings into overall health. The order of the rules is
/// load-bearing: a critical failure beats an incomplete diagnosis, an
/// incomplete diagnosis beats a run where nothing could be evaluated,
/// and any failure beats a clean run.
pub fn aggregate(findings: &[Finding]) -> OverallHealth {
    if findings
        .iter()
        .any(|f| f.status == CheckStatus::Failed && f.severity == Severity::Critical)
    {
        return OverallHealth::Unhealthy;
    }
    if findings.iter().any(|f| f.incomplete) {
        return OverallHealth::Unknown;
    }
    // Nothing produced a verdict and at least one check could not run:
    // the diagnosis is unreliable, never healthy. (A run where every
    // check is not-applicable is a completed diagnosis of nothing, so
    // it stays healthy.)
    let decisive = findings
        .iter()
        .any(|f| matches!(f.status, CheckStatus::Passed | CheckStatus::Failed));
    if !decisive && findings.iter().any(|f| f.status == CheckStatus::Skipped) {
        return OverallHealth::Unknown;
    }
    if findings.iter().any(|f| f.status == CheckStatus::Failed) {
        return OverallHealth::Degraded;
    }
    OverallHealth::Healthy
}

/// The complete result of one diagnostics run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticReport {
    pub schema: String,
    pub generated_at: String,
    pub overall: OverallHealth,
    /// One-line human summary, e.g. "degraded: 1 error, 2 warnings".
    pub summary: String,
    pub findings: Vec<Finding>,
    /// Wall time for the whole run, in nanoseconds.
    pub duration_ns: u64,
}

/// Build the one-line summary from finding counts. Shared by the CLI
/// renderers and the `/api/health` contract so the wording cannot drift.
pub fn summarize(findings: &[Finding]) -> String {
    let (mut critical, mut errors, mut warnings) = (0, 0, 0);
    let mut passed = 0;
    for f in findings {
        if f.status == CheckStatus::Failed {
            match f.severity {
                Severity::Critical => critical += 1,
                Severity::Error => errors += 1,
                Severity::Warning => warnings += 1,
                Severity::Info => warnings += 1,
            }
        } else if f.status == CheckStatus::Passed {
            passed += 1;
        }
    }
    let overall = aggregate(findings);
    if overall == OverallHealth::Healthy {
        return format!("healthy: {passed} passed, no actionable findings");
    }
    let mut parts = Vec::new();
    if critical > 0 {
        parts.push(format!("{critical} critical"));
    }
    if errors > 0 {
        parts.push(format!(
            "{errors} error{}",
            if errors == 1 { "" } else { "s" }
        ));
    }
    if warnings > 0 {
        parts.push(format!(
            "{warnings} warning{}",
            if warnings == 1 { "" } else { "s" }
        ));
    }
    if parts.is_empty() {
        let skipped = findings
            .iter()
            .filter(|f| f.status == CheckStatus::Skipped)
            .count();
        if skipped > 0 {
            parts.push(format!("no check produced a verdict ({skipped} skipped)"));
        } else {
            parts.push("diagnostics incomplete".to_string());
        }
    }
    format!("{}: {}", overall.label(), parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(id: &str, sev: Severity) -> Finding {
        Finding::failed(id, Subsystem::Runtime, sev, "broken", "impact", "fix it")
    }

    #[test]
    fn empty_findings_aggregate_healthy() {
        assert_eq!(aggregate(&[]), OverallHealth::Healthy);
    }

    #[test]
    fn critical_failure_beats_everything() {
        let findings = vec![
            failed("a", Severity::Warning),
            failed("b", Severity::Critical),
            Finding {
                incomplete: true,
                ..failed("c", Severity::Error)
            },
        ];
        assert_eq!(aggregate(&findings), OverallHealth::Unhealthy);
    }

    #[test]
    fn incomplete_check_forces_unknown_not_healthy() {
        // A panicking check must never silently produce a healthy result.
        let findings = vec![Finding {
            incomplete: true,
            ..failed("c", Severity::Error)
        }];
        assert_eq!(aggregate(&findings), OverallHealth::Unknown);
    }

    #[test]
    fn incomplete_loses_to_critical_wins_over_degraded() {
        let findings = vec![
            failed("a", Severity::Warning),
            Finding {
                incomplete: true,
                ..failed("c", Severity::Error)
            },
        ];
        assert_eq!(aggregate(&findings), OverallHealth::Unknown);
    }

    #[test]
    fn error_or_warning_is_degraded() {
        assert_eq!(
            aggregate(&[failed("a", Severity::Error)]),
            OverallHealth::Degraded
        );
        assert_eq!(
            aggregate(&[failed("a", Severity::Warning)]),
            OverallHealth::Degraded
        );
    }

    #[test]
    fn passed_and_skipped_are_healthy() {
        let findings = vec![
            Finding::passed("a", Subsystem::Config, "ok"),
            Finding::skipped("b", Subsystem::Tools, "not requested"),
            Finding::not_applicable("c", Subsystem::Security, "n/a"),
        ];
        assert_eq!(aggregate(&findings), OverallHealth::Healthy);
    }

    #[test]
    fn all_skipped_is_unknown_never_healthy() {
        // A filtered run where nothing could be evaluated must not read
        // as healthy.
        let findings = vec![
            Finding::skipped("a", Subsystem::Providers, "no config"),
            Finding::skipped("b", Subsystem::Providers, "no config"),
        ];
        assert_eq!(aggregate(&findings), OverallHealth::Unknown);
        let s = summarize(&findings);
        assert!(s.starts_with("unknown:"), "{s}");
        assert!(s.contains("2 skipped"), "{s}");
    }

    #[test]
    fn all_not_applicable_stays_healthy() {
        // Not-applicable is a completed diagnosis of nothing: nothing is
        // broken, so this is healthy, not unknown.
        let findings = vec![Finding::not_applicable("a", Subsystem::Tools, "group off")];
        assert_eq!(aggregate(&findings), OverallHealth::Healthy);
    }

    #[test]
    fn warning_severity_failed_counts_as_warning_in_summary() {
        let s = summarize(&[failed("a", Severity::Warning)]);
        assert!(s.starts_with("degraded:"), "{s}");
        assert!(s.contains("1 warning"), "{s}");
    }

    #[test]
    fn healthy_summary_counts_passes() {
        let s = summarize(&[Finding::passed("a", Subsystem::Config, "ok")]);
        assert_eq!(s, "healthy: 1 passed, no actionable findings");
    }

    #[test]
    fn subsystem_parse_accepts_ids_and_aliases() {
        assert_eq!(Subsystem::parse("runtime"), Some(Subsystem::Runtime));
        assert_eq!(Subsystem::parse("providers"), Some(Subsystem::Providers));
        assert_eq!(Subsystem::parse("models"), Some(Subsystem::Providers));
        assert_eq!(Subsystem::parse("storage"), Some(Subsystem::Memory));
        assert_eq!(Subsystem::parse("CONFIG"), Some(Subsystem::Config));
        assert_eq!(Subsystem::parse("bogus"), None);
    }

    #[test]
    fn severity_orders_least_to_most() {
        assert!(Severity::Info < Severity::Warning);
        assert!(Severity::Warning < Severity::Error);
        assert!(Severity::Error < Severity::Critical);
    }
}
