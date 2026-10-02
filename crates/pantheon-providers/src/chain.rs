//! Fallback chain (spec §5 locked policy): default first, ordered fallbacks
//! on failure. Runtime-controlled, never agent-chosen.
//!
//! Retry policy (Umar's spec): each chain entry gets the initial attempt
//! plus up to 3 retries on retryable failure (429, 5xx, timeouts,
//! network errors), with exponential backoff + jitter and the provider's
//! `Retry-After` honored as a wait floor. Non-retryable failures
//! (auth 401/403, other 4xx, bad model) skip retries and go straight to
//! the next fallback. Adapters are single-attempt; the agent loop only
//! sees `ModelTurn`. The chain also owns the policy-plane `ModelEvent`s
//! (Attempt / AttemptFailed / RetryAttempt / Fallback / Exhausted /
//! Usage / Completed), resolving provider metadata - base URL, API key,
//! wire mode, capability/cost facts - from the core catalog.

use crate::catalog::{self, ApiMode};
use crate::error_kind::short_snippet;
use crate::http::{AdapterTurn, ChatTransport, ResolvedModel, TurnOptions};
use crate::model_event::{ModelEvent, ModelEventSink, NoopModelSink};
use crate::{anthropic, openai};
use pantheon_agent::TurnOutcome;
use pantheon_api::error::Layer;
use pantheon_api::error::PantheonError;
use pantheon_api::message::{Message, ToolSchema};
use pantheon_api::model::{DefaultModel, ModelPolicy};
use pantheon_secrets::SecretValue;
use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;

/// A `ModelTurn` backed by the policy chain. Streaming is a preference:
/// it applies only when the catalog says the model streams.
pub struct ProviderChain<T: ChatTransport> {
    pub policy: ModelPolicy,
    pub transport: T,
    pub tools: Vec<ToolSchema>,
    /// Resolved through SecretsBroker at the execution boundary.
    /// Stored as SecretValue so the key is never a plain String in
    /// memory beyond this point.
    pub api_key: Option<SecretValue>,
    /// Retry budget + backoff shape for the per-entry retry loop.
    pub retry: RetryConfig,
    /// Clock for backoff waits: real sleeps in prod, a fake clock in
    /// tests (no real sleeping in unit tests).
    pub sleeper: Arc<dyn Sleeper>,
    /// Resolved after the last turn (default or fallback index).
    pub last_resolved: RefCell<Option<ResolvedModel>>,
    /// Session-level per-request output cap (`/tokens N`): highest
    /// precedence in the max_tokens resolution (see
    /// [`pantheon_api::config::resolve_max_output_tokens`]).
    pub session_max_tokens: Option<u32>,
    /// `[budget].max_tokens` config value: middle precedence, below the
    /// session override and above the model's known maximum output.
    pub budget_max_tokens: Option<u32>,
}

/// Retry budget + backoff shape for one chain entry (Umar's spec:
/// retry 3 times before fallback or fail).
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Retries after the initial attempt, per chain entry.
    pub max_retries: u32,
    /// The first retry waits ~this; the wait doubles each retry, with
    /// full jitter applied (uniform in [0, exp]).
    pub base_delay: Duration,
    /// Cap for any single backoff wait.
    pub max_delay: Duration,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
        }
    }
}

/// Clock seam for retry backoff. Production sleeps for real; tests
/// record the waits (fake clock) so retry timing stays deterministic.
pub trait Sleeper: Send + Sync {
    fn sleep(&self, dur: Duration);
    /// Jitter fraction in [0.0, 1.0): random in prod, fixed in tests.
    fn jitter(&self) -> f64;
}

/// Production sleeper: real thread sleeps, xorshift jitter off the wall
/// clock (no rand crate in the provider plane).
pub struct ThreadSleeper;

