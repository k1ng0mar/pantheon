//! Tests for the context-window fit wired into the drive loop.
//!
//! `fit_to_window` and `compress_oldest` shipped fully tested and were never
//! called from anywhere outside their own test file. These tests cover the
//! wiring itself: that a budget is derived from the *serving* model, that the
//! transcript is actually bounded, and that the two failure modes which would
//! be silent in production are refused.
use super::*;
use pantheon_exec::context::{estimate_messages, WindowBudget};

fn test_session_named(policy: ModelPolicy, tag: &str) -> (Session, std::path::PathBuf) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pantheon-rt-ctx-{}-{}-{}",
        std::process::id(),
        tag,
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let broker = pantheon_secrets::SecretsBroker::new();
    let s = Session::new(
        dir.clone(),
        pantheon_core::capability::Policy::default(),
        policy,
        broker,
    )
    .expect("session");
    (s, dir)
}

fn test_session(policy: ModelPolicy) -> (Session, std::path::PathBuf) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pantheon-rt-ctx-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let broker = pantheon_secrets::SecretsBroker::new();
    let s = Session::new(
        dir.clone(),
        pantheon_core::capability::Policy::default(),
        policy,
        broker,
    )
    .expect("session");
    (s, dir)
}

/// A chain that would fail if it were ever used to make a call.
/// `window_budget` reads only the policy and `last_resolved`, so this test
/// double needs no transport behaviour — only something to construct one.
fn empty_chain(s: &Session) -> ProviderChain<Box<dyn pantheon_providers::ChatTransport>> {
    struct Explode;
    impl pantheon_providers::ChatTransport for Explode {
        fn post(
            &self,
            _req: &pantheon_providers::http::WireRequest,
        ) -> Result<String, PantheonError> {
            panic!("window_budget must not make a provider call")
        }
        fn post_stream(
            &self,
            _req: &pantheon_providers::http::WireRequest,
            _on: &mut dyn FnMut(&str) -> Result<(), PantheonError>,
        ) -> Result<(), PantheonError> {
            panic!("window_budget must not make a provider call")
        }
    }
    ProviderChain::new(
        s.model_policy.clone(),
        Box::new(Explode),
        vec![],
        pantheon_secrets::SecretValue::new(""),
    )
}

fn openai_policy(model: &str) -> ModelPolicy {
    ModelPolicy {
        default: pantheon_core::model::DefaultModel {
            provider: "openai".into(),
            model: model.into(),
        },
        fallbacks: Default::default(),
        auxiliaries: vec![],
    }
}

/// A transcript of `n` bulky exchanges. Each user row is paired with an
/// assistant row so the exchange structure is wire-valid.
fn bulky_transcript(n: usize, per_exchange: usize) -> Vec<Message> {
    let mut out = vec![Message::system("you are a test")];
    for i in 0..n {
        out.push(Message::user(format!("question {i}")));
        out.push(Message::assistant("x".repeat(per_exchange)));
    }
    out
}

/// The headline defect: an oversized transcript must come back smaller.
/// Before the wiring, `drive` sent it to the provider untouched.
#[test]
fn an_oversized_transcript_is_actually_bounded() {
    let (s, _d) = test_session(openai_policy("gpt-4o"));
    let mut msgs = bulky_transcript(60, 8_000);
    let budget = WindowBudget::new(128_000, 16_384);
    let before = estimate_messages(&msgs);
    assert!(before > budget.usable(), "fixture must start over budget");

    let changed = s.fit_context(&mut msgs, &budget, "run_ctx");
    assert!(changed, "fit reported no change on an oversized transcript");
    assert!(
        estimate_messages(&msgs) < before,
        "fit ran but the transcript did not shrink"
    );
    assert!(
        estimate_messages(&msgs) <= budget.usable(),
        "still over budget after fitting: {} > {}",
        estimate_messages(&msgs),
        budget.usable()
    );
}

/// The system preamble and the live final exchange are what keep the request
/// coherent. Dropping either produces a request that is valid JSON and wrong.
#[test]
fn fitting_preserves_the_system_preamble_and_the_live_exchange() {
    let (s, _d) = test_session(openai_policy("gpt-4o"));
    let mut msgs = bulky_transcript(60, 8_000);
    msgs.push(Message::user("the live question"));
    msgs.push(Message::tool("call_x", "y".repeat(4_000)));

    s.fit_context(&mut msgs, &WindowBudget::new(128_000, 16_384), "run_ctx");
    assert!(
        msgs.iter().any(|m| m.content == "you are a test"),
        "system preamble was dropped"
    );
    assert!(
        msgs.iter().any(|m| m.content == "the live question"),
        "the live user row was dropped"
    );
}

