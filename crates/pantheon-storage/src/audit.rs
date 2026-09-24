//! JSONL audit log: one line per ledger event, replayable and
//! sequence-validated. The eval interface — a run's full trajectory in a
//! plain file that Python (or anything) can score without opening SQLite.
//!
//! Line shape:
//!   {"seq": 7, "event": "ToolStarted", "tool": "shell", "call_id": "call_1_0", ...}
//!
//! `seq` is the ledger row id: monotonic per run. Export refuses to emit
//! a file with duplicate or decreasing seq values (a corrupted ledger
//! should fail loudly here, not downstream).
use crate::ledger::LedgerEntry;
use pantheon_core::error::{Layer, PantheonError};
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
        pantheon_core::events::Event::ToolStarted {
            call_id,
            tool,
            args,
            ..
        } => {
            obj["call_id"] = serde_json::json!(call_id);
            obj["tool"] = serde_json::json!(tool);
            obj["args"] = serde_json::json!(args);
        }
        pantheon_core::events::Event::ToolOutput {
            call_id,
            tool,
            truncated,
            ..
        } => {
            obj["call_id"] = serde_json::json!(call_id);
            obj["tool"] = serde_json::json!(tool);
            obj["truncated"] = serde_json::json!(truncated);
        }
        pantheon_core::events::Event::ToolCompleted { call_id, tool, .. } => {
            obj["call_id"] = serde_json::json!(call_id);
            obj["tool"] = serde_json::json!(tool);
        }
        pantheon_core::events::Event::ModelDelta { delta, .. } => {
            obj["delta"] = serde_json::json!(delta);
        }
        pantheon_core::events::Event::RunFailed { code, .. } => {
            obj["code"] = serde_json::json!(code);
        }
        pantheon_core::events::Event::RunCanceled { reason, .. } => {
            obj["reason"] = serde_json::json!(reason);
        }
        pantheon_core::events::Event::ApprovalRequested { scope, .. } => {
            obj["scope"] = serde_json::json!(scope);
        }
        pantheon_core::events::Event::ApprovalGranted { scope, .. }
        | pantheon_core::events::Event::ApprovalDenied { scope, .. } => {
            obj["scope"] = serde_json::json!(scope);
        }
        _ => {}
    }
    serde_json::to_string(&obj).unwrap_or_else(|_| "{}".into())
}

/// Stable snake_case names for event kinds.
fn event_name(e: &pantheon_core::events::Event) -> &'static str {
    use pantheon_core::events::Event::*;
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
        AgentMessage { .. } => "AgentMessage",
        AgentSpawned { .. } => "AgentSpawned",
        AgentCompleted { .. } => "AgentCompleted",
        _ => "Other",
    }
}

/// Export a run's ledger entries to a JSONL file. Refuses non-monotonic
/// or duplicate seq values — a corrupted ledger must fail loudly here.
pub fn export_jsonl(entries: &[LedgerEntry], path: &Path) -> Result<usize, PantheonError> {
    // Validate sequence first: strictly increasing.
    for w in entries.windows(2) {
        if w[1].id <= w[0].id {
            return Err(aerr(
                "AUDIT_SEQ",
                format!("ledger seq not increasing: {} then {}", w[0].id, w[1].id),
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

#[cfg(test)]
mod tests {
    use super::*;
    use pantheon_core::events::Event;
    use pantheon_core::provenance::Provenance;

    fn entry(id: i64, event: Event) -> LedgerEntry {
        LedgerEntry {
            id,
            run_id: "run_audit".into(),
            seq: id,
            ts_ms: 0,
            event,
        }
    }

    #[test]
    fn audit_line_carries_seq_event_and_fields() {
        let e = entry(
            7,
            Event::ToolStarted {
                run_id: "run_audit".into(),
                call_id: "call_1_0".into(),
                tool: "shell".into(),
                args: "ls".into(),
                provenance: Provenance::untrusted("shell"),
            },
        );
        let line = audit_line(&e);
        assert!(line.contains("\"seq\":7"), "{line}");
        assert!(line.contains("\"event\":\"ToolStarted\""), "{line}");
        assert!(line.contains("\"tool\":\"shell\""), "{line}");
    }

    #[test]
    fn export_validates_monotonic_seq() {
        let entries = vec![
            entry(1, Event::RunStarted { run_id: "r".into() }),
            entry(1, Event::RunCompleted { run_id: "r".into() }),
        ];
        let dir = std::env::temp_dir().join(format!("pantheon-audit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let err = export_jsonl(&entries, &dir.join("bad.jsonl")).unwrap_err();
        assert_eq!(err.code, "AUDIT_SEQ");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_writes_one_json_per_line() {
        let entries = vec![
            entry(1, Event::RunStarted { run_id: "r".into() }),
            entry(
                2,
                Event::ModelRequested {
                    run_id: "r".into(),
                    model: "m".into(),
                },
            ),
            entry(3, Event::RunCompleted { run_id: "r".into() }),
        ];
        let dir = std::env::temp_dir().join(format!("pantheon-audit-ok-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("ok.jsonl");
        let n = export_jsonl(&entries, &p).unwrap();
        assert_eq!(n, 3);
        let raw = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 3);
        for l in lines {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            assert!(v.get("seq").is_some());
            assert!(v.get("event").is_some());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
