//! JSONL audit log: one line per ledger event, replayable and
//! sequence-validated. The eval interface - a run's full trajectory in a
//! plain file that Python (or anything) can score without opening SQLite.
//!
//! Line shape:
//!   {"seq": 7, "event": "ToolStarted", "tool": "shell", "call_id": "call_1_0", ...}
//!
//! `seq` is the per-run sequence number (the ledger row id is a separate
//! `id` field on [`LedgerEntry`]; the two coincide only when no rows were
//! ever deleted). Export validates `seq`, the value it actually emits,
//! and refuses to emit a file with duplicate or decreasing seq values (a
//! corrupted ledger should fail loudly here, not downstream).
use crate::ledger::LedgerEntry;
use pantheon_api::error::{Layer, PantheonError};
use std::io::Write;
use std::path::Path;

fn aerr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(code, Layer::Storage, false, cause, "check the ledger", "")
}

/// Serialize one event name + the fields evals care about. Event-specific
/// fields flatten onto the object; unknown events still export (name only)
/// so new event kinds never silently vanish from trajectories.
pub fn audit_line(entry: &LedgerEntry) -> String {
    let mut obj = serde_json::json!({
        "seq": entry.seq,
        "event": event_name(&entry.event),
    });
    match &entry.event {
        pantheon_api::events::Event::ToolStarted {
            call_id,
            tool,
            args,
            ..
        } => {
            obj["call_id"] = serde_json::json!(call_id);
            obj["tool"] = serde_json::json!(tool);
            obj["args"] = serde_json::json!(args);
        }
        pantheon_api::events::Event::ToolOutput {
            call_id,
            tool,
            truncated,
            ..
        } => {
            obj["call_id"] = serde_json::json!(call_id);
            obj["tool"] = serde_json::json!(tool);
            obj["truncated"] = serde_json::json!(truncated);
        }
        pantheon_api::events::Event::ToolCompleted { call_id, tool, .. } => {
            obj["call_id"] = serde_json::json!(call_id);
            obj["tool"] = serde_json::json!(tool);
        }
        pantheon_api::events::Event::ModelDelta { delta, .. } => {
            obj["delta"] = serde_json::json!(delta);
        }
        pantheon_api::events::Event::RunFailed { code, .. } => {
            obj["code"] = serde_json::json!(code);
        }
        pantheon_api::events::Event::RunCanceled { reason, .. } => {
            obj["reason"] = serde_json::json!(reason);
        }
        pantheon_api::events::Event::ApprovalRequested { scope, .. } => {
            obj["scope"] = serde_json::json!(scope);
        }
        pantheon_api::events::Event::ApprovalGranted { scope, .. }
        | pantheon_api::events::Event::ApprovalDenied { scope, .. } => {
            obj["scope"] = serde_json::json!(scope);
        }
        _ => {}
    }
    serde_json::to_string(&obj).unwrap_or_else(|_| "{}".into())
}

/// Stable snake_case names for event kinds.
fn event_name(e: &pantheon_api::events::Event) -> &'static str {
    use pantheon_api::events::Event::*;
    match e {
        RunStarted { .. } => "RunStarted",
        RunProgress { .. } => "RunProgress",
        RunCompleted { .. } => "RunCompleted",
        RunFailed { .. } => "RunFailed",
        RunCanceled { .. } => "RunCanceled",
        RunRecovered { .. } => "RunRecovered",
        ModelRequested { .. } => "ModelRequested",
        ModelCompleted { .. } => "ModelCompleted",
        ModelDelta { .. } => "ModelDelta",
        AssistantMessage { .. } => "AssistantMessage",
        ToolMessage { .. } => "ToolMessage",
        ToolRequested { .. } => "ToolRequested",
        ToolStarted { .. } => "ToolStarted",
        ToolOutput { .. } => "ToolOutput",
        ToolCompleted { .. } => "ToolCompleted",
        ApprovalRequested { .. } => "ApprovalRequested",
        ApprovalGranted { .. } => "ApprovalGranted",
        ApprovalDenied { .. } => "ApprovalDenied",
        PreStateRecorded { .. } => "PreStateRecorded",
        AgentMessage { .. } => "AgentMessage",
        AgentSpawned { .. } => "AgentSpawned",
        AgentCompleted { .. } => "AgentCompleted",
        SessionTitled { .. } => "SessionTitled",
        CheckpointCreated { .. } => "CheckpointCreated",
        AgentBound { .. } => "AgentBound",
        TurnStarted { .. } => "TurnStarted",
        TurnParked { .. } => "TurnParked",
        TurnCompleted { .. } => "TurnCompleted",
        TurnFailed { .. } => "TurnFailed",
        TurnRewound { .. } => "TurnRewound",
        SteeringProvided { .. } => "SteeringProvided",
        AgentMessageDelivered { .. } => "AgentMessageDelivered",
        ToolInterrupted { .. } => "ToolInterrupted",
        TodosUpdated { .. } => "TodosUpdated",
        UiContent { .. } => "UiContent",
        UiUpdated { .. } => "UiUpdated",
        UiActionRequested { .. } => "UiActionRequested",
        UiActionSubmitted { .. } => "UiActionSubmitted",
        ImportedReasoning { .. } => "ImportedReasoning",
        MemoryProposed { .. } => "MemoryProposed",
        UserInputRequested { .. } => "UserInputRequested",
        UserInputProvided { .. } => "UserInputProvided",
        DecisionRequested { .. } => "DecisionRequested",
        DecisionMade { .. } => "DecisionMade",
        DecisionRecorded { .. } => "DecisionRecorded",
        ContextTrimmed { .. } => "ContextTrimmed",
        ContextCompressed { .. } => "ContextCompressed",
        UsageRecorded { .. } => "UsageRecorded",
        BrowserActivity { .. } => "BrowserActivity",
        ScheduledTaskFailed { .. } => "ScheduledTaskFailed",
        ScheduledTaskRecovered { .. } => "ScheduledTaskRecovered",
        _ => "Other",
    }
}

/// Export a run's ledger entries to a JSONL file. Refuses non-monotonic
/// or duplicate seq values - a corrupted ledger must fail loudly here.
pub fn export_jsonl(entries: &[LedgerEntry], path: &Path) -> Result<usize, PantheonError> {
    // Validate the emitted sequence first: strictly increasing `seq`.
    // (Validating `id` here used to pass while the emitted `seq` values
    // decreased, and vice versa - the two coincide only without deletions.)
    for w in entries.windows(2) {
        if w[1].seq <= w[0].seq {
            return Err(aerr(
                "AUDIT_SEQ",
                format!("ledger seq not increasing: {} then {}", w[0].seq, w[1].seq),
            ));
        }
    }
    let mut out = std::io::BufWriter::new(
        std::fs::File::create(path)
            .map_err(|e| aerr("AUDIT_WRITE", format!("create {}: {e}", path.display())))?,
    );
    let mut n = 0usize;
    for e in entries {
        let line = audit_line(e);
        out.write_all(line.as_bytes())
            .and_then(|_| out.write_all(b"\n"))
            .map_err(|e| aerr("AUDIT_WRITE", format!("write: {e}")))?;
        n += 1;
    }
    out.flush()
        .map_err(|e| aerr("AUDIT_WRITE", format!("flush: {e}")))?;
    Ok(n)
}
