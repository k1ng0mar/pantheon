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
    /// Queueing for approval failed (IO, corrupt queue file). Deliberately
    /// NOT EvalRejected: a queue failure is an infrastructure failure,
    /// not a judgement that the proposal was bad, and counting it as
    /// "rejected" inflated the rejection count.
    QueueFailed {
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
    /// Token/cost accounting for one successful auxiliary model call made
    /// during a nightly pass. Kept in this audit because nightly work has no
    /// interactive session id to attribute in `pantheon stats`.
    ModelUsage {
        provider: String,
        model: String,
        input_tokens: u64,
        output_tokens: u64,
        total_tokens: u64,
        cost_usd: Option<f64>,
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
    let start_idx = events
        .iter()
        .rposition(|e| matches!(e, NightlyEvent::PassStarted { .. }))?;
    let started_ms = match events.get(start_idx)? {
        NightlyEvent::PassStarted { at_ms, .. } => *at_ms,
        _ => return None,
    };
    let finished_ms = events[start_idx..]
        .iter()
        .find_map(|e| match e {
            NightlyEvent::PassFinished { at_ms, .. } => Some(*at_ms),
            _ => None,
        })
        // No PassFinished yet means the pass is still running or died
        // mid-pass: bound the window at now, not i64::MAX, or usage
        // from every later pass would be folded into this one.
        .unwrap_or_else(|| pantheon_api::logging::now_ms());
    let mut proposed = 0;
    let mut applied = 0;
    let mut queued = 0;
    let mut rejected = 0;
    let mut escalated = 0;
    for e in &events[start_idx..] {
        match e {
            NightlyEvent::ProposalMade { .. } => proposed += 1,
            NightlyEvent::LessonApplied { .. } | NightlyEvent::Applied { .. } => applied += 1,
            NightlyEvent::QueuedForApproval { .. } => queued += 1,
            NightlyEvent::EvalRejected { .. } | NightlyEvent::ReplayFailed { .. } => rejected += 1,
            NightlyEvent::Escalated { .. } => escalated += 1,
            _ => {}
        }
    }
    // Usage callbacks can append before the buffered lifecycle events are
    // flushed at pass end, so select by their durable timestamp, not JSONL
    // position. Rewinds do not apply to these maintenance model calls.
    let usage = events
        .iter()
        .fold((0u64, 0u64, 0u64, 0.0f64), |mut totals, e| {
            if let NightlyEvent::ModelUsage {
                total_tokens,
                cost_usd,
                at_ms,
                ..
            } = e
            {
                if *at_ms >= started_ms && *at_ms <= finished_ms {
                    totals.0 += 1;
                    totals.1 += *total_tokens;
                    if let Some(cost) = cost_usd {
                        totals.2 += 1;
                        totals.3 += *cost;
                    }
                }
            }
            totals
        });
    let usage_summary = if usage.0 == 0 {
        String::new()
    } else if usage.2 == 0 {
        format!(", {} model calls, {} tokens, cost n/a", usage.0, usage.1)
    } else {
        format!(
            ", {} model calls, {} tokens, ${:.4} estimated",
            usage.0, usage.1, usage.3
        )
    };
    Some(format!(
        "{proposed} proposed, {applied} applied, {queued} awaiting approval, {rejected} rejected, {escalated} escalated{usage_summary}"
    ))
}

// Small deterministic invariant tests only.
