//! Tests for `crate::model_event::tests` — sibling file so sources stay test-free.
use super::*;

#[test]
fn attempt_projects_to_model_requested() {
    let ev = ModelEvent::Attempt {
        provider: "openai".into(),
        model: "gpt-test".into(),
        chain_index: 0,
        streaming: true,
    };
    match ev.to_event("r1") {
        Some(pantheon_api::events::Event::ModelRequested { run_id, model }) => {
            assert_eq!(run_id, "r1");
            assert_eq!(model, "gpt-test");
        }
        other => panic!("unexpected projection: {other:?}"),
    }
}

#[test]
fn text_delta_projects_and_reasoning_does_not() {
    let delta = ModelEvent::TextDelta { text: "hi".into() };
    assert!(matches!(
        delta.to_event("r1"),
        Some(pantheon_api::events::Event::ModelDelta { .. })
    ));
    let think = ModelEvent::ReasoningDelta { text: "hmm".into() };
    assert!(think.to_event("r1").is_none());
}

#[test]
fn fallback_projects_to_run_progress() {
    let ev = ModelEvent::Fallback {
        from_index: 0,
        from_provider: "openai".into(),
        from_model: "gpt".into(),
        from_code: "PROVIDER_HTTP".into(),
        to_index: 1,
        to_provider: "deepseek".into(),
        to_model: "ds".into(),
    };
    match ev.to_event("r1") {
        Some(pantheon_api::events::Event::RunProgress { detail, .. }) => {
            assert!(detail.contains("fallback"));
            assert!(detail.contains("deepseek"));
            assert!(detail.contains("PROVIDER_HTTP"));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn closure_sink_works() {
    use std::cell::RefCell;
    let seen: RefCell<Vec<String>> = RefCell::new(vec![]);
    let sink = |ev: ModelEvent| {
        if let ModelEvent::TextDelta { text } = ev {
            seen.borrow_mut().push(text);
        }
    };
    sink.emit(ModelEvent::TextDelta { text: "a".into() });
    sink.emit(ModelEvent::Completed {
        finish_reason: Some("stop".into()),
    });
    assert_eq!(*seen.borrow(), vec!["a".to_string()]);
}
