//! Mid-turn steering: operator guidance injected into a running turn
//! without canceling it and without waiting for it to end.
//! Run with `cargo test -p pantheon-eval`.

use pantheon_api::events::Event;
use pantheon_api::message::Role;
use pantheon_api::provenance::TrustTier;
use pantheon_runtime::session::{rebuild_messages, steering_content, Session};

fn test_session(tag: &str) -> (Session, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("pantheon-steer-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let policy = pantheon_api::capability::Policy::coder();
    let model_policy = pantheon_api::model::ModelPolicy {
        reasoning_budget: Default::default(),
        reasoning: Default::default(),
        default: pantheon_api::model::DefaultModel {
            provider: "test".into(),
            model: "test".into(),
        },
        fallbacks: pantheon_api::model::FallbackChain {
            fallbacks: Vec::new(),
        },
        auxiliaries: Vec::new(),
    };
    let secrets = pantheon_secrets::SecretsBroker::from_system_env();
    let s = Session::new(dir.clone(), policy, model_policy, secrets).unwrap();
    (s, dir)
}

/// Steering is a FIFO inbox: multiple steers in one turn are all
/// delivered, oldest first, and the inbox is empty afterwards.
#[test]
fn steer_inbox_is_fifo_and_drains_clean() {
    let (s, dir) = test_session("fifo");
    s.steer("first");
    s.steer("second");
    s.steer("third");
    assert_eq!(s.drain_steers(), vec!["first", "second", "third"]);
    assert!(
        s.drain_steers().is_empty(),
        "draining twice must not duplicate"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Steering must never trip the cancel token: the turn keeps running,
/// in-flight tool calls complete, and only the guidance is added.
#[test]
fn steer_never_cancels_the_run() {
    let (s, dir) = test_session("nocancel");
    s.steer("redirect this");
    assert!(!s.is_canceled(), "steering must not signal cancellation");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `deliver_steers` (the turn-boundary path `drive` calls) records one
/// durable `SteeringProvided` row per guidance and appends a marked
/// User-tier message the model reads on its next step.
#[test]
fn deliver_steers_records_ledger_and_marked_messages() {
    let (s, dir) = test_session("deliver");
    let run = "run_steer_deliver";
    s.supervisor.start_run(run).unwrap();

    s.steer("use tabs not spaces");
    s.steer("and skip the tests dir");
    let mut messages = Vec::new();
    s.deliver_steers(&mut messages, run).unwrap();

    assert_eq!(messages.len(), 2, "both steers delivered in order");
    for (m, text) in messages
        .iter()
        .zip(["use tabs not spaces", "and skip the tests dir"])
    {
        assert_eq!(m.role, Role::User);
        assert_eq!(m.content, steering_content(text));
        assert!(
            m.content.contains(text),
            "guidance text must survive verbatim"
        );
        assert_ne!(m.content, text, "steering must be marked, not a bare echo");
        assert_eq!(
            m.provenance.as_ref().map(|p| p.trust),
            Some(TrustTier::User),
            "steering is a direct operator instruction"
        );
    }

    let provided: Vec<String> = s
        .supervisor
        .replay(run)
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.event {
            Event::SteeringProvided { text, .. } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(
        provided,
        vec!["use tabs not spaces", "and skip the tests dir"],
        "ledger holds the steering record in order"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Resume fidelity: a `SteeringProvided` row rebuilds into exactly the
/// message the model saw mid-turn, in position.
#[test]
fn rebuild_messages_restores_steering_in_order() {
    let (s, dir) = test_session("rebuild");
    let run = "run_steer_rebuild";
    s.supervisor.start_run(run).unwrap();
    let user = |t: &str| Event::AssistantMessage {
        run_id: run.into(),
        message: pantheon_api::message::Message::user(t),
    };
    s.supervisor.emit(user("write a server")).unwrap();
    s.supervisor
        .emit(Event::SteeringProvided {
            run_id: run.into(),
            text: "actually make it a CLI".into(),
        })
        .unwrap();
    s.supervisor
        .emit(Event::AssistantMessage {
            run_id: run.into(),
            message: pantheon_api::message::Message::assistant("done"),
        })
        .unwrap();

    let msgs = rebuild_messages(s.supervisor.replay(run).unwrap());
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0].content, "write a server");
    assert_eq!(msgs[1].content, steering_content("actually make it a CLI"));
    assert_eq!(msgs[1].role, Role::User);
    assert_eq!(
        msgs[1].provenance.as_ref().map(|p| p.trust),
        Some(TrustTier::User)
    );
    assert_eq!(msgs[2].content, "done");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Steering with nothing queued delivers nothing and touches nothing:
/// no phantom rows, no messages.
#[test]
fn deliver_with_empty_inbox_is_a_noop() {
    let (s, dir) = test_session("noop");
    let run = "run_steer_noop";
    s.supervisor.start_run(run).unwrap();
    let mut messages = Vec::new();
    s.deliver_steers(&mut messages, run).unwrap();
    assert!(messages.is_empty());
    assert!(
        s.supervisor
            .replay(run)
            .unwrap()
            .iter()
            .all(|e| !matches!(e.event, Event::SteeringProvided { .. })),
        "no steering rows without steering"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
