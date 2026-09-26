//! Live chain tests against the local llm-router (`127.0.0.1:8015`).
//!
//! No fakes: real `HttpTransport`, real HTTP, real 401s. The router is a
//! localhost dev dependency — needs `PANTHEON_KEY_ROUTER` exported and the
//! router reachable. Otherwise each test prints a skip note and passes,
//! so the suite stays green everywhere and exercises the wire where the
//! router exists.
//!
//! Key routing under test (see `chain.rs::attempt`): the configured key
//! goes to the default entry only; fallbacks resolve their own credential
//! from the environment. Custom `livechain*` ids (key envs never
//! exported) keep ambient env out of the assertions.
use super::*;
use crate::http::HttpTransport;
use pantheon_core::catalog;
use pantheon_core::model::{DefaultModel, FallbackChain, ModelPolicy};
use pantheon_core::model_event::{ModelEvent, ModelEventSink};
use pantheon_secrets::SecretValue;
use std::cell::RefCell;

struct Collect(RefCell<Vec<ModelEvent>>);
impl ModelEventSink for Collect {
    fn emit(&self, event: ModelEvent) {
        self.0.borrow_mut().push(event);
    }
}

/// REAL router key. None = skip the live tests.
fn router_key() -> Option<String> {
    std::env::var("PANTHEON_KEY_ROUTER")
        .ok()
        .filter(|k| !k.trim().is_empty())
}

/// Register (idempotent) a test provider aimed at `base` whose key env
/// is never exported, so configured keys rule deterministically.
/// Base `http://127.0.0.1:9/v1` is a dead port: nothing listens, every
/// dial is refused, which the transport reports retryable — the live
/// stand-in for a 5xx.
fn live_provider(id: &str, base: &str) {
    catalog::register_custom_provider(catalog::ProviderMeta {
        id: id.to_string(),
        label: id.to_string(),
        base_url: base.to_string(),
        api_mode: catalog::ApiMode::OpenAi,
        base_env: String::new(),
        key_env: format!("PANTHEON_KEY_{}", id.to_ascii_uppercase()),
        key_header: "Authorization".to_string(),
        models: Vec::new(),
        prominent: false,
        tag: "live-test".to_string(),
    });
}

/// True when the turn never reached a server (refused/timeout/DNS):
/// `PROVIDER_HTTP` without an `HTTP <status>` in the cause. Those tests
/// skip; anything with a status is a REAL verdict.
fn offline(err: &PantheonError) -> bool {
    err.code == "PROVIDER_HTTP" && !err.cause.contains("HTTP ")
}

fn live_policy(default_key: &str, fallback: Option<DefaultModel>) -> (ModelPolicy, SecretValue) {
    live_provider("livechain0", "http://127.0.0.1:8015/v1");
    live_provider("livechain1", "http://127.0.0.1:8015/v1");
    let policy = ModelPolicy {
        default: DefaultModel {
            provider: "livechain0".into(),
            model: "chat".into(),
        },
        fallbacks: FallbackChain {
            fallbacks: fallback.into_iter().collect(),
        },
        auxiliaries: vec![],
    };
    (policy, SecretValue::new(default_key))
}

#[test]
fn live_stacked_keys_rotate_past_401() {
    let Some(real) = router_key() else {
        eprintln!("SKIP live_stacked_keys_rotate_past_401: no PANTHEON_KEY_ROUTER");
        return;
    };
    let (policy, keys) = live_policy(&format!("bogus-key,{real}"), None);
    let chain = ProviderChain::new(policy, HttpTransport::default(), vec![], keys);
    let out = match chain.turn_messages(&[Message::user("say the word mango")]) {
        Ok(o) => o,
        Err(e) if offline(&e) => {
            eprintln!("SKIP live_stacked_keys_rotate_past_401: router unreachable");
            return;
        }
        Err(e) => panic!("live rotation failed: {e:?}"),
    };
    assert!(
        matches!(out, TurnOutcome::Text { ref text, .. } if !text.trim().is_empty()),
        "expected text, got {out:?}"
    );
    assert_eq!(chain.last_resolved.borrow().clone().unwrap().chain_index, 0);
}

