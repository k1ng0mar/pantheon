//! Behavioral tests for the provider chain retry policy: 3 retries
//! before fallback, each fallback gets its own 3, non-retryable skips
//! retries, exhaustion fails loudly. Fake clock everywhere - no real
//! sleeping. Runs under `cargo test -p pantheon-eval`.
use pantheon_api::error::PantheonError;
use pantheon_api::message::Message;
use pantheon_api::model::{DefaultModel, FallbackChain, ModelPolicy};
use pantheon_providers::catalog;
use pantheon_providers::chain::{ProviderChain, RetryConfig, Sleeper};
use pantheon_providers::http::{ChatTransport, StreamEnd, WireRequest};
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

/// The streaming path returns provider payloads one at a time, like the
/// production SSE transport. Keeping the request body lets the test verify
/// that the chain actually asks the provider to stream.
struct StreamStub {
    request_bodies: Mutex<Vec<String>>,
    payloads: Vec<&'static str>,
    fail_after_payloads: bool,
}
impl ChatTransport for StreamStub {
    fn post(&self, _req: &WireRequest) -> Result<String, PantheonError> {
        panic!("streaming turns must use post_stream")
    }

    fn post_stream(
        &self,
        req: &WireRequest,
        on_payload: &mut dyn FnMut(&str) -> Result<(), PantheonError>,
    ) -> Result<StreamEnd, PantheonError> {
        self.request_bodies.lock().unwrap().push(req.body.clone());
        for payload in &self.payloads {
            on_payload(payload)?;
        }
        if self.fail_after_payloads {
            Err(PantheonError::new(
                "PROVIDER_READ",
                pantheon_api::error::Layer::Provider,
                true,
                "connection reset after partial text",
                "retry the request",
                "",
            ))
        } else {
            Ok(StreamEnd::Done)
        }
    }
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
    ) -> Result<pantheon_providers::http::StreamEnd, PantheonError> {
        unimplemented!("single-shot tests")
    }
}

fn rate_provider(id: &str) {
    provider_with_mode(id, catalog::ApiMode::OpenAi);
}

