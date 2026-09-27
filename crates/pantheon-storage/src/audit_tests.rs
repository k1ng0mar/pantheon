//! Tests for `pantheon_storage::audit::tests` — sibling file so sources stay test-free.
use super::*;
use pantheon_api::events::Event;
use pantheon_api::provenance::Provenance;

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
