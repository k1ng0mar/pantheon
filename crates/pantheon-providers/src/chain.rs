//! Fallback chain (spec §5 locked policy): default first, ordered fallbacks
//! on retryable failure only. Runtime-controlled, never agent-chosen.
//!
//! This is the ONLY place fallback logic exists. Adapters are
//! single-attempt; the agent loop only sees `ModelTurn`. The chain also
//! owns the policy-plane `ModelEvent`s (Attempt / AttemptFailed / Fallback /
//! Exhausted / Usage / Completed), resolving provider metadata — base URL,
//! API key, wire mode, capability/cost facts — from the core catalog.

use crate::catalog::{self, ApiMode};
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
    /// Resolved after the last turn (default or fallback index).
    pub last_resolved: RefCell<Option<ResolvedModel>>,
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
            last_resolved: RefCell::new(None),
        }
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
                    let max_tokens = meta
                        .max_output_tokens
                        .unwrap_or(anthropic::DEFAULT_MAX_TOKENS);
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
                        // `cost_usd: None` (always 0) — it never sees catalog
                        // prices, so the chain owns the fix-up. The agent
                        // loop's `max_cost_cents` budget reads
                        // `outcome.cost_cents()`; without this it can never
                        // trip.
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
                    backoff_for_rate_limit(e);
                    key_failures += 1;
                    sink.emit(ModelEvent::AttemptFailed {
                        provider: model.provider.clone(),
                        model: model.model.clone(),
                        chain_index: idx,
                        code: format!("{}:key{key_failures}", e.code),
                        retryable: true,
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
                    });
                    return result;
                }
            }
        }
        unreachable!("keys is never empty")
    }

    /// Next chain entry after `failed` (its chain index). `None` exhausts.
    fn next_in_chain(&self, failed: usize) -> Option<(usize, &DefaultModel)> {
        let prev = failed.checked_sub(1);
        crate::on_retryable_failure(&self.policy, prev).map(|(ni, m)| (ni + 1, m))
    }

    /// Walk the chain: default, then each fallback on retryable failure
    /// only. Emits chain events for every transition, including the
    /// failure cause on each Fallback so `pantheon logs` answers why.
    fn run(
        &self,
        messages: &[Message],
        stream: bool,
        opts: &TurnOptions,
        sink: &dyn ModelEventSink,
    ) -> Result<TurnOutcome, PantheonError> {
        // (failed_idx, provider, model, code, cause-snippet) of last failure.
        let mut failed: Option<(usize, String, String, String)> = None;
        loop {
            let (idx, model) = match &failed {
                None => (0usize, &self.policy.default),
                Some((f, fp, fm, fc)) => match self.next_in_chain(*f) {
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
                                Some((_, fp, fm, fc)) => {
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
                            Some((_, fp, fm, fc)) => {
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
            match self.attempt(idx, model, messages, stream, opts, sink) {
                Ok(turn) => {
                    *self.last_resolved.borrow_mut() = Some(ResolvedModel {
                        provider: model.provider.clone(),
                        model: model.model.clone(),
                        chain_index: idx,
                    });
                    return Ok(turn.outcome);
                }
                Err(e) if e.retryable => {
                    // Same pacing before a fallback: the 429's Retry-After
                    // applies to the provider, not the key, so wait before
                    // trying the next chain entry. No next entry → no wait;
                    // sleeping before exhaustion would just delay the error.
                    if self.next_in_chain(idx).is_some() {
                        backoff_for_rate_limit(&e);
                    }
                    failed = Some((
                        idx,
                        model.provider.clone(),
                        model.model.clone(),
                        e.code.clone(),
                    ));
                    continue;
                }
                Err(e) => return Err(e),
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
                } else if let Some(rest) = l.strip_prefix("tool") {
                    let id = l
                        .split('[')
                        .nth(1)
                        .and_then(|s| s.split(']').next())
                        .unwrap_or("t")
                        .to_string();
                    let _ = rest;
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

/// A stacked key is worth rotating past only for credential/quota
/// failures: 401/403 (dead key) and 429 (this key is over quota).
/// Anything else (bad request, server error, network) fails the turn
/// as before — rotating keys cannot fix it.
fn is_key_failure(e: &PantheonError) -> bool {
    e.code == "PROVIDER_HTTP"
        && (e.cause.contains("HTTP 401")
            || e.cause.contains("HTTP 403")
            || e.cause.contains("HTTP 429"))
}

/// Honor a parsed `Retry-After` before the next retry step (key rotation
/// or fallback). The transport stamps the capped wait on the error cause;
/// no stamp → no sleep, exactly as before.
fn backoff_for_rate_limit(e: &PantheonError) {
    if let Some(secs) = crate::http::retry_after_secs(e).filter(|&s| s > 0) {
        pantheon_api::logging::warn(
            "provider",
            format!("rate limited: honoring Retry-After, waiting {secs}s before retry"),
        );
        std::thread::sleep(std::time::Duration::from_secs(secs));
    }
}

#[cfg(test)]
#[path = "chain_tests.rs"]
mod tests;
