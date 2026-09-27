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
use crate::catalog;
use crate::http::HttpTransport;
use crate::http::WireRequest;
use crate::model_event::{ModelEvent, ModelEventSink};
use pantheon_api::model::{DefaultModel, FallbackChain, ModelPolicy};
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
        dev: false,
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
        reasoning_budget: Default::default(),
        reasoning: Default::default(),
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
        reasoning_budget: Default::default(),
        reasoning: Default::default(),
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
        reasoning: Default::default(),
        reasoning_budget: Default::default(),
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
        reasoning: Default::default(),
        reasoning_budget: Default::default(),
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
        reasoning: Default::default(),
        reasoning_budget: Default::default(),
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
        dev: false,
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

// --- Cost ceiling regression (no live router needed) ---

/// Fake transport: every POST returns one canned OpenAI chat-completion
/// response with fixed usage (100k prompt / 50k completion tokens).
struct StubTransport {
    body: String,
}
impl ChatTransport for StubTransport {
    fn post(&self, _req: &WireRequest) -> Result<String, PantheonError> {
        Ok(self.body.clone())
    }
    fn post_stream(
        &self,
        _req: &WireRequest,
        _on_payload: &mut dyn FnMut(&str) -> Result<(), PantheonError>,
    ) -> Result<(), PantheonError> {
        unimplemented!("single-shot test")
    }
}

#[test]
fn outcome_carries_catalog_cost_so_cost_ceiling_trips() {
    // Regression: the adapter bakes `cost_cents` at parse time from
    // `cost_usd: None` (always 0), so the agent loop's `max_cost_cents`
    // budget accumulated zeros and could never trip. The chain now
    // stamps the catalog estimate onto the outcome before returning it.
    // Uses a cataloged priced model (openai/gpt-4o-mini) so the estimate
    // is real; the stub transport means no network and no API key.
    let policy = ModelPolicy {
        reasoning_budget: Default::default(),
        reasoning: Default::default(),
        default: DefaultModel {
            provider: "openai".into(),
            model: "gpt-4o-mini".into(),
        },
        fallbacks: FallbackChain { fallbacks: vec![] },
        auxiliaries: vec![],
    };
    let body = serde_json::json!({
        "id": "chatcmpl-cost",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hello"},
            "finish_reason": "stop",
        }],
        "usage": {
            "prompt_tokens": 100_000,
            "completion_tokens": 50_000,
            "total_tokens": 150_000,
        },
    })
    .to_string();
    let chain = ProviderChain::new(policy, StubTransport { body }, vec![], SecretValue::new("k"));
    let out = chain
        .turn_messages(&[Message::user("hi")])
        .expect("stubbed turn should succeed");
    // Expected cost from the catalog itself, so price edits don't break
    // the test: (0.015 + 0.03) USD -> cents, same formula the chain uses.
    let expected = (catalog::model_meta("openai", "gpt-4o-mini")
        .cost
        .estimate(100_000, 50_000)
        .expect("catalog must price gpt-4o-mini")
        * 100.0) as u32;
    assert!(expected > 0, "test needs a priced model");
    assert_eq!(
        out.cost_cents(),
        expected,
        "outcome must carry the catalog estimate, got {out:?}"
    );
    // The accumulation the agent loop performs (`total_cost_cents +=
    // outcome.cost_cents()`, then `total_cost_cents >= max_cost_cents`):
    // a cap equal to this turn's cost trips on the single turn now.
    let mut total_cost_cents = 0u32;
    total_cost_cents += out.cost_cents();
    let cap = expected;
    assert!(
        total_cost_cents >= cap,
        "budget check (total {total_cost_cents} >= cap {cap}) must trip"
    );
}

// --- Retry-After backoff (no live router needed) ---

/// Fake transport: every POST fails with a 429 carrying the exact cause
/// shape `send()` stamps for a real 429 (`(retry-after: Ns)` marker).
struct RateLimitStub {
    calls: std::sync::atomic::AtomicUsize,
    retry_after_secs: u64,
}
impl ChatTransport for RateLimitStub {
    fn post(&self, _req: &WireRequest) -> Result<String, PantheonError> {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(PantheonError::new(
            "PROVIDER_HTTP",
            pantheon_api::error::Layer::Provider,
            true,
            format!(
                "http://stub/v1: HTTP 429 slow down (retry-after: {}s)",
                self.retry_after_secs
            ),
            "wait and retry",
            "",
        ))
    }
    fn post_stream(
        &self,
        _req: &WireRequest,
        _on_payload: &mut dyn FnMut(&str) -> Result<(), PantheonError>,
    ) -> Result<(), PantheonError> {
        unimplemented!("single-shot test")
    }
}

