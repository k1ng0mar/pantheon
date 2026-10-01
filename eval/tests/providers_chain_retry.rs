//! Behavioral tests for the provider chain retry policy: 3 retries
//! before fallback, each fallback gets its own 3, non-retryable skips
//! retries, exhaustion fails loudly. Fake clock everywhere — no real
//! sleeping. Runs under `cargo test -p pantheon-eval`.
use pantheon_api::error::PantheonError;
use pantheon_api::message::Message;
use pantheon_api::model::{DefaultModel, FallbackChain, ModelPolicy};
use pantheon_providers::catalog;
use pantheon_providers::chain::{ProviderChain, RetryConfig, Sleeper};
use pantheon_providers::http::{ChatTransport, WireRequest};
use pantheon_providers::model_event::{ModelEvent, ModelEventSink};
use pantheon_secrets::SecretValue;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Collect(RefCell<Vec<ModelEvent>>);
impl ModelEventSink for Collect {
    fn emit(&self, event: ModelEvent) {
        self.0.borrow_mut().push(event);
    }
}

/// Fake clock: records waits, never sleeps; fixed jitter 1.0 keeps the
/// exponential waits deterministic.
#[derive(Default)]
struct FakeClock {
    waits: Mutex<Vec<Duration>>,
}
impl Sleeper for FakeClock {
    fn sleep(&self, dur: Duration) {
        self.waits.lock().unwrap().push(dur);
    }
    fn jitter(&self) -> f64 {
        1.0
    }
}

/// Scripted stub: pops a queued outcome per call. `Err` carries a code
/// and a `retryable` flag stamped into the PantheonError; `Ok` succeeds.
enum Outcome {
    Fail { code: &'static str, retryable: bool },
    Ok,
}
struct ScriptStub {
    calls: AtomicUsize,
    outcomes: Mutex<VecDeque<Outcome>>,
}
impl ChatTransport for ScriptStub {
    fn post(&self, _req: &WireRequest) -> Result<String, PantheonError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.outcomes.lock().unwrap().pop_front() {
            // Minimal OpenAI chat-completions body the response parser
            // accepts.
            Some(Outcome::Ok) => Ok(
                r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#.into(),
            ),
            Some(Outcome::Fail { code, retryable }) => Err(PantheonError::new(
                code,
                pantheon_api::error::Layer::Provider,
                retryable,
                format!("{code} exploded"),
                "see the event detail",
                "",
            )),
            None => panic!("stub script exhausted"),
        }
    }
    fn post_stream(
        &self,
        _req: &WireRequest,
        _on_payload: &mut dyn FnMut(&str) -> Result<(), PantheonError>,
    ) -> Result<(), PantheonError> {
        unimplemented!("single-shot tests")
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
        tag: "retry-test".to_string(),
    });
}

fn policy(first: &str, rest: &[&str]) -> ModelPolicy {
    ModelPolicy {
        reasoning_budget: Default::default(),
        reasoning: Default::default(),
        default: DefaultModel {
            provider: first.into(),
            model: "chat".into(),
        },
        fallbacks: FallbackChain {
            fallbacks: rest
                .iter()
                .map(|p| DefaultModel {
                    provider: p.to_string(),
                    model: "chat".to_string(),
                })
                .collect(),
        },
        auxiliaries: vec![],
    }
}

fn retryable(code: &'static str) -> Outcome {
    Outcome::Fail {
        code,
        retryable: true,
    }
}

fn chain(policy: ModelPolicy, stub: ScriptStub) -> (ProviderChain<ScriptStub>, Arc<FakeClock>) {
    let clock = Arc::new(FakeClock::default());
    let chain = ProviderChain {
        policy,
        transport: stub,
        tools: vec![],
        api_key: Some(SecretValue::new("k1")),
        retry: RetryConfig::default(),
        sleeper: clock.clone(),
        last_resolved: RefCell::new(None),
        session_max_tokens: None,
        budget_max_tokens: None,
    };
    (chain, clock)
}

fn retry_events(events: &Collect) -> Vec<(u32, u32)> {
    events
        .0
        .borrow()
        .iter()
        .filter_map(|e| match e {
            ModelEvent::RetryAttempt {
                attempt,
                max_attempts,
                ..
            } => Some((*attempt, *max_attempts)),
            _ => None,
        })
        .collect()
}

#[test]
fn three_retries_then_fallback() {
    // The default model fails retryably 4 times, then the fallback
    // succeeds. The chain must burn exactly 3 retries on the first
    // model before engaging the fallback.
    rate_provider("retrya");
    rate_provider("retryb");
    let stub = ScriptStub {
        calls: AtomicUsize::new(0),
        outcomes: Mutex::new(VecDeque::from([
            retryable("PROVIDER_HTTP"),
            retryable("PROVIDER_HTTP"),
            retryable("PROVIDER_HTTP"),
            retryable("PROVIDER_HTTP"),
            Outcome::Ok,
        ])),
    };
    let (chain, clock) = chain(policy("retrya", &["retryb"]), stub);
    let c = Collect(RefCell::new(vec![]));
    let started = std::time::Instant::now();
    chain
        .turn_with_sink(&[Message::user("go")], &c)
        .expect("fallback should succeed");
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(chain.transport.calls.load(Ordering::SeqCst), 5);
    assert_eq!(retry_events(&c), vec![(1, 3), (2, 3), (3, 3)]);
    // Exponential backoff at jitter 1.0: 500ms, 1s, 2s.
    assert_eq!(
        clock.waits.lock().unwrap().clone(),
        vec![
            Duration::from_millis(500),
            Duration::from_secs(1),
            Duration::from_secs(2)
        ]
    );
    assert!(
        c.0.borrow().iter().any(|e| matches!(
            e,
            ModelEvent::Fallback { to_provider, .. } if to_provider == "retryb"
        )),
        "fallback must engage after the retries are spent"
    );
    assert!(
        c.0.borrow()
            .iter()
            .any(|e| matches!(e, ModelEvent::Completed { .. } | ModelEvent::Usage { .. })),
        "the fallback's success ends the turn"
    );
}

