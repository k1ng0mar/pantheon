//! Tests for `pantheon_runtime::session::cancel_tests` — sibling file so sources stay test-free.
use super::*;

fn test_session(tag: &str) -> (Session, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("pantheon-cancel-{tag}-{}", std::process::id()));
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

/// The cancel token starts clear, and reset_cancel clears it again.
#[test]
fn cancel_token_lifecycle() {
    let (s, _dir) = test_session("tok");
    assert!(!s.is_canceled(), "fresh session is not canceled");
    s.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(s.is_canceled(), "token observed");
    s.reset_cancel();
    assert!(!s.is_canceled(), "reset clears the token");
}

/// Cancelling a running run records the intent durably and leaves the
/// run in `canceled` — not `failed` — so replay can tell the difference
/// and `reopen_run` can bring it back.
#[test]
fn cancel_marks_run_canceled_and_recoverable() {
    let (s, dir) = test_session("mark");
    let run = "run_cancel_me";
    s.supervisor.start_run(run).unwrap();
    assert_eq!(
        s.supervisor.ledger_status(run).unwrap().as_deref(),
        Some("running")
    );

    s.cancel_current_run(run, "user pressed esc twice");

    assert_eq!(
        s.supervisor.ledger_status(run).unwrap().as_deref(),
        Some("canceled"),
        "cancel is terminal and distinct from failed"
    );
    assert!(s.is_canceled(), "loop token signaled");

    // Recoverable: the run reopens and the transcript is intact.
    assert!(s.supervisor.ledger_reopen_run(run).unwrap());
    assert_eq!(
        s.supervisor.ledger_status(run).unwrap().as_deref(),
        Some("running")
    );
    let entries = s.supervisor.replay(run).unwrap();
    assert!(
        entries
            .iter()
            .any(|e| matches!(e.event, Event::RunCanceled { .. })),
        "cancel intent is in the ledger trail"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A cancel that arrives after the run already finished is rejected
/// rather than clobbering a terminal state.
#[test]
fn cancel_after_completion_is_rejected() {
    let (s, dir) = test_session("done");
    let run = "run_already_done";
    s.supervisor.start_run(run).unwrap();
    s.supervisor.complete(run).unwrap();
    let err = s.supervisor.cancel_run_intent(run, "too late").unwrap_err();
    assert_eq!(err.code, "RT_TERMINAL", "completed runs are not cancelable");
    assert_eq!(
        s.supervisor.ledger_status(run).unwrap().as_deref(),
        Some("completed"),
        "terminal state preserved"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