fn rate_provider(id: &str) {
    catalog::register_custom_provider(catalog::ProviderMeta {
        id: id.to_string(),
        label: id.to_string(),
        base_url: "http://127.0.0.1:9/v1".to_string(),
        api_mode: catalog::ApiMode::OpenAi,
        base_env: String::new(),
        key_env: format!("PANTHEON_KEY_{}", id.to_ascii_uppercase()),
        key_header: "Authorization".to_string(),
        models: Vec::new(),
        prominent: false,
        dev: false,
        tag: "rate-test".to_string(),
    });
}

#[test]
fn rate_limit_backoff_paces_key_rotation_and_fallback() {
    // Regression: a 429 used to rotate stacked keys and fall back
    // immediately, hammering a rate-limited provider. The transport now
    // stamps the parsed Retry-After on the error and the chain sleeps
    // (capped) before each next step — key rotation and fallback alike.
    //
    // `retry-after: 1s` keeps the test fast while proving the sleep is
    // actually taken: 2 waits (rotation, pre-fallback) × 1s.
    rate_provider("ratelimit0");
    rate_provider("ratelimit1");
    let policy = ModelPolicy {
        reasoning_budget: Default::default(),
        reasoning: Default::default(),
        default: DefaultModel {
            provider: "ratelimit0".into(),
            model: "chat".into(),
        },
        fallbacks: FallbackChain {
            fallbacks: vec![DefaultModel {
                provider: "ratelimit1".into(),
                model: "chat".into(),
            }],
        },
        auxiliaries: vec![],
    };
    let stub = RateLimitStub {
        calls: std::sync::atomic::AtomicUsize::new(0),
        retry_after_secs: 1,
    };
    let chain = ProviderChain {
        policy,
        transport: stub,
        tools: vec![],
        api_key: Some(SecretValue::new("k1,k2")),
        last_resolved: RefCell::new(None),
    };
    let c = Collect(RefCell::new(vec![]));
    let started = std::time::Instant::now();
    let err = chain
        .turn_with_sink(&[Message::user("go")], &c)
        .unwrap_err();
    let elapsed = started.elapsed();
    assert_eq!(err.code, "PROVIDER_EXHAUSTED");
    // Both stacked keys tried on the default entry, then the fallback.
    assert_eq!(
        chain
            .transport
            .calls
            .load(std::sync::atomic::Ordering::SeqCst),
        3,
        "k1, k2, then the fallback entry"
    );
    // Rotation really happened (second key attempted after the first 429).
    assert!(
        c.0.borrow().iter().any(|e| matches!(
            e,
            ModelEvent::AttemptFailed { code, .. } if code.contains(":key1")
        )),
        "rotation must emit AttemptFailed with the :key1 marker"
    );
    assert!(
        c.0.borrow()
            .iter()
            .any(|e| matches!(e, ModelEvent::Fallback { to_index: 1, .. })),
        "a retryable 429 must stay fallback-eligible"
    );
    // The waits were taken: 2 × 1s (rotation + pre-fallback). No sleep
    // before exhaustion — the third 429 ends the chain immediately.
    assert!(
        elapsed >= std::time::Duration::from_secs(2),
        "backoff sleeps were skipped: elapsed {elapsed:?}"
    );
}

#[test]
fn rate_limit_without_header_does_not_sleep() {
    // A 429 with no Retry-After header carries no marker: rotation and
    // fallback proceed immediately, exactly as before.
    rate_provider("ratelimit2");
    let policy = ModelPolicy {
        reasoning_budget: Default::default(),
        reasoning: Default::default(),
        default: DefaultModel {
            provider: "ratelimit2".into(),
            model: "chat".into(),
        },
        fallbacks: FallbackChain { fallbacks: vec![] },
        auxiliaries: vec![],
    };
    struct BareStub;
    impl ChatTransport for BareStub {
        fn post(&self, _req: &WireRequest) -> Result<String, PantheonError> {
            Err(PantheonError::new(
                "PROVIDER_HTTP",
                pantheon_api::error::Layer::Provider,
                true,
                "http://stub/v1: HTTP 429 slow down".to_string(),
                "wait and retry",
                "",
            ))
        }
        fn post_stream(
            &self,
            _req: &WireRequest,
            _on_payload: &mut dyn FnMut(&str) -> Result<(), PantheonError>,
        ) -> Result<(), PantheonError> {
            unimplemented!("single-shot test")
        }
    }
    let chain = ProviderChain::new(policy, BareStub, vec![], SecretValue::new("k"));
    let started = std::time::Instant::now();
    let err = chain.turn_messages(&[Message::user("go")]).unwrap_err();
    assert_eq!(err.code, "PROVIDER_EXHAUSTED");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "no Retry-After header must mean no sleep"
    );
}
