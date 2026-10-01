//! Audit: every nightly lifecycle event, one JSONL line each.
//!
//! The audit log is the single source of truth for what the nightly pass
//! did and why. Events are append-only JSONL at
//! `<data_dir>/nightly/nightly.jsonl`.

use std::path::{Path, PathBuf};

pub fn audit_path(data_dir: &Path) -> PathBuf {
    data_dir.join("nightly").join("nightly.jsonl")
}

/// Lifecycle events worth auditing. `Debug` formatting is the human
/// line; the JSON payload is the machine one.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum NightlyEvent {
    PassStarted {
        runs_scanned: usize,
        at_ms: i64,
    },
    SignalObserved {
        kind: String,
        hits: usize,
        at_ms: i64,
    },
    ProposalMade {
        id: String,
        kind: String,
        title: String,
        at_ms: i64,
    },
    EvalPassed {
        id: String,
        summary: String,
        at_ms: i64,
    },
    EvalRejected {
        id: String,
        reason: String,
        at_ms: i64,
    },
    ReplayPassed {
        id: String,
        summary: String,
        at_ms: i64,
    },
    ReplayFailed {
        id: String,
        reason: String,
        at_ms: i64,
    },
    LessonApplied {
        id: String,
        key: String,
        at_ms: i64,
    },
    QueuedForApproval {
        id: String,
        kind: String,
        at_ms: i64,
    },
    ApprovalDecided {
        id: String,
        approved: bool,
        at_ms: i64,
    },
    Applied {
        id: String,
        kind: String,
        at_ms: i64,
    },
    /// An approved proposal could not be applied (disk error, bad skill
    /// name). The proposal stays in the pending queue so the human can
    /// retry or deny it; nothing is silently dropped.
    ApplyFailed {
        id: String,
        reason: String,
        at_ms: i64,
    },
    /// One iteration of the bounded fix loop (see `crate::fixloop`):
    /// a deterministic repair was attempted and validation re-ran.
    FixAttempt {
        id: String,
        attempt: usize,
        phase: String,
        detail: String,
        at_ms: i64,
    },
    /// The fix loop gave up: the proposal failed validation after the
    /// maximum attempts (or hit an unrepairable infrastructure failure).
    /// Marked `NeedsAttention`; surfaced in `nightly-escalated.json`
    /// and the pass report. Never queued for approval.
    Escalated {
        id: String,
        reason: String,
        attempts: usize,
        at_ms: i64,
    },
    PassFinished {
        proposals: usize,
        applied: usize,
        pending: usize,
        at_ms: i64,
    },
}

/// Append one event to the audit log.
pub fn audit(data_dir: &Path, event: &NightlyEvent) -> Result<(), String> {
    let path = audit_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let mut line = serde_json::to_string(event).map_err(|e| format!("encode event: {e}"))?;
    line.push('\n');
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    f.write_all(line.as_bytes())
        .map_err(|e| format!("append {}: {e}", path.display()))
}

/// Read every audit event (most recent last); unparseable lines are
/// skipped so one corrupt line never breaks status views.
pub fn read_events(data_dir: &Path) -> Vec<NightlyEvent> {
    let text = std::fs::read_to_string(audit_path(data_dir)).unwrap_or_default();
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// One-line summary of the most recent pass, for status surfaces.
/// Counts lifecycle events since the last `PassStarted`.
pub fn last_run_summary(data_dir: &Path) -> Option<String> {
    let events = read_events(data_dir);
    let start = events
        .iter()
        .rposition(|e| matches!(e, NightlyEvent::PassStarted { .. }))?;
    let mut proposed = 0;
    let mut applied = 0;
    let mut queued = 0;
    let mut rejected = 0;
    let mut escalated = 0;
    for e in &events[start..] {
        match e {
            NightlyEvent::ProposalMade { .. } => proposed += 1,
            NightlyEvent::LessonApplied { .. } | NightlyEvent::Applied { .. } => applied += 1,
            NightlyEvent::QueuedForApproval { .. } => queued += 1,
            NightlyEvent::EvalRejected { .. } | NightlyEvent::ReplayFailed { .. } => rejected += 1,
            NightlyEvent::Escalated { .. } => escalated += 1,
            _ => {}
        }
    }
    Some(format!(
        "{proposed} proposed, {applied} applied, {queued} awaiting approval, {rejected} rejected, {escalated} escalated"
    ))
}

// Small deterministic invariant tests only.
