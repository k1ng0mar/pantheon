//! Fallback chain (spec §5 locked policy): default first, ordered fallbacks
//! on retryable failure only. Runtime-controlled, never agent-chosen.
//!
//! This is the ONLY place fallback logic exists. Adapters are
//! single-attempt; the agent loop only sees `ModelTurn`. The chain also
//! owns the policy-plane `ModelEvent`s (Attempt / AttemptFailed / Fallback /
//! Exhausted / Usage / Completed), resolving provider metadata — base URL,
//! API key, wire mode, capability/cost facts — from the core catalog.

use crate::http::{AdapterTurn, ChatTransport, ResolvedModel};
use crate::{anthropic, openai};
use pantheon_agent::TurnOutcome;
use pantheon_core::catalog::{self, ApiMode};
use pantheon_core::error::Layer;
use pantheon_core::error::PantheonError;
use pantheon_core::message::{Message, ToolSchema};
use pantheon_core::model::{DefaultModel, ModelPolicy};
use pantheon_core::model_event::{ModelEvent, ModelEventSink, NoopModelSink};
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
            let result: Result<AdapterTurn, PantheonError> = match api_mode {
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
                    );
                    if stream {
                        anthropic::stream(&self.transport, req, sink)
                    } else {
                        anthropic::complete(&self.transport, req, sink)
                    }
                }
            };

            match &result {
                Ok(turn) => {
                    if let Some(mut usage) = turn.usage {
                        usage.cost_usd =
                            meta.cost.estimate(usage.input_tokens, usage.output_tokens);
                        sink.emit(ModelEvent::Usage { usage });
                    }
                    sink.emit(ModelEvent::Completed {
                        finish_reason: turn.finish_reason.clone(),
                    });
                    return result;
                }
                Err(e) if !last_key && is_key_failure(e) => {
                    // This key is dead/over quota; the next stacked key gets
                    // the turn. Marked retryable so /explain shows rotation.
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
    /// failure cause on each Fallback so `/explain` answers why.
    fn run(
        &self,
        messages: &[Message],
        stream: bool,
        sink: &dyn ModelEventSink,
    ) -> Result<TurnOutcome, PantheonError> {
        // (failed_idx, provider, model, code, cause-snippet) of last failure.
        let mut failed: Option<(usize, String, String, String)> = None;
        loop {
            let (idx, model) = match &failed {
                None => (0usize, &self.policy.default),
                Some((f, fp, fm, fc)) => match self.next_in_chain(*f) {
                    Some((ni, nm)) => {
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
            match self.attempt(idx, model, messages, stream, sink) {
                Ok(turn) => {
                    *self.last_resolved.borrow_mut() = Some(ResolvedModel {
                        provider: model.provider.clone(),
                        model: model.model.clone(),
                        chain_index: idx,
                    });
                    return Ok(turn.outcome);
                }
                Err(e) if e.retryable => {
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
        self.run(messages, false, sink)
    }

    /// Streaming turn: deltas emit as chunks arrive (subject to catalog
    /// `streaming` support per model).
    pub fn turn_stream(
        &self,
        messages: &[Message],
        sink: &dyn ModelEventSink,
    ) -> Result<TurnOutcome, PantheonError> {
        self.run(messages, true, sink)
    }

    /// Compat turn: no sink, single-shot.
    pub fn turn_messages(&self, messages: &[Message]) -> Result<TurnOutcome, PantheonError> {
        let noop = NoopModelSink;
        self.run(messages, false, &noop)
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

#[cfg(test)]
#[path = "chain_tests.rs"]
mod tests;
