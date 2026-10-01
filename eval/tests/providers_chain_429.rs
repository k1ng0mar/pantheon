//! Behavioral test for the provider chain: a 429 with Retry-After must
//! pace key rotation and the retry backoff with the provider's asked-for
//! wait as a floor. Runs under `cargo test -p pantheon-eval`, not beside
//! the code. Fake clock: no real sleeping.
use pantheon_api::error::PantheonError;
use pantheon_api::message::Message;
use pantheon_api::model::{DefaultModel, FallbackChain, ModelPolicy};
use pantheon_providers::catalog;
use pantheon_providers::chain::{ProviderChain, RetryConfig, Sleeper};
use pantheon_providers::http::{ChatTransport, WireRequest};
use pantheon_providers::model_event::{ModelEvent, ModelEventSink};
use pantheon_secrets::SecretValue;
use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Collect(RefCell<Vec<ModelEvent>>);
impl ModelEventSink for Collect {
    fn emit(&self, event: ModelEvent) {
        self.0.borrow_mut().push(event);
    }
}

/// Fake clock: records waits, never sleeps; fixed jitter for determinism.
struct FakeClock {
    waits: Mutex<Vec<Duration>>,
    jitter: f64,
}
impl Sleeper for FakeClock {
    fn sleep(&self, dur: Duration) {
        self.waits.lock().unwrap().push(dur);
    }
    fn jitter(&self) -> f64 {
        self.jitter
    }
}

/// Fake transport: every POST fails with a 429 carrying the exact cause
/// shape `send()` stamps for a real 429 (`(retry-after: Ns)` marker).
struct RateLimitStub {
    calls: AtomicUsize,
    retry_after_secs: u64,
}
impl ChatTransport for RateLimitStub {
    fn post(&self, _req: &WireRequest) -> Result<String, PantheonError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
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
        recommended: false,
        tag: "rate-test".to_string(),
    });
}

#[test]
fn rate_limit_retry_after_floors_backoff_and_paces_rotation() {
    // A 429's Retry-After is a wait floor everywhere: stacked-key
    // rotation honors it before the next key, and each of the 3 retries
    // waits at least what the provider asked for. Fake clock, so the
    // waits are recorded, not slept.
    rate_provider("ratelimit0");
    let policy = ModelPolicy {
        reasoning_budget: Default::default(),
        reasoning: Default::default(),
        default: DefaultModel {
            provider: "ratelimit0".into(),
            model: "chat".into(),
        },
        fallbacks: FallbackChain { fallbacks: vec![] },
        auxiliaries: vec![],
    };
    let clock = Arc::new(FakeClock {
        waits: Mutex::new(Vec::new()),
        jitter: 1.0,
    });
    let stub = RateLimitStub {
        calls: AtomicUsize::new(0),
        retry_after_secs: 1,
    };
    let chain = ProviderChain {
        policy,
        transport: stub,
        tools: vec![],
        api_key: Some(SecretValue::new("k1,k2")),
        retry: RetryConfig::default(),
        sleeper: clock.clone(),
        last_resolved: RefCell::new(None),
        session_max_tokens: None,
        budget_max_tokens: None,
    };
    let c = Collect(RefCell::new(vec![]));
    let started = std::time::Instant::now();
    let err = chain
        .turn_with_sink(&[Message::user("go")], &c)
        .unwrap_err();
    let elapsed = started.elapsed();
    assert_eq!(err.code, "PROVIDER_EXHAUSTED");
    // 4 attempts (initial + 3 retries) x 2 stacked keys.
    assert_eq!(
        chain.transport.calls.load(Ordering::SeqCst),
        8,
        "each attempt tries k1, rotates, then k2"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "fake clock must not really sleep: elapsed {elapsed:?}"
    );
    // Rotation really happened (second key attempted after the first 429).
    assert!(
        c.0.borrow().iter().any(|e| matches!(
            e,
            ModelEvent::AttemptFailed { code, .. } if code.contains(":key1")
        )),
        "rotation must emit AttemptFailed with the :key1 marker"
    );
    // The 3 retries were announced with the Retry-After floor applied:
    // max(exp backoff, 1s) at jitter 1.0 -> 1s, 1s, 2s.
    let retry_waits: Vec<u64> =
        c.0.borrow()
            .iter()
            .filter_map(|e| match e {
                ModelEvent::RetryAttempt { wait_secs, .. } => Some(*wait_secs),
                _ => None,
            })
            .collect();
    assert_eq!(
        retry_waits,
        vec![1, 1, 2],
        "retry waits must floor at the 1s Retry-After"
    );
    // Every recorded wait — rotation and backoff alike — honors the
    // provider's asked-for second.
    let waits = clock.waits.lock().unwrap().clone();
    assert_eq!(waits.len(), 7, "4 rotation waits + 3 retry waits");
    assert!(
        waits.iter().all(|w| *w >= Duration::from_secs(1)),
        "all waits must honor Retry-After: {waits:?}"
    );
    assert!(
        c.0.borrow()
            .iter()
            .any(|e| matches!(e, ModelEvent::Exhausted { .. })),
        "a retryable 429 with no fallback left must exhaust"
    );
}