impl Sleeper for ThreadSleeper {
    fn sleep(&self, dur: Duration) {
        std::thread::sleep(dur);
    }
    fn jitter(&self) -> f64 {
        // xorshift64* seeded from wall-clock nanos: good enough for
        // backoff jitter, no extra dependency.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0x9E37_79B9);
        let mut x = nanos | 1;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        let r = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (r >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Wait before retry number `retry` (1-based): exponential
/// `base * 2^(retry-1)` capped at `max_delay`, full jitter (uniform in
/// [0, exp]), then `max` with the provider's asked-for Retry-After
/// the provider's wait is a floor, not a suggestion. Pure, so the
/// backoff math is unit-testable without sleeping.
pub(crate) fn backoff_delay(
    cfg: &RetryConfig,
    retry: u32,
    jitter: f64,
    retry_after: Option<Duration>,
) -> Duration {
    let shift = retry.saturating_sub(1).min(10);
    let exp = cfg.base_delay.saturating_mul(1 << shift).min(cfg.max_delay);
    let jittered = Duration::from_secs_f64(exp.as_secs_f64() * jitter.clamp(0.0, 1.0));
    match retry_after {
        Some(ra) => jittered.max(ra),
        None => jittered,
    }
}

impl<T: ChatTransport> ProviderChain<T> {
    pub fn new(
        policy: ModelPolicy,
        transport: T,
        tools: Vec<ToolSchema>,
        api_key: SecretValue,
    ) -> Self {
        Self {
            policy,
            transport,
            tools,
            api_key: Some(api_key),
            retry: RetryConfig::default(),
            sleeper: Arc::new(ThreadSleeper),
            last_resolved: RefCell::new(None),
            session_max_tokens: None,
            budget_max_tokens: None,
        }
    }

    /// Backoff wait before retry `retry` (1-based) of this entry: the
    /// error's stamped Retry-After (already capped at parse time) floors
    /// the exponential+jitter wait, so a rate-limited provider is never
    /// hammered early.
    fn wait_before_retry(&self, retry: u32, err: &PantheonError) -> Duration {
        let retry_after = crate::http::retry_after_secs(err).map(Duration::from_secs);
        backoff_delay(&self.retry, retry, self.sleeper.jitter(), retry_after)
    }

    /// Run one attempt against `model`, emitting Attempt + adapter events +
    /// Usage/Completed (or AttemptFailed). No fallback here.
    ///
    /// The resolved key may stack several keys comma-separated
    /// (`KEY=k1,k2` in `<data_dir>/.env`, written by `pantheon model`).
    /// Keys are tried in order; a 401/403/429 on one key rotates to the
    /// next instead of failing the turn, so stacked keys spread quota
    /// and survive single-key revocation.
    fn attempt(
        &self,
        idx: usize,
        model: &DefaultModel,
        messages: &[Message],
        stream: bool,
        opts: &TurnOptions,
        sink: &dyn ModelEventSink,
    ) -> Result<AdapterTurn, PantheonError> {
        let meta = catalog::model_meta(&model.provider, &model.model);
        // A session model below the 32k context floor is refused before
        // any request goes out (unknown window fails open).
        pantheon_api::config::check_context_window(&model.model, meta.context_limit)?;
        // Per-request output cap: /tokens > [budget].max_tokens > the
        // model's known maximum output (16k fallback when unknown),
        // clamped to the model's known maximum. The aux paths (title,
        // judge, compress, ...) carry their own fixed wire budgets and
        // never reach this code.
        let max_tokens = pantheon_api::config::resolve_max_output_tokens(
            self.session_max_tokens,
            self.budget_max_tokens,
            meta.max_output_tokens,
        );
        let api_mode = catalog::provider(&model.provider)
            .map(|p| p.api_mode)
            .unwrap_or(ApiMode::OpenAi);
        let base = crate::http::resolve_base(&model.provider)?;
        // The configured key belongs to the default provider. Fallbacks may
        // use their own catalog/env credential, but must never receive the
        // primary provider's secret by accident.
        let configured = if idx == 0 {
            self.api_key.as_ref().map(|kv| kv.expose()).unwrap_or("")
        } else {
            ""
        };
        let key = catalog::key_for(&model.provider, configured);
        let mut keys: Vec<String> = key
            .split(',')
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .collect();
        if keys.is_empty() {
            keys.push(String::new());
        }
        let stream = stream && meta.streaming;
        sink.emit(ModelEvent::Attempt {
            provider: model.provider.clone(),
            model: model.model.clone(),
            chain_index: idx,
            streaming: stream,
        });

        let mut key_failures = 0usize;
        for (ki, one_key) in keys.iter().enumerate() {
            let last_key = ki + 1 >= keys.len();
            let mut result: Result<AdapterTurn, PantheonError> = match api_mode {
                ApiMode::OpenAi => {
                    let key_header = catalog::key_header_for(&model.provider);
                    let req = openai::request(
                        &base,
                        one_key,
                        &key_header,
                        &model.model,
                        messages,
                        &self.tools,
                        stream,
                        max_tokens,
                        self.policy.reasoning,
                        opts,
                    );
                    if stream {
                        openai::stream(&self.transport, req, sink)
                    } else {
                        openai::complete(&self.transport, req, sink)
                    }
                }
                ApiMode::Anthropic => {
                    let req = anthropic::request(
                        &base,
                        one_key,
                        &model.model,
                        messages,
                        &self.tools,
                        stream,
                        max_tokens,
                        self.policy.reasoning,
                        self.policy.reasoning_budget,
                        opts,
                    );
                    if stream {
                        anthropic::stream(&self.transport, req, sink)
                    } else {
                        anthropic::complete(&self.transport, req, sink)
                    }
                }
            };

            match &mut result {
                Ok(turn) => {
                    if let Some(mut usage) = turn.usage {
                        usage.cost_usd =
                            meta.cost.estimate(usage.input_tokens, usage.output_tokens);
                        // The adapter baked `cost_cents` at parse time from
                        // `cost_usd: None` (always 0) - it never sees catalog
                        // prices, so the chain owns the fix-up. Cost feeds
                        // stats only (there is no cost cap); without this
                        // every tracked turn would report $0.
                        if let Some(usd) = usage.cost_usd {
                            stamp_outcome_cost(&mut turn.outcome, (usd * 100.0) as u32);
                        }
                        sink.emit(ModelEvent::Usage { usage });
                    }
                    sink.emit(ModelEvent::Completed {
                        finish_reason: turn.finish_reason.clone(),
                    });
                    return result;
                }
                Err(e) if !last_key && is_key_failure(e) => {
                    // This key is dead/over quota; the next stacked key gets
                    // the turn. A 429 stamps the provider's asked-for wait:
                    // honor it before rotating so we stop hammering a
                    // rate-limited endpoint. Marked retryable so
                    // `pantheon logs` shows rotation.
                    self.backoff_for_rate_limit(e);
                    key_failures += 1;
                    sink.emit(ModelEvent::AttemptFailed {
                        provider: model.provider.clone(),
                        model: model.model.clone(),
                        chain_index: idx,
                        code: format!("{}:key{key_failures}", e.code),
                        retryable: true,
                        cause: failure_snippet(e),
                    });
                    continue;
                }
                Err(e) => {
                    sink.emit(ModelEvent::AttemptFailed {
                        provider: model.provider.clone(),
                        model: model.model.clone(),
                        chain_index: idx,
                        code: e.code.clone(),
                        retryable: e.retryable,
                        cause: failure_snippet(e),
                    });
                    return result;
                }
            }
        }
        unreachable!("keys is never empty")
    }

    /// Next chain entry after `failed` (its chain index). `None` exhausts.
    /// (Name is historical: the walk now continues on any failure
    /// retryable ones burn their retries first, non-retryable ones move
    /// straight on.)
    fn next_in_chain(&self, failed: usize) -> Option<(usize, &DefaultModel)> {
        let prev = failed.checked_sub(1);
        crate::on_retryable_failure(&self.policy, prev).map(|(ni, m)| (ni + 1, m))
    }

    /// One chain entry: the initial attempt plus up to
    /// `retry.max_retries` retries on retryable failure only, with
    /// exponential backoff + jitter (Retry-After honored as a floor).
    /// Emits `RetryAttempt` before each wait so the UI shows
    /// "retrying n/3" instead of a frozen screen. Non-retryable failures
    /// skip retries entirely - the caller walks straight to the next
    /// fallback.
    fn attempt_with_retries(
        &self,
        idx: usize,
        model: &DefaultModel,
        messages: &[Message],
        stream: bool,
        opts: &TurnOptions,
        sink: &dyn ModelEventSink,
    ) -> Result<AdapterTurn, PantheonError> {
        let mut last_err = match self.attempt(idx, model, messages, stream, opts, sink) {
            Ok(turn) => return Ok(turn),
            Err(e) => e,
        };
        for retry in 1..=self.retry.max_retries {
            if !last_err.retryable {
                break;
            }
            let wait = self.wait_before_retry(retry, &last_err);
            pantheon_api::logging::warn(
                "provider",
                format!(
                    "retry {retry}/{} {}/{} after {}s ({})",
                    self.retry.max_retries,
                    model.provider,
                    model.model,
                    wait.as_secs_f64().ceil() as u64,
                    last_err.code
                ),
            );
            sink.emit(ModelEvent::RetryAttempt {
                provider: model.provider.clone(),
                model: model.model.clone(),
                chain_index: idx,
                attempt: retry,
                max_attempts: self.retry.max_retries,
                code: last_err.code.clone(),
                wait_secs: wait.as_secs_f64().ceil() as u64,
            });
            self.sleeper.sleep(wait);
            match self.attempt(idx, model, messages, stream, opts, sink) {
                Ok(turn) => return Ok(turn),
                Err(e) => last_err = e,
            }
        }
        Err(last_err)
    }

    /// Honor a parsed `Retry-After` before rotating a stacked key. The
    /// transport stamps the capped wait on the error cause; no stamp →
    /// no sleep, exactly as before. Goes through the injected sleeper so
    /// tests stay sleep-free.
    fn backoff_for_rate_limit(&self, e: &PantheonError) {
        if let Some(secs) = crate::http::retry_after_secs(e).filter(|&s| s > 0) {
            pantheon_api::logging::warn(
                "provider",
                format!("rate limited: honoring Retry-After, waiting {secs}s before retry"),
            );
            self.sleeper.sleep(Duration::from_secs(secs));
        }
    }

    /// Walk the chain: default, then each fallback on failure. A
    /// retryable failure retries the same entry up to 3 times first
    /// (exponential backoff + jitter, Retry-After honored); a
    /// non-retryable failure skips retries and goes straight to the next
    /// entry. Emits chain events for every transition, including the
    /// failure cause on each Fallback so `pantheon logs` answers why.
    fn run(
        &self,
        messages: &[Message],
        stream: bool,
        opts: &TurnOptions,
        sink: &dyn ModelEventSink,
    ) -> Result<TurnOutcome, PantheonError> {
        // Vision gate (provider/chain layer, resolved from the catalog
        // never the UI): image parts need a vision-capable model. A
        // non-vision model would 400 mid-turn or silently ignore the
        // pictures, so this fails LOUDLY before the first attempt - no
        // fallback either, since silently switching models on a capability
        // mismatch is exactly the drop this gate exists to prevent.
        let image_count: usize = messages.iter().map(|m| m.images.len()).sum();
        if image_count > 0 {
            if image_count > pantheon_api::message::MAX_IMAGES_PER_MESSAGE {
                return Err(PantheonError::new(
                    "VISION_TOO_MANY_IMAGES",
                    Layer::Provider,
                    false,
                    format!(
                        "turn carries {image_count} images, over the {}-per-turn limit",
                        pantheon_api::message::MAX_IMAGES_PER_MESSAGE
                    ),
                    "send fewer images per turn",
                    "",
                ));
            }
            let meta =
                catalog::model_meta(&self.policy.default.provider, &self.policy.default.model);
            if !meta.vision {
                return Err(PantheonError::new(
                    "VISION_UNSUPPORTED",
                    Layer::Provider,
                    false,
                    format!(
                        "turn carries {image_count} image(s) but {}/{} has no vision support (catalog vision=false)",
                        self.policy.default.provider, self.policy.default.model
                    ),
                    "use a vision-capable model for turns with images, or attach the files as plain paths instead",
                    "",
                ));
            }
        }
        // (failed_idx, provider, model, code, cause-snippet) of last failure.
        let mut failed: Option<(usize, String, String, String, String)> = None;
        loop {
            let (idx, model) = match &failed {
                None => (0usize, &self.policy.default),
                Some((f, fp, fm, fc, fcause)) => match self.next_in_chain(*f) {
                    Some((ni, nm)) => {
                        // The ledger records the fallback, but the ledger is
                        // per-run and a chain that fails before any run is
                        // recorded leaves no trace at all. This is the one
                        // path that answers "why was it slow / why did it end
                        // up on that model", so it goes to the log too.
                        pantheon_api::logging::warn(
                            "provider",
                            format!(
                                "fallback {f} ({fp}/{fm}, {fc}) → {ni} ({}/{})",
                                nm.provider, nm.model
                            ),
                        );
                        sink.emit(ModelEvent::Fallback {
                            from_index: *f,
                            from_provider: fp.clone(),
                            from_model: fm.clone(),
                            from_code: fc.clone(),
                            from_cause: fcause.clone(),
                            to_index: ni,
                            to_provider: nm.provider.clone(),
                            to_model: nm.model.clone(),
                        });
                        (ni, nm)
                    }
                    None => {
                        pantheon_api::logging::error(
                            "provider",
                            match &failed {
                                Some((_, fp, fm, fc, _)) => {
                                    format!("chain exhausted; last failure {fc} on {fp}/{fm}")
                                }
                                None => "chain exhausted; no provider was configured".to_string(),
                            },
                        );
                        sink.emit(ModelEvent::Exhausted {
                            code: "PROVIDER_EXHAUSTED".into(),
                        });
                        // The tuple already carries the code of the failure
                        // that ended the chain. Reporting only "all fallbacks
                        // failed" threw away the only actionable part: a
                        // connection refused and an auth rejection need
                        // completely different fixes, and both arrived here
                        // as the same three words.
                        let (cause, entry) = match &failed {
                            Some((_, fp, fm, fc, _)) => {
                                (format!("last failure: {fc}"), format!("{fp}/{fm}"))
                            }
                            None => ("no provider was configured".to_string(), "none".into()),
                        };
                        let remedy = format!(
                            "check {entry}: is the endpoint reachable, is the API key set, \
                             and does that provider serve that model? `pantheon doctor` and \
                             `pantheon model --list` check both"
                        );
                        return Err(PantheonError::new(
                            "PROVIDER_EXHAUSTED",
                            Layer::Provider,
                            false,
                            format!("default and all fallbacks failed ({cause})"),
                            remedy,
                            "",
                        ));
                    }
                },
            };
            match self.attempt_with_retries(idx, model, messages, stream, opts, sink) {
                Ok(turn) => {
                    *self.last_resolved.borrow_mut() = Some(ResolvedModel {
                        provider: model.provider.clone(),
                        model: model.model.clone(),
                        chain_index: idx,
                    });
                    return Ok(turn.outcome);
                }
                Err(e) => {
                    // Retryable or not, the walk continues to the next
                    // chain entry: retryable failures already burned their
                    // 3 retries inside attempt_with_retries (each wait
                    // honored Retry-After, so no extra pacing is needed
                    // before the fallback); non-retryable failures skip
                    // retries entirely. The loop top emits Fallback - or
                    // Exhausted when nothing is left - so the verdict
                    // always renders.
                    failed = Some((
                        idx,
                        model.provider.clone(),
                        model.model.clone(),
                        e.code.clone(),
                        failure_snippet(&e),
                    ));
                    continue;
                }
            }
        }
    }

    /// Direct canonical-message turn (the wired loop uses this): policy
    /// events flow into `sink`.
    pub fn turn_with_sink(
        &self,
        messages: &[Message],
        sink: &dyn ModelEventSink,
    ) -> Result<TurnOutcome, PantheonError> {
        self.run(messages, false, &TurnOptions::default(), sink)
    }

    /// Same, with per-turn wire knobs (structured output, tool choice).
    pub fn turn_with_options(
        &self,
        messages: &[Message],
        opts: &TurnOptions,
        sink: &dyn ModelEventSink,
    ) -> Result<TurnOutcome, PantheonError> {
        self.run(messages, false, opts, sink)
    }

    /// Streaming turn: deltas emit as chunks arrive (subject to catalog
    /// `streaming` support per model).
    pub fn turn_stream(
        &self,
        messages: &[Message],
        sink: &dyn ModelEventSink,
    ) -> Result<TurnOutcome, PantheonError> {
        self.run(messages, true, &TurnOptions::default(), sink)
    }

    /// Streaming turn with per-turn wire knobs.
    pub fn turn_stream_with_options(
        &self,
        messages: &[Message],
        opts: &TurnOptions,
        sink: &dyn ModelEventSink,
    ) -> Result<TurnOutcome, PantheonError> {
        self.run(messages, true, opts, sink)
    }

    /// Compat turn: no sink, single-shot.
    pub fn turn_messages(&self, messages: &[Message]) -> Result<TurnOutcome, PantheonError> {
        let noop = NoopModelSink;
        self.run(messages, false, &TurnOptions::default(), &noop)
    }
}

impl<T: ChatTransport> pantheon_agent::ModelTurn for ProviderChain<T> {
    fn turn(&self, transcript: &[String]) -> Result<TurnOutcome, PantheonError> {
        // Legacy string transcript -> canonical messages (compat path).
        let msgs: Vec<Message> = transcript
            .iter()
            .map(|l| {
                if let Some(rest) = l.strip_prefix("user: ") {
                    Message::user(rest)
                } else if let Some(rest) = l.strip_prefix("assistant: ") {
                    Message::assistant(rest)
                } else if l.starts_with("tool") {
                    let id = l
                        .split('[')
                        .nth(1)
                        .and_then(|s| s.split(']').next())
                        .unwrap_or("t")
                        .to_string();
                    Message::tool(format!("call_{id}"), l)
                } else {
                    Message::user(l.clone())
                }
            })
            .collect();
        self.turn_messages(&msgs)
    }
}

/// Stamp an estimated cost onto a turn outcome. The `Delegate` variant
/// carries no cost accounting, so it is left alone.
fn stamp_outcome_cost(outcome: &mut TurnOutcome, cost_cents: u32) {
    match outcome {
        TurnOutcome::Text { cost_cents: c, .. } => *c = cost_cents,
        TurnOutcome::Tools { cost_cents: c, .. } => *c = cost_cents,
        TurnOutcome::Delegate { .. } => {}
    }
}

/// Short, redacted cause snippet for `AttemptFailed` / `Fallback`
/// events. A provider error body can echo request data, so the shared
/// redaction pass runs before the snippet reaches the ledger, the TUI
/// card, or `pantheon logs`. A 429's asked-for wait survives truncation:
/// it is the most actionable part of a rate limit.
fn failure_snippet(e: &PantheonError) -> String {
    let redacted = pantheon_api::logging::redact(&e.cause);
    let mut s = short_snippet(&redacted, 160);
    if !s.contains("(retry-after:") {
        if let Some(secs) = crate::error_kind::retry_after_secs_from_cause(&redacted) {
            s.push_str(&format!(" (retry-after: {secs}s)"));
        }
    }
    s
}

/// A stacked key is worth rotating past only for credential/quota
/// failures: 401/403 (dead key) and 429 (this key is over quota).
/// Anything else (bad request, server error, network) fails the turn
/// as before - rotating keys cannot fix it.
fn is_key_failure(e: &PantheonError) -> bool {
    e.code == "PROVIDER_HTTP"
        && (e.cause.contains("HTTP 401")
            || e.cause.contains("HTTP 403")
            || e.cause.contains("HTTP 429"))
}