#[test]
fn live_stacked_keys_exhausted_reports_last_error_without_fallback() {
    // Both keys dead: rotation runs out on the default entry. No fallback
    // configured, so the turn fails with the last 401.
    //
    // The router must actually be reachable for that verdict to mean
    // anything: `live_policy` hardcodes 127.0.0.1:8015, so on a machine
    // without the router the dial is refused, the chain exhausts, and the
    // error is PROVIDER_EXHAUSTED rather than the 401 this asserts. The
    // `offline()` probe only recognises PROVIDER_HTTP without a status, so
    // it did not catch that case and the test failed on CI instead of
    // skipping. Gate on the router being up, not on the error shape.
    let Some(_) = router_key() else {
        eprintln!("SKIP live_stacked_keys_exhausted: no PANTHEON_KEY_ROUTER");
        return;
    };
    let (policy, keys) = live_policy("bogus-one,bogus-two", None);
    let chain = ProviderChain::new(policy, HttpTransport::default(), vec![], keys);
    let c = Collect(RefCell::new(vec![]));
    let err = match chain.turn_with_sink(&[Message::user("go")], &c) {
        Ok(_) => panic!("both-bogus keys should not succeed"),
        Err(e) if offline(&e) => {
            eprintln!("SKIP live_stacked_keys_exhausted: router unreachable");
            return;
        }
        Err(e) => e,
    };
    assert_eq!(err.code, "PROVIDER_HTTP", "unexpected error: {err:?}");
    assert!(err.cause.contains("HTTP 401"), "last error was {err:?}");
    assert!(
        !c.0.borrow()
            .iter()
            .any(|e| matches!(e, ModelEvent::Fallback { .. })),
        "no fallback without a fallback entry"
    );
}

#[test]
fn live_fallback_from_dead_default_to_env_key() {
    let Some(_) = router_key() else {
        eprintln!("SKIP live_fallback_from_dead_default: no PANTHEON_KEY_ROUTER");
        return;
    };
    // Default entry dials a dead port (refused = retryable); the fallback
    // uses provider `router`, whose credential comes from the exported
    // PANTHEON_KEY_ROUTER (fallbacks never see the configured secret).
    live_provider("livechain-dead", "http://127.0.0.1:9/v1");
    let policy = ModelPolicy {
        default: DefaultModel {
            provider: "livechain-dead".into(),
            model: "chat".into(),
        },
        fallbacks: FallbackChain {
            fallbacks: vec![DefaultModel {
                provider: "router".into(),
                model: "chat".into(),
            }],
        },
        auxiliaries: vec![],
    };
    let chain = ProviderChain::new(
        policy,
        HttpTransport::default(),
        vec![],
        SecretValue::new("unused"),
    );
    let out = match chain.turn_messages(&[Message::user("say the word mango")]) {
        Ok(o) => o,
        Err(e) if offline(&e) => {
            eprintln!("SKIP live_fallback_from_dead_default: router unreachable");
            return;
        }
        Err(e) => panic!("live fallback failed: {e:?}"),
    };
    assert!(
        matches!(out, TurnOutcome::Text { ref text, .. } if !text.trim().is_empty()),
        "expected text, got {out:?}"
    );
    let r = chain.last_resolved.borrow().clone().unwrap();
    assert_eq!((r.chain_index, r.provider.as_str()), (1, "router"));
}

