//! Fork-a-session behavior: `Supervisor::fork_run` copies the durable event
//! prefix through a chosen user turn into a brand-new run.
//! Run with `cargo test -p pantheon-eval`.

use pantheon_api::events::Event;
use pantheon_api::message::{Message, Role};
use pantheon_runtime::Supervisor;
use tempfile::tempdir;

fn user_msg(run: &str, text: &str) -> Event {
    Event::AssistantMessage {
        run_id: run.into(),
        message: Message::user(text),
    }
}

fn assistant_msg(run: &str, text: &str) -> Event {
    Event::AssistantMessage {
        run_id: run.into(),
        message: Message::assistant(text),
    }
}

/// A three-turn conversation in the ledger: user/assistant pairs wrapped
/// in turn boundaries, plus a title.
fn seed_three_turns(sup: &Supervisor, run: &str) {
    sup.start_run(run).unwrap();
    sup.emit(Event::SessionTitled {
        run_id: run.into(),
        title: "deep talk".into(),
        model: "t".into(),
        source: "manual".into(),
    })
    .unwrap();
    for (i, (q, a)) in [("q1", "a1"), ("q2", "a2"), ("q3", "a3")]
        .into_iter()
        .enumerate()
    {
        let turn = format!("turn-{i}");
        sup.emit(Event::TurnStarted {
            run_id: run.into(),
            turn_id: turn.clone(),
        })
        .unwrap();
        sup.emit(user_msg(run, q)).unwrap();
        sup.emit(assistant_msg(run, a)).unwrap();
        sup.emit(Event::TurnCompleted {
            run_id: run.into(),
            turn_id: turn,
            outcome: "answered".into(),
        })
        .unwrap();
    }
}

fn user_texts(sup: &Supervisor, run: &str) -> Vec<String> {
    sup.replay(run)
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.event {
            Event::AssistantMessage { message, .. } if message.role == Role::User => {
                Some(message.content)
            }
            _ => None,
        })
        .collect()
}

#[test]
fn fork_at_middle_turn_copies_exactly_that_prefix() {
    let dir = tempdir().unwrap();
    let sup = Supervisor::open(dir.path().to_path_buf()).unwrap();
    seed_three_turns(&sup, "src");

    let (new_id, n) = sup.fork_run("src", Some(2)).unwrap();
    assert_eq!(n, 2);
    assert_ne!(new_id, "src");

    // The fork holds turns 1..=2 with identical content, in order.
    let texts = user_texts(&sup, &new_id);
    assert_eq!(texts, vec!["q1".to_string(), "q2".to_string()]);
    let all: Vec<Message> =
        pantheon_runtime::session::rebuild_messages(sup.replay(&new_id).unwrap());
    assert_eq!(all.len(), 4);
    assert_eq!(all[1].content, "a1");

    // The source run is untouched: still three turns.
    assert_eq!(
        user_texts(&sup, "src"),
        vec!["q1".to_string(), "q2".to_string(), "q3".to_string()]
    );

    // Provenance: fork marker names the source and the turn.
    let marker = sup
        .replay(&new_id)
        .unwrap()
        .into_iter()
        .find_map(|e| match e.event {
            Event::RunProgress { detail, .. } => Some(detail),
            _ => None,
        })
        .unwrap();
    assert!(marker.contains("src"), "marker names source: {marker}");
    assert!(marker.contains("turn 2"), "marker names turn: {marker}");

    // Title carried over and marked.
    let title = sup.ledger_title(&new_id).unwrap().unwrap();
    assert_eq!(title, "deep talk (fork)");
}

#[test]
fn fork_without_turn_forks_the_latest() {
    let dir = tempdir().unwrap();
    let sup = Supervisor::open(dir.path().to_path_buf()).unwrap();
    seed_three_turns(&sup, "src");

    let (new_id, n) = sup.fork_run("src", None).unwrap();
    assert_eq!(n, 3);
    assert_eq!(
        user_texts(&sup, &new_id),
        vec!["q1".to_string(), "q2".to_string(), "q3".to_string()]
    );
}

#[test]
fn fork_records_lineage_in_ledger() {
    let dir = tempdir().unwrap();
    let sup = Supervisor::open(dir.path().to_path_buf()).unwrap();
    seed_three_turns(&sup, "root");

    // First fork: parent is the root run.
    let (mid, _) = sup.fork_run("root", None).unwrap();
    assert_eq!(
        sup.ledger_forked_from(&mid).unwrap(),
        Some("root".to_string())
    );
    // The source run is not a fork itself.
    assert_eq!(sup.ledger_forked_from("root").unwrap(), None);

    // Fork of a fork: the pointer is the direct parent only (a chain, not a tree).
    let (leaf, _) = sup.fork_run(&mid, None).unwrap();
    assert_eq!(sup.ledger_forked_from(&leaf).unwrap(), Some(mid.clone()));

    // Walking the chain reaches the root in exactly two hops.
    let mut cursor = Some(leaf);
    let mut hops = 0;
    while let Some(run) = cursor {
        cursor = sup.ledger_forked_from(&run).unwrap();
        hops += 1;
        assert!(hops <= 4, "lineage chain should be short");
    }
    assert_eq!(hops, 3, "leaf -> mid -> root -> None");

    // Rebinding a run that already has a parent is a hard error.
    let err = sup.ledger_forked_from(&mid).unwrap(); // sanity: still the original parent
    assert_eq!(err, Some("root".to_string()));
}

#[test]
fn fork_rejects_bad_turns_and_empty_runs() {
    let dir = tempdir().unwrap();
    let sup = Supervisor::open(dir.path().to_path_buf()).unwrap();
    seed_three_turns(&sup, "src");

    let err = sup.fork_run("src", Some(0)).unwrap_err();
    assert_eq!(err.code, "RT_FORK_RANGE");
    let err = sup.fork_run("src", Some(4)).unwrap_err();
    assert_eq!(err.code, "RT_FORK_RANGE");
    // Source still intact after rejected forks.
    assert_eq!(user_texts(&sup, "src").len(), 3);

    sup.start_run("empty").unwrap();
    let err = sup.fork_run("empty", None).unwrap_err();
    assert_eq!(err.code, "RT_FORK_EMPTY");
}

#[test]
fn with_run_id_preserves_everything_but_the_address() {
    let ev = Event::ToolStarted {
        run_id: "old".into(),
        call_id: "call_1".into(),
        tool: "shell.exec".into(),
        args: "ls".into(),
        provenance: pantheon_api::provenance::Provenance {
            source: "model".into(),
            origin_seq: None,
            trust: pantheon_api::provenance::TrustTier::User,
        },
    };
    let moved = ev.with_run_id("new");
    match moved {
        Event::ToolStarted {
            run_id,
            call_id,
            tool,
            args,
            ..
        } => {
            assert_eq!(run_id, "new");
            assert_eq!(call_id, "call_1");
            assert_eq!(tool, "shell.exec");
            assert_eq!(args, "ls");
        }
        other => panic!("wrong variant: {other:?}"),
    }
    // The original is unchanged.
    match ev {
        Event::ToolStarted { run_id, .. } => assert_eq!(run_id, "old"),
        _ => unreachable!(),
    }
}
