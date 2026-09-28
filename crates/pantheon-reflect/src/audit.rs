//! The reflection audit log: every proposal lifecycle event, appended as
//! one JSON line to `<data_dir>/reflect.jsonl`. This is what makes
//! self-modification accountable: what was proposed, what evals it passed,
//! who approved it, and which sessions it learned from.

use crate::Proposal;
use pantheon_api::error::PantheonError;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Lifecycle events, in the order they can occur.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AuditEvent {
    Proposed,
    EvalPassed {
        summary: String,
    },
    EvalFailed {
        reason: String,
    },
    Approved {
        by: String,
    },
    Denied {
        by: String,
    },
    Applied {
        detail: String,
    },
    /// Dry-run or superseded: recorded for completeness, changed nothing.
    Skipped {
        reason: String,
    },
    /// Pass-level completion marker. Written once per non-dry-run pass,
    /// even when the pass produced zero proposals, so `status` can always
    /// summarize the latest run.
    PassCompleted {
        proposed: usize,
        applied: usize,
        pending: usize,
        rejected: usize,
    },
}

impl AuditEvent {
    pub fn name(&self) -> &'static str {
        match self {
            AuditEvent::Proposed => "proposed",
            AuditEvent::EvalPassed { .. } => "eval_passed",
            AuditEvent::EvalFailed { .. } => "eval_failed",
            AuditEvent::Approved { .. } => "approved",
            AuditEvent::Denied { .. } => "denied",
            AuditEvent::Applied { .. } => "applied",
            AuditEvent::Skipped { .. } => "skipped",
            AuditEvent::PassCompleted { .. } => "pass_completed",
        }
    }
}

/// One audit line.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AuditRecord {
    pub ts_ms: i64,
    /// The reflection pass that produced this event. Groups the whole
    /// lifecycle of one pass (`/reflect`, `pantheon reflect`, scheduled,
    /// or automatic) so `log` and `status` can summarize per run.
    #[serde(default)]
    pub pass_id: String,
    pub proposal_id: String,
    pub kind: String,
    pub title: String,
    pub event: AuditEvent,
    pub provenance_runs: Vec<String>,
    pub eval_summary: Option<String>,
    pub approver: Option<String>,
}

/// Append-only JSONL audit log.
pub struct ReflectAudit {
    path: PathBuf,
    pass_id: String,
}

impl ReflectAudit {
    pub fn open(path: &Path) -> Result<Self, std::io::Error> {
        Self::open_for_pass(path, String::new())
    }

    /// Open the audit log tagged to one pass. Every record written
    /// through this handle carries `pass_id`, which is what `log` and
    /// `status` group by.
    pub fn open_for_pass(path: &Path, pass_id: String) -> Result<Self, std::io::Error> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Self {
            path: path.to_path_buf(),
            pass_id,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one lifecycle event for a proposal.
    pub fn record(
        &self,
        proposal: &Proposal,
        event: AuditEvent,
        approver: Option<&str>,
    ) -> Result<(), PantheonError> {
        let eval_summary = match &event {
            AuditEvent::EvalPassed { summary } => Some(summary.clone()),
            _ => None,
        };
        let record = AuditRecord {
            ts_ms: crate::now_ms(),
            pass_id: self.pass_id.clone(),
            proposal_id: proposal.id.clone(),
            kind: proposal.kind_name().to_string(),
            title: proposal.title.clone(),
            event,
            provenance_runs: proposal.provenance_runs.clone(),
            eval_summary,
            approver: approver.map(|s| s.to_string()),
        };
        self.append(&record)
    }

    /// Read the whole log, oldest first. Malformed lines are skipped, not
    /// fatal: the audit log must never take down a reflection pass.
    pub fn read_all(&self) -> Vec<AuditRecord> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|l| serde_json::from_str::<AuditRecord>(l).ok())
            .collect()
    }

    /// Append the pass-level completion record. Called once per
    /// non-dry-run pass, even when it produced zero proposals, so
    /// `status`/`log` can always summarize the latest run.
    pub fn record_pass(
        &self,
        proposed: usize,
        applied: usize,
        pending: usize,
        rejected: usize,
    ) -> Result<(), PantheonError> {
        self.append(&AuditRecord {
            ts_ms: crate::now_ms(),
            pass_id: self.pass_id.clone(),
            proposal_id: format!("pass:{}", self.pass_id),
            kind: "pass".to_string(),
            title: format!(
                "pass completed: {proposed} proposed, {applied} applied, {pending} pending, {rejected} rejected"
            ),
            event: AuditEvent::PassCompleted {
                proposed,
                applied,
                pending,
                rejected,
            },
            provenance_runs: Vec::new(),
            eval_summary: None,
            approver: None,
        })
    }

    fn append(&self, record: &AuditRecord) -> Result<(), PantheonError> {
        let line = serde_json::to_string(record).map_err(|e| {
            PantheonError::new(
                "RFL_AUDIT_ENCODE",
                pantheon_api::error::Layer::Runtime,
                false,
                format!("encode audit record: {e}"),
                "report this as a bug",
                String::new(),
            )
        })?;
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| {
                PantheonError::new(
                    "RFL_AUDIT_WRITE",
                    pantheon_api::error::Layer::Runtime,
                    false,
                    format!("append to {}: {e}", self.path.display()),
                    "check the data dir is writable",
                    String::new(),
                )
            })?;
        writeln!(f, "{line}").map_err(|e| {
            PantheonError::new(
                "RFL_AUDIT_WRITE",
                pantheon_api::error::Layer::Runtime,
                false,
                format!("append to {}: {e}", self.path.display()),
                "check disk space",
                String::new(),
            )
        })?;
        Ok(())
    }
}