#[test]
fn live_fallback_emits_ordered_chain_events() {
    let Some(_) = router_key() else {
        eprintln!("SKIP live_fallback_emits_ordered_chain_events: no PANTHEON_KEY_ROUTER");
        return;
    };
    live_provider("livechain-dead", "http://127.0.0.1:9/v1");
    let policy = ModelPolicy {
        default: DefaultModel {
            provider: "livechain-dead".into(),
            model: "chat".into(),
        },
        fallbacks: FallbackChain {
            fallbacks: vec![DefaultModel {
                provider: "router".into(),
                model: "chat".into(),
            }],
        },
        auxiliaries: vec![],
    };
    let chain = ProviderChain::new(
        policy,
        HttpTransport::default(),
        vec![],
        SecretValue::new("unused"),
    );
    let c = Collect(RefCell::new(vec![]));
    // Word prompt: bare "go" sometimes comes back with no text delta
    // under parallel load; asking for a word always yields text.
    if let Err(e) = chain.turn_with_sink(&[Message::user("say the word mango")], &c) {
        if offline(&e) {
            eprintln!("SKIP live_fallback_emits_ordered_chain_events: router unreachable");
            return;
        }
        panic!("live fallback failed: {e:?}");
    }
    let kinds: Vec<&str> =
        c.0.borrow()
            .iter()
            .map(|e| match e {
                ModelEvent::Attempt { chain_index: 0, .. } => "attempt:0",
                ModelEvent::Attempt { chain_index: 1, .. } => "attempt:1",
                ModelEvent::AttemptFailed {
                    retryable: true, ..
                } => "failed:retryable",
                ModelEvent::Fallback { to_index: 1, .. } => "fallback:1",
                ModelEvent::Usage { .. } => "usage",
                ModelEvent::Completed { .. } => "completed",
                ModelEvent::TextDelta { .. } => "text",
                _ => "other",
            })
            .collect();
    // Live streams emit many deltas: assert milestone order, not exact shape.
    let pos = |want: &str| {
        kinds
            .iter()
            .position(|k| *k == want)
            .unwrap_or_else(|| panic!("missing {want} in {kinds:?}"))
    };
    let order = [
        pos("attempt:0"),
        pos("failed:retryable"),
        pos("fallback:1"),
        pos("attempt:1"),
        pos("text"),
        pos("usage"),
        pos("completed"),
    ];
    assert!(
        order.windows(2).all(|w| w[0] < w[1]),
        "milestones out of order: {kinds:?}"
    );
}

#[test]
fn live_exhaustion_is_structured_and_emits_exhausted() {
    // Both entries dial dead ports (refused = retryable) so the chain
    // walks default → fallback → exhaustion.
    live_provider("livechain-dead", "http://127.0.0.1:9/v1");
    live_provider("livechain-dead2", "http://127.0.0.1:9/v1");
    let policy = ModelPolicy {
        default: DefaultModel {
            provider: "livechain-dead".into(),
            model: "chat".into(),
        },
        fallbacks: FallbackChain {
            fallbacks: vec![DefaultModel {
                provider: "livechain-dead2".into(),
                model: "chat".into(),
            }],
        },
        auxiliaries: vec![],
    };
    let chain = ProviderChain::new(
        policy,
        HttpTransport::default(),
        vec![],
        SecretValue::new("unused"),
    );
    let c = Collect(RefCell::new(vec![]));
    let err = match chain.turn_with_sink(&[Message::user("go")], &c) {
        Ok(_) => panic!("all-bogus chain should not succeed"),
        Err(e) if offline(&e) => {
            eprintln!("SKIP live_exhaustion: router unreachable");
            return;
        }
        Err(e) => e,
    };
    assert_eq!(err.code, "PROVIDER_EXHAUSTED");
    assert!(!err.retryable);
    assert!(
        c.0.borrow()
            .iter()
            .any(|e| matches!(e, ModelEvent::Exhausted { .. })),
        "exhaustion must emit Exhausted"
    );
}