/// An uncataloged model must be left completely alone.
///
/// This is the case worth a test: `model_meta` defaults `context_limit` to
/// `None`, and both wrong repairs are tempting. Assuming a small limit would
/// silently truncate a conversation on a 1M-window model; assuming no limit
/// would let the transcript grow until the provider rejects it. Doing nothing
/// is the only defensible default, so it is the one that is asserted.
#[test]
fn an_uncataloged_model_gets_no_budget_and_is_untouched() {
    let (s, _d) = test_session(openai_policy("gpt-4o-not-a-real-model"));
    let chain = empty_chain(&s);
    assert!(
        s.window_budget(&chain).is_none(),
        "an unknown model must not produce a budget"
    );
}

/// A cataloged model does, and the reserve is subtracted before the safety
/// margin, so the usable window is below the raw limit.
#[test]
fn a_cataloged_model_produces_a_budget_reserving_output_room() {
    let (s, _d) = test_session(openai_policy("gpt-4o"));
    let chain = empty_chain(&s);
    let b = s
        .window_budget(&chain)
        .expect("gpt-4o is cataloged, so it must have a budget");
    assert_eq!(b.limit, 128_000);
    assert_eq!(b.reserve_output, 16_384);
    assert!(
        b.usable() < 128_000 && b.usable() > 90_000,
        "usable {} does not reflect limit-minus-reserve-minus-margin",
        b.usable()
    );
}

/// A transcript already inside the window must come back byte-identical.
/// Re-compressing or re-ordering a healthy transcript would churn the
/// provider cache and lose detail for no reason.
#[test]
fn an_under_budget_transcript_is_left_byte_identical() {
    let (s, _d) = test_session(openai_policy("gpt-4o"));
    let msgs = bulky_transcript(2, 100);
    let mut copy = msgs.clone();
    let changed = s.fit_context(&mut copy, &WindowBudget::new(128_000, 16_384), "run_ctx");
    assert!(!changed, "an under-budget transcript was modified");
    assert_eq!(msgs, copy);
}

/// Compression is an optimization, not a dependency. With no `[compression]`
/// aux configured the fit must still bound the transcript, and must not
/// report an error.
#[test]
fn without_a_compression_aux_the_deterministic_fit_still_bounds_the_transcript() {
    let (s, _d) = test_session(openai_policy("gpt-4o"));
    assert!(
        s.model_policy
            .auxiliary(&pantheon_core::model::AuxiliaryKind::Compression)
            .is_none(),
        "fixture must have no compression aux"
    );
    let mut msgs = bulky_transcript(60, 8_000);
    let budget = WindowBudget::new(128_000, 16_384);
    s.fit_context(&mut msgs, &budget, "run_ctx");
    assert!(
        estimate_messages(&msgs) <= budget.usable(),
        "the deterministic fit is the only guarantee; without it the run dies"
    );
}

/// An unrepresentable window (the essential rows alone do not fit) must not
/// take the turn down. The provider's own error is more informative, and
/// dying here would mask it.
#[test]
fn an_unfittable_transcript_leaves_the_run_alone_instead_of_failing_it() {
    let (s, _d) = test_session(openai_policy("gpt-4o"));
    let mut msgs = bulky_transcript(6, 20_000);
    let before = msgs.clone();
    // A window smaller than a single tool row: unfittable by construction.
    let changed = s.fit_context(&mut msgs, &WindowBudget::new(200, 0), "run_ctx");
    assert!(
        !changed,
        "an unfittable transcript must not report a change"
    );
    assert_eq!(before, msgs, "the transcript must be left intact");
}

/// The ledger is the audit trail, so a trim that is not recorded there did not
/// happen as far as replay, `runs <id>`, and any extension hook are concerned.
/// Both events must land: `ContextCompressed` says meaning was preserved,
/// `ContextTrimmed` says rows were dropped. Emitting only the first would make
/// a lossy turn look lossless.
#[test]
fn a_fit_is_recorded_in_the_ledger() {
    let (s, _d) = test_session_named(openai_policy("gpt-4o"), "ledger");
    s.supervisor.start_run("run_fit").unwrap();
    let mut msgs = bulky_transcript(60, 8_000);
    let changed = s.fit_context(&mut msgs, &WindowBudget::new(128_000, 16_384), "run_fit");
    assert!(changed);

    let events = s.supervisor.replay("run_fit").unwrap();
    let trimmed: Vec<&Event> = events
        .iter()
        .map(|e| &e.event)
        .filter(|e| matches!(e, Event::ContextTrimmed { .. }))
        .collect();
    assert_eq!(trimmed.len(), 1, "expected exactly one ContextTrimmed");
    match trimmed[0] {
        Event::ContextTrimmed {
            estimated,
            window,
            dropped_rows,
            ..
        } => {
            assert!(*estimated <= *window, "recorded an over-budget fit");
            assert!(
                *dropped_rows > 0,
                "the fit dropped nothing yet was recorded"
            );
        }
        _ => unreachable!(),
    }
}
