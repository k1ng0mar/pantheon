//! The `pantheon.health/v1` contract served by `GET /api/health`.
//!
//! Built from a [`DiagnosticReport`] by the same pipeline the CLI
//! uses - no duplicated check logic. The mobile Health page renders
//! this document directly.

use crate::finding::{CheckStatus, DiagnosticReport, Finding, Severity, Subsystem};
use serde::Serialize;

/// Contract version. Bumped only on breaking shape changes.
pub const SCHEMA: &str = "pantheon.health/v1";

/// Per-subsystem rollup for the health contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubsystemStatus {
    Healthy,
    Review,
    Down,
    Unknown,
}

impl SubsystemStatus {
    pub fn label(self) -> &'static str {
        match self {
            SubsystemStatus::Healthy => "Healthy",
            SubsystemStatus::Review => "Review",
            SubsystemStatus::Down => "Down",
            SubsystemStatus::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CoreRuntime {
    pub status: SubsystemStatus,
    pub label: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthCheck {
    pub id: String,
    pub status: CheckStatus,
    pub severity: Severity,
    pub summary: String,
    pub details: String,
    pub impact: String,
    pub remediation: String,
    pub duration_ns: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthSubsystem {
    pub id: String,
    pub title: String,
    pub subtitle: String,
    pub status: SubsystemStatus,
    pub checks: Vec<HealthCheck>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthIssue {
    pub id: String,
    pub subsystem: Subsystem,
    /// Contract severity: error|warning|info. A critical finding maps
    /// to "error" here (its criticality is preserved in `checks`).
    pub severity: String,
    pub title: String,
    pub detail: String,
    pub hint: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthResponse {
    pub schema: String,
    pub generated_at: String,
    pub overall: crate::finding::OverallHealth,
    pub summary: String,
    pub core_runtime: CoreRuntime,
    pub issue_count: usize,
    pub subsystems: Vec<HealthSubsystem>,
    pub issues: Vec<HealthIssue>,
}

/// Roll one subsystem's findings up to a status: healthy when no check
/// failed, review on warnings, down on error/critical failures,
/// unknown when the checks could not run.
fn subsystem_status(findings: &[&Finding]) -> SubsystemStatus {
    if findings.is_empty() {
        return SubsystemStatus::Unknown;
    }
    if findings.iter().any(|f| f.incomplete) {
        return SubsystemStatus::Unknown;
    }
    if findings.iter().any(|f| {
        f.status == CheckStatus::Failed
            && matches!(f.severity, Severity::Critical | Severity::Error)
    }) {
        return SubsystemStatus::Down;
    }
    if findings.iter().any(|f| f.status == CheckStatus::Failed) {
        return SubsystemStatus::Review;
    }
    SubsystemStatus::Healthy
}

/// Build the `pantheon.health/v1` document from a diagnostics report.
pub fn build(report: &DiagnosticReport) -> HealthResponse {
    let subsystems: Vec<HealthSubsystem> = Subsystem::ALL
        .iter()
        .map(|sub| {
            let group: Vec<&Finding> = report
                .findings
                .iter()
                .filter(|f| f.subsystem == *sub)
                .collect();
            let status = subsystem_status(&group);
            let checks = group
                .into_iter()
                .map(|f| HealthCheck {
                    id: f.id.clone(),
                    status: f.status,
                    severity: f.severity,
                    summary: f.summary.clone(),
                    details: f.details.clone().unwrap_or_default(),
                    impact: f.impact.clone(),
                    remediation: f.remediation.clone(),
                    duration_ns: f.duration_ns,
                })
                .collect();
            HealthSubsystem {
                id: sub.id().to_string(),
                title: sub.title().to_string(),
                subtitle: sub.subtitle().to_string(),
                status,
                checks,
            }
        })
        .collect();

    let runtime_status = subsystems
        .iter()
        .find(|s| s.id == "runtime")
        .map(|s| s.status)
        .unwrap_or(SubsystemStatus::Unknown);

    // Issues: findings with severity error/warning/critical, most severe
    // first, stable by id within a severity.
    let mut actionable: Vec<&Finding> = report
        .findings
        .iter()
        .filter(|f| {
            matches!(
                f.severity,
                Severity::Critical | Severity::Error | Severity::Warning
            ) && f.status == CheckStatus::Failed
        })
        .collect();
    actionable.sort_by(|a, b| b.severity.cmp(&a.severity).then_with(|| a.id.cmp(&b.id)));
    let issues: Vec<HealthIssue> = actionable
        .into_iter()
        .map(|f| HealthIssue {
            id: f.id.clone(),
            subsystem: f.subsystem,
            severity: match f.severity {
                Severity::Critical => "error".to_string(),
                Severity::Error => "error".to_string(),
                Severity::Warning => "warning".to_string(),
                Severity::Info => "info".to_string(),
            },
            title: f.summary.clone(),
            detail: if f.impact.is_empty() {
                f.details.clone().unwrap_or_default()
            } else {
                f.impact.clone()
            },
            hint: f.remediation.clone(),
        })
        .collect();

    HealthResponse {
        schema: SCHEMA.to_string(),
        generated_at: report.generated_at.clone(),
        overall: report.overall,
        summary: report.summary.clone(),
        core_runtime: CoreRuntime {
            status: runtime_status,
            label: runtime_status.label().to_string(),
        },
        issue_count: issues.len(),
        subsystems,
        issues,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::finding::{DiagnosticReport, Finding};

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

    #[test]
    fn contract_shape_matches_spec() {
        let resp = build(&report_with(vec![
            Finding::passed("config.file", Subsystem::Config, "parsed"),
            Finding::passed(
                "runtime.nightly_state",
                Subsystem::Runtime,
                "nightly pass enabled",
            ),
            Finding::failed(
                "providers.model_key",
                Subsystem::Providers,
                Severity::Error,
                "KEY is not set",
                "model requests fail auth",
                "export KEY=<key>",
            ),
            Finding::failed(
                "tools.skill_deps",
                Subsystem::Tools,
                Severity::Warning,
                "node not found",
                "node-debug skill degrades",
                "install node",
            ),
        ]));
        let v = serde_json::to_value(&resp).expect("serialize");
        assert_eq!(v["schema"], "pantheon.health/v1");
        assert_eq!(v["overall"], "degraded");
        assert_eq!(v["issue_count"], 2);
        assert_eq!(v["subsystems"].as_array().unwrap().len(), 6);
        // Subsystem ids/titles per the contract.
        let subs = v["subsystems"].as_array().unwrap();
        assert_eq!(subs[0]["id"], "runtime");
        assert_eq!(subs[0]["title"], "Runtime & execution");
        assert_eq!(subs[1]["title"], "Models & providers");
        assert_eq!(subs[2]["title"], "Memory & storage");
        assert_eq!(subs[3]["title"], "Tools & integrations");
        assert_eq!(subs[4]["title"], "Security");
        assert_eq!(subs[5]["title"], "Configuration");
        // Rollups: providers down (error), tools review (warning).
        let providers = &subs[1];
        assert_eq!(providers["status"], "down");
        assert_eq!(subs[3]["status"], "review");
        assert_eq!(subs[0]["status"], "healthy");
        // core_runtime mirrors the runtime subsystem.
        assert_eq!(v["core_runtime"]["status"], "healthy");
        assert_eq!(v["core_runtime"]["label"], "Healthy");
        // Issues: most severe first, critical would map to "error".
        let issues = v["issues"].as_array().unwrap();
        assert_eq!(issues[0]["id"], "providers.model_key");
        assert_eq!(issues[0]["severity"], "error");
        assert_eq!(issues[0]["title"], "KEY is not set");
        assert_eq!(issues[0]["detail"], "model requests fail auth");
        assert_eq!(issues[0]["hint"], "export KEY=<key>");
        assert_eq!(issues[0]["subsystem"], "providers");
        assert_eq!(issues[1]["severity"], "warning");
    }

    #[test]
    fn critical_maps_to_error_in_issues_but_down_in_subsystem() {
        let resp = build(&report_with(vec![Finding::failed(
            "memory.ledger",
            Subsystem::Memory,
            Severity::Critical,
            "ledger does not open",
            "no persistence",
            "check disk",
        )]));
        let v = serde_json::to_value(&resp).expect("serialize");
        assert_eq!(v["overall"], "unhealthy");
        assert_eq!(v["subsystems"][2]["status"], "down");
        assert_eq!(v["issues"][0]["severity"], "error");
    }

    #[test]
    fn incomplete_check_is_unknown_everywhere() {
        let resp = build(&report_with(vec![Finding {
            incomplete: true,
            ..Finding::failed(
                "tools.plugins",
                Subsystem::Tools,
                Severity::Error,
                "panicked",
                "unevaluated",
                "rerun",
            )
        }]));
        let v = serde_json::to_value(&resp).expect("serialize");
        assert_eq!(v["overall"], "unknown");
        assert_eq!(v["subsystems"][3]["status"], "unknown");
    }

    #[test]
    fn empty_subsystem_is_unknown_not_healthy() {
        // A subsystem with no findings must not read as healthy.
        let resp = build(&report_with(vec![]));
        let v = serde_json::to_value(&resp).expect("serialize");
        for s in v["subsystems"].as_array().unwrap() {
            assert_eq!(s["status"], "unknown");
        }
        assert_eq!(v["core_runtime"]["status"], "unknown");
        assert_eq!(v["core_runtime"]["label"], "Unknown");
    }
}
