//! Behavioral / integration tests moved out of the crate per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval`.
use pantheon_api::events::Event;
use pantheon_storage::{export_jsonl, LedgerEntry};

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