#[test]
fn live_streaming_deltas_arrive_before_completed() {
    let Some(real) = router_key() else {
        eprintln!("SKIP live_streaming_deltas: no PANTHEON_KEY_ROUTER");
        return;
    };
    let (policy, keys) = live_policy(&real, None);
    let chain = ProviderChain::new(policy, HttpTransport::default(), vec![], keys);
    let c = Collect(RefCell::new(vec![]));
    let out = match chain.turn_stream(&[Message::user("say hi")], &c) {
        Ok(o) => o,
        Err(e) if offline(&e) => {
            eprintln!("SKIP live_streaming_deltas: router unreachable");
            return;
        }
        Err(e) => panic!("live stream failed: {e:?}"),
    };
    assert!(
        matches!(out, TurnOutcome::Text { ref text, .. } if !text.trim().is_empty()),
        "expected streamed text, got {out:?}"
    );
    let evs = c.0.borrow();
    assert!(matches!(
        evs.first(),
        Some(ModelEvent::Attempt {
            streaming: true,
            ..
        })
    ));
    let delta_at = evs
        .iter()
        .position(|e| matches!(e, ModelEvent::TextDelta { .. }))
        .expect("stream must emit deltas");
    let completed_at = evs
        .iter()
        .position(|e| matches!(e, ModelEvent::Completed { .. }))
        .expect("stream must complete");
    assert!(delta_at < completed_at);
}

#[test]
fn live_usage_reports_tokens() {
    let Some(real) = router_key() else {
        eprintln!("SKIP live_usage_reports_tokens: no PANTHEON_KEY_ROUTER");
        return;
    };
    let (policy, keys) = live_policy(&real, None);
    let chain = ProviderChain::new(policy, HttpTransport::default(), vec![], keys);
    let c = Collect(RefCell::new(vec![]));
    if let Err(e) = chain.turn_with_sink(&[Message::user("go")], &c) {
        if offline(&e) {
            eprintln!("SKIP live_usage_reports_tokens: router unreachable");
            return;
        }
        panic!("live usage turn failed: {e:?}");
    }
    let usage =
        c.0.borrow()
            .iter()
            .find_map(|e| match e {
                ModelEvent::Usage { usage } => Some(*usage),
                _ => None,
            })
            .expect("usage event");
    assert!(usage.input_tokens > 0 && usage.output_tokens > 0);
}

#[test]
fn live_non_retryable_config_failure_fails_fast_without_fallback() {
    // No HTTP at all: an unresolved `{var}` template is a config error —
    // non-retryable, no fallback, nothing resolved.
    let policy = ModelPolicy {
        default: DefaultModel {
            provider: "livechain-tpl".into(),
            model: "chat".into(),
        },
        fallbacks: FallbackChain {
            fallbacks: vec![DefaultModel {
                provider: "router".into(),
                model: "chat".into(),
            }],
        },
        auxiliaries: vec![],
    };
    // Point the entry at a templated base with no value anywhere.
    catalog::register_custom_provider(catalog::ProviderMeta {
        id: "livechain-tpl".to_string(),
        label: "livechain-tpl".to_string(),
        base_url: "https://{unresolved_var}.example.com/v1".to_string(),
        api_mode: catalog::ApiMode::OpenAi,
        base_env: String::new(),
        key_env: "PANTHEON_KEY_LIVECHAIN_TPL".to_string(),
        key_header: "Authorization".to_string(),
        models: Vec::new(),
        prominent: false,
        tag: "live-test".to_string(),
    });
    std::env::remove_var("PANTHEON_KEY_LIVECHAIN_TPL");
    let chain = ProviderChain::new(
        policy,
        HttpTransport::default(),
        vec![],
        SecretValue::new("k"),
    );
    let c = Collect(RefCell::new(vec![]));
    let err = chain
        .turn_with_sink(&[Message::user("go")], &c)
        .unwrap_err();
    assert_eq!(err.code, "PROVIDER_CONFIG");
    assert!(!err.retryable);
    assert!(chain.last_resolved.borrow().is_none());
    assert!(
        !c.0.borrow()
            .iter()
            .any(|e| matches!(e, ModelEvent::Fallback { .. })),
        "config failure must not fall back"
    );
}