/// One-line summary of the most recent reflection pass, for
/// `/reflect status` and `pantheon reflect status`. Groups by `pass_id`
/// (records written before pass ids existed count as one legacy run).
/// `None` when there is no history at all.
pub fn last_run_summary(data_dir: &Path) -> Option<String> {
    let audit = ReflectAudit::open(&data_dir.join("reflect.jsonl")).ok()?;
    let records = audit.read_all();
    if records.is_empty() {
        return None;
    }
    let last_pass = records
        .iter()
        .rev()
        .find_map(|r| {
            if r.pass_id.is_empty() {
                None
            } else {
                Some(r.pass_id.clone())
            }
        })
        .unwrap_or_default();
    let pass: Vec<&AuditRecord> = if last_pass.is_empty() {
        records.iter().collect()
    } else {
        records.iter().filter(|r| r.pass_id == last_pass).collect()
    };
    let count = |name: &str| pass.iter().filter(|r| r.event.name() == name).count();
    let proposed = count("proposed");
    let applied = count("applied");
    let eval_failed = count("eval_failed");
    let denied = count("denied");
    let pending = pass
        .iter()
        .filter(|r| r.event.name() == "proposed")
        .filter(|r| {
            !pass.iter().any(|o| {
                o.proposal_id == r.proposal_id
                    && matches!(
                        o.event.name(),
                        "applied" | "denied" | "skipped" | "eval_failed"
                    )
            })
        })
        .count();
    Some(format!(
        "last pass: {proposed} proposed, {applied} applied, {pending} awaiting approval, {eval_failed} eval failures, {denied} denied"
    ))
}

/// Render the audit log for `pantheon reflect log`.
pub fn render_log(records: &[AuditRecord]) -> String {
    if records.is_empty() {
        return "no reflection history yet — run `pantheon reflect` first".to_string();
    }
    let mut out = String::new();
    for r in records.iter().rev().take(50) {
        let ts = r.ts_ms;
        let extra = match &r.event {
            AuditEvent::EvalPassed { summary } => format!(" ({summary})"),
            AuditEvent::EvalFailed { reason } => format!(" ({reason})"),
            AuditEvent::Approved { by } | AuditEvent::Denied { by } => format!(" (by {by})"),
            AuditEvent::Applied { detail } => format!(" ({detail})"),
            AuditEvent::Skipped { reason } => format!(" ({reason})"),
            // The pass record's title already carries the counts.
            AuditEvent::PassCompleted { .. } => String::new(),
            AuditEvent::Proposed => String::new(),
        };
        let runs = if r.provenance_runs.is_empty() {
            String::new()
        } else {
            format!(" ← {}", r.provenance_runs.join(","))
        };
        out.push_str(&format!(
            "[{}] {} {}: {}{}{}\n",
            ts,
            r.kind,
            r.event.name(),
            r.title,
            extra,
            runs
        ));
    }
    out
}

// Small deterministic invariant tests only.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_empty_log() {
        assert!(render_log(&[]).contains("no reflection history"));
    }
}