fn provider_with_mode(id: &str, api_mode: catalog::ApiMode) {
    catalog::register_custom_provider(catalog::ProviderMeta {
        id: id.to_string(),
        label: id.to_string(),
        base_url: "http://127.0.0.1:9/v1".to_string(),
        api_mode,
        base_env: String::new(),
        key_env: format!("PANTHEON_KEY_{}", id.to_ascii_uppercase()),
        key_header: "Authorization".to_string(),
        models: Vec::new(),
        prominent: false,
        dev: false,
        recommended: false,
        tag: "retry-test".to_string(),
        auth_command: None,
        auth_refresh_ms: None,
        reserved: false,
        allowed_params: vec![],
        oauth_apps: vec![],
        sigv4: false,
        reasoning_map: Default::default(),
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
        oauth_broker: None,
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
fn streaming_turn_sends_stream_request_and_emits_incremental_text() {
    rate_provider("retry-stream");
    let clock = Arc::new(FakeClock::default());
    let transport = StreamStub {
        request_bodies: Mutex::new(Vec::new()),
        fail_after_payloads: false,
        payloads: vec![
            r#"{"choices":[{"delta":{"content":"hello "},"finish_reason":null}]}"#,
            r#"{"choices":[{"delta":{"content":"world"},"finish_reason":null}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ],
    };
    let chain = ProviderChain {
        policy: policy("retry-stream", &[]),
        transport,
        tools: vec![],
        api_key: Some(SecretValue::new("k1")),
        retry: RetryConfig::default(),
        sleeper: clock,
        last_resolved: RefCell::new(None),
        session_max_tokens: None,
        budget_max_tokens: None,
        oauth_broker: None,
    };
    let events = Collect(RefCell::new(Vec::new()));

    let outcome = chain
        .turn_stream(&[Message::user("go")], &events)
        .expect("streamed turn");

    assert!(matches!(
        outcome,
        pantheon_agent::TurnOutcome::Text { ref text, .. } if text == "hello world"
    ));
    let bodies = chain.transport.request_bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&bodies[0]).unwrap()["stream"],
        true
    );
    let emitted: Vec<_> = events
        .0
        .borrow()
        .iter()
        .filter_map(|event| match event {
            ModelEvent::TextDelta { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(emitted, ["hello ", "world"]);
    assert!(events.0.borrow().iter().any(|event| matches!(
        event,
        ModelEvent::Attempt {
            streaming: true,
            ..
        }
    )));
}

#[test]
fn anthropic_streaming_turn_emits_incremental_text() {
    provider_with_mode("retry-stream-anthropic", catalog::ApiMode::Anthropic);
    let clock = Arc::new(FakeClock::default());
    let transport = StreamStub {
        request_bodies: Mutex::new(Vec::new()),
        fail_after_payloads: false,
        payloads: vec![
            r#"{"type":"message_start","message":{"usage":{"input_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello "}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"world"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}"#,
            r#"{"type":"message_stop"}"#,
        ],
    };
    let chain = ProviderChain {
        policy: policy("retry-stream-anthropic", &[]),
        transport,
        tools: vec![],
        api_key: Some(SecretValue::new("k1")),
        retry: RetryConfig::default(),
        sleeper: clock,
        last_resolved: RefCell::new(None),
        session_max_tokens: None,
        budget_max_tokens: None,
        oauth_broker: None,
    };
    let events = Collect(RefCell::new(Vec::new()));

    let outcome = chain
        .turn_stream(&[Message::user("go")], &events)
        .expect("streamed turn");

    assert!(matches!(
        outcome,
        pantheon_agent::TurnOutcome::Text { ref text, .. } if text == "hello world"
    ));
    let bodies = chain.transport.request_bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&bodies[0]).unwrap()["stream"],
        true
    );
    let emitted: Vec<_> = events
        .0
        .borrow()
        .iter()
        .filter_map(|event| match event {
            ModelEvent::TextDelta { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(emitted, ["hello ", "world"]);
    assert!(events.0.borrow().iter().any(|event| matches!(
        event,
        ModelEvent::Attempt {
            streaming: true,
            ..
        }
    )));
}

#[test]
fn failed_stream_after_visible_text_is_not_replayed_or_fallen_back() {
    rate_provider("retry-stream-partial");
    rate_provider("retry-stream-fallback");
    let clock = Arc::new(FakeClock::default());
    let transport = StreamStub {
        request_bodies: Mutex::new(Vec::new()),
        fail_after_payloads: true,
        payloads: vec![r#"{"choices":[{"delta":{"content":"partial"},"finish_reason":null}]}"#],
    };
    let chain = ProviderChain {
        policy: policy("retry-stream-partial", &["retry-stream-fallback"]),
        transport,
        tools: vec![],
        api_key: Some(SecretValue::new("k1")),
        retry: RetryConfig::default(),
        sleeper: clock.clone(),
        last_resolved: RefCell::new(None),
        session_max_tokens: None,
        budget_max_tokens: None,
        oauth_broker: None,
    };
    let events = Collect(RefCell::new(Vec::new()));

    let err = chain
        .turn_stream(&[Message::user("go")], &events)
        .expect_err("a failed stream cannot produce a successful answer");

    assert_eq!(err.code, "PROVIDER_STREAM_INTERRUPTED");
    assert_eq!(chain.transport.request_bodies.lock().unwrap().len(), 1);
    assert!(clock.waits.lock().unwrap().is_empty());
    let events = events.0.borrow();
    assert!(events.iter().any(|event| matches!(
        event,
        ModelEvent::TextDelta { text } if text == "partial"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        ModelEvent::AttemptFailed { code, retryable: false, .. }
            if code == "PROVIDER_STREAM_INTERRUPTED"
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        ModelEvent::RetryAttempt { .. } | ModelEvent::Fallback { .. }
    )));
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
    // Every chain entry - default AND each fallback - gets its own 3
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
    // A 401 is not retryable: no RetryAttempt, no waits - straight to
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

/// A provider that answers successfully with no text and no tool calls,
/// optionally recovering on a later call. Reproduces the output-budget
/// shape (`finish_reason=length`, empty content) and the blank-completion
/// shape.
struct EmptyStub {
    calls: AtomicUsize,
    finish_reason: &'static str,
    recover_on: usize,
}
impl ChatTransport for EmptyStub {
    fn post(&self, _req: &WireRequest) -> Result<String, PantheonError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if self.recover_on > 0 && n >= self.recover_on {
            return Ok(
                r#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#
                    .into(),
            );
        }
        Ok(format!(
            r#"{{"choices":[{{"message":{{"content":""}},"finish_reason":"{}"}}],"usage":{{"prompt_tokens":1,"completion_tokens":1}}}}"#,
            self.finish_reason
        ))
    }
    fn post_stream(
        &self,
        _req: &WireRequest,
        _on_payload: &mut dyn FnMut(&str) -> Result<(), PantheonError>,
    ) -> Result<StreamEnd, PantheonError> {
        unimplemented!("single-shot tests")
    }
}

fn empty_chain(
    provider: &str,
    finish_reason: &'static str,
    recover_on: usize,
) -> (ProviderChain<EmptyStub>, Arc<FakeClock>) {
    rate_provider(provider);
    let clock = Arc::new(FakeClock::default());
    let chain = ProviderChain {
        policy: policy(provider, &[]),
        transport: EmptyStub {
            calls: AtomicUsize::new(0),
            finish_reason,
            recover_on,
        },
        tools: vec![],
        api_key: Some(SecretValue::new("k1")),
        retry: RetryConfig::default(),
        sleeper: clock.clone(),
        last_resolved: RefCell::new(None),
        session_max_tokens: None,
        budget_max_tokens: None,
        oauth_broker: None,
    };
    (chain, clock)
}

#[test]
fn empty_completion_on_output_limit_retries_and_recovers() {
    // The real failure behind "the reasoning consumed the entire budget":
    // a successful provider turn with no visible text. It must retry, not
    // complete the run with a blank answer.
    let (chain, _) = empty_chain("retry-empty-limit", "length", 2);
    let events = Collect(RefCell::new(vec![]));
    let outcome = chain
        .turn_with_sink(&[Message::user("go")], &events)
        .expect("the retry recovers");
    assert!(matches!(
        outcome,
        pantheon_agent::TurnOutcome::Text { ref text, .. } if text == "ok"
    ));
    assert_eq!(chain.transport.calls.load(Ordering::SeqCst), 2);
    let borrowed = events.0.borrow();
    assert!(
        borrowed.iter().any(|e| matches!(
            e,
            ModelEvent::AttemptFailed {
                code,
                retryable: true,
                ..
            } if code == "PROVIDER_OUTPUT_LIMIT"
        )),
        "the empty attempt is reported as a retryable output-limit failure"
    );
    assert_eq!(
        borrowed
            .iter()
            .filter(|e| matches!(e, ModelEvent::Completed { .. }))
            .count(),
        1,
        "only the recovered attempt completes"
    );
}

#[test]
fn empty_completion_exhaustion_names_the_output_limit_fix() {
    let (chain, _) = empty_chain("retry-empty-limit-dead", "length", 0);
    let err = chain
        .turn_messages(&[Message::user("go")])
        .expect_err("all attempts were empty");
    assert_eq!(err.code, "PROVIDER_EXHAUSTED");
    assert_eq!(
        chain.transport.calls.load(Ordering::SeqCst),
        4,
        "initial attempt plus three retries"
    );
    assert!(
        err.cause.contains("PROVIDER_OUTPUT_LIMIT"),
        "exhaustion names the last failure: {}",
        err.cause
    );
    assert!(
        err.remediation.contains("max_tokens"),
        "the remedy must be actionable, not a reachability guess: {}",
        err.remediation
    );
}

#[test]
fn empty_completion_without_length_is_a_retryable_empty_answer() {
    let (chain, _) = empty_chain("retry-empty-stop", "stop", 2);
    let events = Collect(RefCell::new(vec![]));
    let outcome = chain
        .turn_with_sink(&[Message::user("go")], &events)
        .expect("the retry recovers");
    assert!(matches!(
        outcome,
        pantheon_agent::TurnOutcome::Text { ref text, .. } if text == "ok"
    ));
    assert!(events.0.borrow().iter().any(|e| matches!(
        e,
        ModelEvent::AttemptFailed {
            code,
            retryable: true,
            ..
        } if code == "PROVIDER_EMPTY_ANSWER"
    )));
}

/// Fails the first stream after emitting only reasoning, then succeeds.
/// Mirrors the common "stream ended without finish_reason" shape where the
/// model had already shown its thoughts but no answer.
struct ReasoningThenFailStub {
    calls: AtomicUsize,
}
impl ChatTransport for ReasoningThenFailStub {
    fn post(&self, _req: &WireRequest) -> Result<String, PantheonError> {
        panic!("streaming turns must use post_stream")
    }
    fn post_stream(
        &self,
        _req: &WireRequest,
        on_payload: &mut dyn FnMut(&str) -> Result<(), PantheonError>,
    ) -> Result<StreamEnd, PantheonError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if n == 1 {
            on_payload(
                r#"{"choices":[{"delta":{"reasoning_content":"thinking"},"finish_reason":null}]}"#,
            )?;
            return Err(PantheonError::new(
                "PROVIDER_READ",
                pantheon_api::error::Layer::Provider,
                true,
                "connection reset after reasoning",
                "retry the request",
                "",
            ));
        }
        on_payload(r#"{"choices":[{"delta":{"content":"done"},"finish_reason":null}]}"#)?;
        on_payload(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#)?;
        Ok(StreamEnd::Done)
    }
}

#[test]
fn failed_stream_after_reasoning_only_is_retried() {
    // Reasoning is not a deliverable answer and the ledger does not persist
    // it, so a failed stream that only streamed thoughts must retry rather
    // than surfacing a dead turn.
    rate_provider("retry-stream-thoughts");
    let clock = Arc::new(FakeClock::default());
    let chain = ProviderChain {
        policy: policy("retry-stream-thoughts", &[]),
        transport: ReasoningThenFailStub {
            calls: AtomicUsize::new(0),
        },
        tools: vec![],
        api_key: Some(SecretValue::new("k1")),
        retry: RetryConfig::default(),
        sleeper: clock.clone(),
        last_resolved: RefCell::new(None),
        session_max_tokens: None,
        budget_max_tokens: None,
        oauth_broker: None,
    };
    let events = Collect(RefCell::new(Vec::new()));

    let outcome = chain
        .turn_stream(&[Message::user("go")], &events)
        .expect("the retry recovers");
    assert!(matches!(
        outcome,
        pantheon_agent::TurnOutcome::Text { ref text, .. } if text == "done"
    ));
    assert_eq!(chain.transport.calls.load(Ordering::SeqCst), 2);
    let borrowed = events.0.borrow();
    assert!(
        borrowed
            .iter()
            .any(|e| matches!(e, ModelEvent::RetryAttempt { .. })),
        "a thoughts-only failed stream retries"
    );
    let visible: Vec<&str> = borrowed
        .iter()
        .filter_map(|e| match e {
            ModelEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(visible, ["done"], "no visible text is duplicated");
}