#[test]
fn each_fallback_gets_its_own_retries() {
    // Every chain entry — default AND each fallback — gets its own 3
    // retries: 4 calls per entry, 12 total across the three models.
    rate_provider("retry1");
    rate_provider("retry2");
    rate_provider("retry3");
    let mut script = VecDeque::new();
    for _ in 0..12 {
        script.push_back(retryable("PROVIDER_TIMEOUT"));
    }
    let stub = ScriptStub {
        calls: AtomicUsize::new(0),
        outcomes: Mutex::new(script),
    };
    let (chain, _) = chain(policy("retry1", &["retry2", "retry3"]), stub);
    let c = Collect(RefCell::new(vec![]));
    let err = chain
        .turn_with_sink(&[Message::user("go")], &c)
        .unwrap_err();
    assert_eq!(err.code, "PROVIDER_EXHAUSTED");
    assert_eq!(chain.transport.calls.load(Ordering::SeqCst), 12);
    assert_eq!(retry_events(&c).len(), 9, "3 retries per model x 3 models");
    assert_eq!(
        retry_events(&c),
        vec![
            (1, 3),
            (2, 3),
            (3, 3),
            (1, 3),
            (2, 3),
            (3, 3),
            (1, 3),
            (2, 3),
            (3, 3)
        ],
        "each model's retries must count 1..3 independently"
    );
    let fallbacks: Vec<String> =
        c.0.borrow()
            .iter()
            .filter_map(|e| match e {
                ModelEvent::Fallback { to_provider, .. } => Some(to_provider.clone()),
                _ => None,
            })
            .collect();
    assert_eq!(fallbacks, vec!["retry2".to_string(), "retry3".to_string()]);
}

#[test]
fn non_retryable_failure_skips_retries() {
    // A 401 is not retryable: no RetryAttempt, no waits — straight to
    // the fallback. The fallback succeeds.
    rate_provider("retry401a");
    rate_provider("retry401b");
    let stub = ScriptStub {
        calls: AtomicUsize::new(0),
        outcomes: Mutex::new(VecDeque::from([
            Outcome::Fail {
                code: "PROVIDER_AUTH",
                retryable: false,
            },
            Outcome::Ok,
        ])),
    };
    let (chain, clock) = chain(policy("retry401a", &["retry401b"]), stub);
    let c = Collect(RefCell::new(vec![]));
    chain
        .turn_with_sink(&[Message::user("go")], &c)
        .expect("fallback should succeed");
    assert_eq!(chain.transport.calls.load(Ordering::SeqCst), 2);
    assert!(
        retry_events(&c).is_empty(),
        "non-retryable failures must never emit RetryAttempt"
    );
    assert!(
        clock.waits.lock().unwrap().is_empty(),
        "no retries means no waits"
    );
    assert!(
        c.0.borrow().iter().any(|e| matches!(
            e,
            ModelEvent::Fallback { to_provider, .. } if to_provider == "retry401b"
        )),
        "non-retryable still advances to the fallback"
    );
}

#[test]
fn chain_exhaustion_fails_loudly_with_last_code() {
    // Nothing left to try: the turn fails with the exhaustion code and
    // the failure of the last model is named in the event/detail.
    rate_provider("retrylast1");
    rate_provider("retrylast2");
    let mut script = VecDeque::new();
    for _ in 0..8 {
        script.push_back(retryable("PROVIDER_TIMEOUT"));
    }
    let stub = ScriptStub {
        calls: AtomicUsize::new(0),
        outcomes: Mutex::new(script),
    };
    let (chain, _) = chain(policy("retrylast1", &["retrylast2"]), stub);
    let c = Collect(RefCell::new(vec![]));
    let err = chain
        .turn_with_sink(&[Message::user("go")], &c)
        .unwrap_err();
    assert_eq!(err.code, "PROVIDER_EXHAUSTED");
    assert_eq!(chain.transport.calls.load(Ordering::SeqCst), 8);
    assert!(
        err.cause.contains("PROVIDER_TIMEOUT"),
        "exhaustion names the last model's failure code: {}",
        err.cause
    );
    assert!(
        c.0.borrow()
            .iter()
            .any(|e| matches!(e, ModelEvent::Exhausted { .. })),
        "the chain must emit Exhausted"
    );
    assert!(
        !c.0.borrow()
            .iter()
            .any(|e| matches!(e, ModelEvent::Completed { .. } | ModelEvent::Usage { .. })),
        "exhaustion must not be reported as success"
    );
}
