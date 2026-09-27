//! Behavioral test for the provider chain: a 429 with Retry-After must
//! pace key rotation and fallback with real (capped) sleeps (~2s). Moved
//! here from `pantheon-providers/src/chain_tests.rs`; runs under
//! `cargo test -p pantheon-eval`, not beside the code.
use pantheon_api::error::PantheonError;
use pantheon_api::message::Message;
use pantheon_api::model::{DefaultModel, FallbackChain, ModelPolicy};
use pantheon_providers::catalog;
use pantheon_providers::chain::ProviderChain;
use pantheon_providers::http::{ChatTransport, WireRequest};
use pantheon_providers::model_event::{ModelEvent, ModelEventSink};
use pantheon_secrets::SecretValue;
use std::cell::RefCell;

struct Collect(RefCell<Vec<ModelEvent>>);
impl ModelEventSink for Collect {
    fn emit(&self, event: ModelEvent) {
        self.0.borrow_mut().push(event);
    }
}

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
