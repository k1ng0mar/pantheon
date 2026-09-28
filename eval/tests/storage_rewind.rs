//! Durable rewind: `TurnRewound` markers are honored by replay.
//!
//! Behavioral / integration tests per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval`.
use pantheon_api::events::Event;
use pantheon_api::message::Message;
use pantheon_storage::Ledger;

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

fn assistant_texts(ledger: &Ledger, run: &str) -> Vec<String> {
    ledger
        .replay(run)
        .unwrap()
        .iter()
        .filter_map(|e| match &e.event {
            Event::AssistantMessage { message, .. } => Some(message.content.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn rewound_turn_is_excluded_from_replay() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .append(&Event::RunStarted {
            run_id: "r".into(),
        })
        .unwrap();
    append_turn(&ledger, "r", "t1", "first answer");
    append_turn(&ledger, "r", "t2", "second answer");
    ledger
        .append(&Event::TurnRewound {
            run_id: "r".into(),
            turn_id: "t2".into(),
        })
        .unwrap();

    let texts = assistant_texts(&ledger, "r");
    assert_eq!(texts, vec!["first answer".to_string()]);

    // The marker itself survives in the effective history (audit trail).
    let kinds: Vec<&str> = ledger
        .replay("r")
        .unwrap()
        .iter()
        .map(|e| match &e.event {
            Event::RunStarted { .. } => "started",
            Event::TurnStarted { .. } => "turn_started",
            Event::AssistantMessage { .. } => "assistant",
            Event::TurnCompleted { .. } => "turn_done",
            Event::TurnRewound { .. } => "rewound",
            _ => "other",
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "started",
            "turn_started",
            "assistant",
            "turn_done",
            "rewound"
        ]
    );
}

#[test]
fn turns_started_after_rewind_replay_normally() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .append(&Event::RunStarted {
            run_id: "r".into(),
        })
        .unwrap();
    append_turn(&ledger, "r", "t1", "first answer");
    append_turn(&ledger, "r", "t2", "bad answer");
    ledger
        .append(&Event::TurnRewound {
            run_id: "r".into(),
            turn_id: "t2".into(),
        })
        .unwrap();
    // Operator retries after the rewind: a brand-new turn.
    append_turn(&ledger, "r", "t3", "better answer");

    let texts = assistant_texts(&ledger, "r");
    assert_eq!(texts, vec!["first answer".to_string(), "better answer".to_string()]);
}

#[test]
fn rewind_marker_for_unknown_turn_keeps_history() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .append(&Event::RunStarted {
            run_id: "r".into(),
        })
        .unwrap();
    append_turn(&ledger, "r", "t1", "first answer");
    // Defensive: a marker naming a turn with no TurnStarted must not
    // truncate anything; the marker itself is still recorded.
    ledger
        .append(&Event::TurnRewound {
            run_id: "r".into(),
            turn_id: "nope".into(),
        })
        .unwrap();

    let texts = assistant_texts(&ledger, "r");
    assert_eq!(texts, vec!["first answer".to_string()]);
    assert!(ledger
        .replay("r")
        .unwrap()
        .iter()
        .any(|e| matches!(e.event, Event::TurnRewound { .. })));
}

#[test]
fn double_rewind_rolls_back_two_turns() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .append(&Event::RunStarted {
            run_id: "r".into(),
        })
        .unwrap();
    append_turn(&ledger, "r", "t1", "first");
    append_turn(&ledger, "r", "t2", "second");
    append_turn(&ledger, "r", "t3", "third");
    ledger
        .append(&Event::TurnRewound {
            run_id: "r".into(),
            turn_id: "t3".into(),
        })
        .unwrap();
    ledger
        .append(&Event::TurnRewound {
            run_id: "r".into(),
            turn_id: "t2".into(),
        })
        .unwrap();

    let texts = assistant_texts(&ledger, "r");
    assert_eq!(texts, vec!["first".to_string()]);
}

#[test]
fn run_log_shows_rewind_marker_not_rewound_turns() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .append(&Event::RunStarted {
            run_id: "r".into(),
        })
        .unwrap();
    append_turn(&ledger, "r", "t1", "kept");
    append_turn(&ledger, "r", "t2", "discarded");
    ledger
        .append(&Event::TurnRewound {
            run_id: "r".into(),
            turn_id: "t2".into(),
        })
        .unwrap();

    let log = ledger.render_run_log("r").unwrap();
    assert!(log.contains("turn rewound: t2"), "log shows marker: {log}");
    assert!(
        !log.contains("turn started: t2") && !log.contains("turn completed: t2"),
        "no t2 turn lines remain: {log}"
    );
}
