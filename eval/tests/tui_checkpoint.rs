//! Durable checkpoints: `CheckpointCreated` markers are listed and restorable.
//!
//! Behavioral / integration tests per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval`.
use pantheon_api::events::Event;
use pantheon_api::message::Message;
use pantheon_storage::{Ledger, LedgerEntry};
use pantheon_tui::checkpoint as cp;

fn append_turn(ledger: &Ledger, run: &str, turn_id: &str, text: &str) {
    ledger
        .append(&Event::TurnStarted {
            run_id: run.into(),
            turn_id: turn_id.into(),
        })
        .unwrap();
    ledger
        .append(&Event::AssistantMessage {
            run_id: run.into(),
            message: Message::assistant(text),
        })
        .unwrap();
    ledger
        .append(&Event::TurnCompleted {
            run_id: run.into(),
            turn_id: turn_id.into(),
            outcome: "ok".into(),
        })
        .unwrap();
}

fn replay(ledger: &Ledger, run: &str) -> Vec<LedgerEntry> {
    ledger.replay(run).unwrap()
}

fn checkpoint(ledger: &Ledger, run: &str, turn_id: &str, name: &str) {
    ledger
        .append(&Event::CheckpointCreated {
            run_id: run.into(),
            turn_id: turn_id.into(),
            name: name.into(),
        })
        .unwrap();
}

#[test]
fn checkpoint_is_created_and_listed_with_turn_number() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .append(&Event::RunStarted { run_id: "r".into() })
        .unwrap();
    append_turn(&ledger, "r", "t1", "first");
    append_turn(&ledger, "r", "t2", "second");
    checkpoint(&ledger, "r", "t2", "a");

    let cps = cp::list_checkpoints(&replay(&ledger, "r"));
    assert_eq!(cps.len(), 1);
    assert_eq!(cps[0].name, "a");
    assert_eq!(cps[0].turn_id, "t2");
    assert_eq!(cps[0].turn_no, 2);
}

#[test]
fn checkpoint_duplicate_name_is_detected() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .append(&Event::RunStarted { run_id: "r".into() })
        .unwrap();
    append_turn(&ledger, "r", "t1", "first");
    checkpoint(&ledger, "r", "t1", "a");

    let entries = replay(&ledger, "r");
    assert!(cp::checkpoint_exists(&entries, "a"));
    assert!(!cp::checkpoint_exists(&entries, "b"));
    // Auto-names skip taken slots.
    assert_eq!(cp::next_auto_name(&entries), "cp-2");
    checkpoint(&ledger, "r", "t1", "cp-2");
    assert_eq!(cp::next_auto_name(&replay(&ledger, "r")), "cp-3");
}

#[test]
fn restore_plan_targets_the_turn_after_the_checkpoint() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .append(&Event::RunStarted { run_id: "r".into() })
        .unwrap();
    append_turn(&ledger, "r", "t1", "first");
    append_turn(&ledger, "r", "t2", "second");
    append_turn(&ledger, "r", "t3", "third");
    checkpoint(&ledger, "r", "t2", "a");

    let plan = cp::plan_restore(&replay(&ledger, "r"), "a").unwrap();
    // The checkpoint turn itself is kept: the rewind names the next turn.
    assert_eq!(plan.keep_turn_no, 2);
    assert_eq!(plan.drop_from_turn_no, 3);
    assert_eq!(plan.rewind_target, "t3");
    assert_eq!(plan.dropped, 1);
}

#[test]
fn restore_truncates_replay_to_the_checkpoint_turn() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .append(&Event::RunStarted { run_id: "r".into() })
        .unwrap();
    append_turn(&ledger, "r", "t1", "first");
    append_turn(&ledger, "r", "t2", "second");
    // Checkpoint names the current (latest) turn, as `/checkpoint` does.
    checkpoint(&ledger, "r", "t2", "a");
    append_turn(&ledger, "r", "t3", "third");

    // This is exactly what `/restore a` emits after planning.
    let plan = cp::plan_restore(&replay(&ledger, "r"), "a").unwrap();
    ledger
        .append(&Event::TurnRewound {
            run_id: "r".into(),
            turn_id: plan.rewind_target,
        })
        .unwrap();

    let entries = replay(&ledger, "r");
    assert_eq!(
        cp::effective_turn_ids(&entries),
        vec!["t1".to_string(), "t2".to_string()]
    );
    let texts: Vec<String> = entries
        .iter()
        .filter_map(|e| match &e.event {
            Event::AssistantMessage { message, .. } => Some(message.content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["first".to_string(), "second".to_string()]);
    // The checkpoint marker survives the restore and stays usable.
    assert_eq!(cp::list_checkpoints(&entries).len(), 1);
}

#[test]
fn restore_at_latest_turn_is_rejected() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .append(&Event::RunStarted { run_id: "r".into() })
        .unwrap();
    append_turn(&ledger, "r", "t1", "first");
    append_turn(&ledger, "r", "t2", "second");
    checkpoint(&ledger, "r", "t2", "a");

    let err = cp::plan_restore(&replay(&ledger, "r"), "a").unwrap_err();
    assert!(err.contains("already at checkpoint"), "unexpected: {err}");
}

#[test]
fn restore_unknown_checkpoint_is_rejected() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .append(&Event::RunStarted { run_id: "r".into() })
        .unwrap();
    append_turn(&ledger, "r", "t1", "first");

    let err = cp::plan_restore(&replay(&ledger, "r"), "nope").unwrap_err();
    assert!(err.contains("no checkpoint named"), "unexpected: {err}");
}

#[test]
fn checkpoint_in_rewound_range_disappears_from_effective_history() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .append(&Event::RunStarted { run_id: "r".into() })
        .unwrap();
    append_turn(&ledger, "r", "t1", "first");
    append_turn(&ledger, "r", "t2", "second");
    checkpoint(&ledger, "r", "t1", "a");
    // Rewinding t1 removes everything from its TurnStarted onward,
    // including the checkpoint marker: history is hidden, not rewritten.
    ledger
        .append(&Event::TurnRewound {
            run_id: "r".into(),
            turn_id: "t1".into(),
        })
        .unwrap();

    let entries = replay(&ledger, "r");
    assert!(cp::list_checkpoints(&entries).is_empty());
    let err = cp::plan_restore(&entries, "a").unwrap_err();
    assert!(err.contains("no checkpoint named"), "unexpected: {err}");
}
