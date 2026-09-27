//! Behavioral / integration tests moved out of the crate's unit suite.
//!
//! Policy: only small deterministic unit tests live beside the code
//! (`cargo test -p <crate>`). Everything behavioral — SQLite stores,
//! threads, sockets, subprocesses, timing, filesystem — lives here and
//! runs via `cargo test -p pantheon-eval`.

use pantheon_runtime::session::*;
use pantheon_runtime::Supervisor;
use pantheon_api::capability::Policy;
use pantheon_api::events::Event;
use pantheon_tools::tools::ToolRegistry;



/// Phase 5: three 400ms tool calls in one turn must finish in well under
/// a second if they run concurrently (sequential would be >=1.2s).
#[test]
fn parallel_tool_calls_overlap_in_wall_time() {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Instant;

    let mut reg = ToolRegistry::new();
    let counter = std::sync::Arc::new(AtomicU32::new(0));
    let c2 = counter.clone();
    for i in 0..3 {
        let c = c2.clone();
        reg.register(
            pantheon_api::message::ToolSchema {
                name: format!("slow_{i}"),
                description: "sleeps 400ms".into(),
                parameters: serde_json::json!({}),
            },
            pantheon_api::capability::Capability::ShellExecute,
            move |_args| {
                c.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(400));
                Ok(format!("done_{i}"))
            },
        );
    }
    // Three calls the model "asked for" in one turn.
    let calls: Vec<pantheon_agent::ToolCall> = (0..3)
        .map(|i| pantheon_agent::ToolCall {
            name: format!("slow_{i}"),
            capability: pantheon_api::capability::Capability::ShellExecute,
            args: "{}".into(),
        })
        .collect();
    // Execute the same way drive() does: scoped threads over the registry.
    let t0 = Instant::now();
    let results: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = calls
            .iter()
            .map(|c| {
                let r = &reg;
                s.spawn(move || r.execute(&c.name, &c.args))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>()
    });
    let elapsed = t0.elapsed();
    for (i, r) in results.iter().enumerate() {
        assert_eq!(r.as_ref().unwrap(), &format!("done_{i}"));
    }
    assert_eq!(counter.load(Ordering::SeqCst), 3);
    // Sequential would be >= 1.2s. Parallel: < 0.9s with slack.
    assert!(
        elapsed < std::time::Duration::from_millis(900),
        "calls ran sequentially: {elapsed:?}"
    );
}


#[test]
fn denied_scope_settles_instead_of_reparking_on_resume() {
    // A denied call must not leave the run stuck: on resume the denial
    // becomes a transcript tool result and the run can finish.
    let dir = std::env::temp_dir().join(format!("pantheon-deny-settle-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sup = Supervisor::open(dir).unwrap();
    sup.start_run("deny-resume").unwrap();
    sup.emit(Event::ApprovalRequested {
        run_id: "deny-resume".into(),
        scope: "call_0_0".into(),
    })
    .unwrap();
    sup.deny("deny-resume", "call_0_0").unwrap();
    // The denial scope is visible on replay and must be excluded from
    // re-parking by the drive() partition logic.
    let entries = sup.replay("deny-resume").unwrap();
    let denied: Vec<String> = entries
        .iter()
        .filter_map(|e| match &e.event {
            Event::ApprovalDenied { scope, .. } => Some(scope.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(denied, vec!["call_0_0".to_string()]);
    // Status flipped back to running (not parked) after the denial.
    assert_eq!(
        sup.ledger_status("deny-resume").unwrap().as_deref(),
        Some("running")
    );
}


#[test]
fn switch_model_rejects_empty_and_trims() {
    let s = switch_test_session("reject");
    assert!(s.switch_model("", "m").is_err(), "empty provider refused");
    assert!(s.switch_model("p", "  ").is_err(), "blank model refused");
    assert_eq!(
        s.default_model(),
        ("test".into(), "test".into()),
        "refused switch changes nothing"
    );
    s.switch_model("  openai  ", "  gpt-4o-mini  ").unwrap();
    assert_eq!(
        s.default_model(),
        ("openai".into(), "gpt-4o-mini".into()),
        "surrounding whitespace trimmed"
    );
}


#[test]
fn compress_now_reports_cleanly_on_an_empty_run() {
    let s = switch_test_session("compress-empty");
    // A cataloged model with an empty transcript: the fit short-circuits
    // before any model call, so no key or network is needed.
    s.switch_model("openai", "gpt-4o-mini").unwrap();
    let run = "run_compress_empty";
    s.supervisor.start_run(run).unwrap();
    let rep = s.compress_now(run).unwrap();
    assert!(!rep.changed, "nothing to fit on an empty transcript");
    assert!(
        !rep.unknown_window,
        "openai/gpt-4o-mini has a catalog window"
    );
    assert_eq!((rep.before, rep.after), (0, 0));
}


#[test]
fn compress_now_refuses_without_a_known_window() {
    let s = switch_test_session("compress-unknown");
    s.switch_model("some-uncataloged-provider", "mystery-model")
        .unwrap();
    let run = "run_compress_unknown";
    s.supervisor.start_run(run).unwrap();
    let rep = s.compress_now(run).unwrap();
    assert!(
        rep.unknown_window,
        "no catalog window means no fit, not a guessed one"
    );
}


#[test]
fn new_session_starts_at_depth_zero() {
    // The top-level loop must report depth 0: `chat_turn` builds the
    // loop from `self.depth`, and a fresh session has no parent.
    let s = drive_test_session("depth-zero");
    assert_eq!(s.depth, 0);
    let _ = std::fs::remove_dir_all(s.supervisor.data_dir());
}


fn switch_test_session(tag: &str) -> Session {
    let dir = std::env::temp_dir().join(format!("pantheon-switch-{tag}-{}", std::process::id()));
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
            fallbacks: vec![pantheon_api::model::DefaultModel {
                provider: "fb".into(),
                model: "fb-model".into(),
            }],
        },
        auxiliaries: Vec::new(),
    };
    let secrets = pantheon_secrets::SecretsBroker::new();
    Session::new(dir, policy, model_policy, secrets).unwrap()
}


fn drive_test_session(tag: &str) -> Session {
    let dir = std::env::temp_dir().join(format!("pantheon-rt-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    Session::new(
        dir,
        Policy::coder(),
        drive_test_policy(),
        pantheon_secrets::SecretsBroker::from_system_env(),
    )
    .unwrap()
}


fn drive_test_policy() -> pantheon_api::model::ModelPolicy {
    pantheon_api::model::ModelPolicy {
        default: pantheon_api::model::DefaultModel {
            provider: "openai".into(),
            model: "test-model".into(),
        },
        fallbacks: pantheon_api::model::FallbackChain {
            fallbacks: Vec::new(),
        },
        auxiliaries: Vec::new(),
        reasoning: pantheon_api::model::ReasoningLevel::default(),
        reasoning_budget: None,
    }
}

