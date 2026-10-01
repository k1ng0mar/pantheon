//! Normalized model events (§5 model layer): what a provider adapter emits,
//! independent of wire format. OpenAI-compat and Anthropic adapters both
//! project their streams into `ModelEvent`; the runtime projects
//! `ModelEvent` into the ledger `Event` at the boundary.
//!
//! Fallback/chain control also surfaces here (`Attempt`, `AttemptFailed`,
//! `Fallback`, `Exhausted`) so observability can answer "why fallback?"
//! without the agent loop knowing anything about providers.

use serde::{Deserialize, Serialize};

/// Token accounting for one completed attempt, plus cost when the catalog
/// knows the model's prices.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    /// Estimated USD cost; `None` when the catalog has no price for the model.
    pub cost_usd: Option<f64>,
}

/// One normalized provider event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ModelEvent {
    /// An attempt is starting against `provider`/`model`.
    Attempt {
        provider: String,
        model: String,
        /// 0 = default model, n = n-th fallback.
        chain_index: usize,
        streaming: bool,
    },
    /// Incremental assistant text.
    TextDelta { text: String },
    /// Incremental reasoning/thinking text (not part of the final answer).
    ReasoningDelta { text: String },
    /// One complete tool call (assembled from stream fragments or a
    /// single-shot response).
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    /// Token/cost accounting reported by the provider.
    Usage { usage: ModelUsage },
    /// Terminal success of an attempt.
    Completed { finish_reason: Option<String> },
    /// An attempt failed. The chain may still fall back when `retryable`.
    /// `cause` is a short human snippet (truncated provider error) so the
    /// error card and `pantheon logs` can answer WHY each failure happened,
    /// not just its code: `PROVIDER_HTTP` alone cannot tell a 429 from a
    /// 401, and the card classifies the kind from this snippet.
    AttemptFailed {
        provider: String,
        model: String,
        chain_index: usize,
        code: String,
        retryable: bool,
        cause: String,
    },
    /// The chain is retrying the SAME provider/model after a retryable
    /// failure (Umar's spec: up to 3 retries per chain entry, exponential
    /// backoff + jitter, Retry-After honored). `attempt` is the 1-based
    /// retry number, `max_attempts` the retry budget for this entry.
    /// Emitted BEFORE the backoff wait, so the UI can show
    /// "retrying 2/3" instead of a frozen screen while the wait elapses.
    RetryAttempt {
        provider: String,
        model: String,
        chain_index: usize,
        attempt: u32,
        max_attempts: u32,
        code: String,
        /// Seconds the chain waits before this retry (display only).
        wait_secs: u64,
    },
    /// The chain moved from `from_index` to fallback `to_index`.
    /// Carries the failure cause so the ledger records why.
    Fallback {
        from_index: usize,
        from_provider: String,
        from_model: String,
        from_code: String,
        from_cause: String,
        to_index: usize,
        to_provider: String,
        to_model: String,
    },
    /// Default and every fallback failed; the chain is done.
    Exhausted { code: String },
}

impl ModelEvent {
    /// Project into the core ledger `Event` for a run. Returns `None` for
    /// high-frequency or provider-internal events the ledger should not
    /// store (deltas are logged via `ModelDelta` only for visible text;
    /// reasoning/tool/usage rows stay provider-plane).
    ///
    /// `Exhausted` deliberately does NOT map to `RunFailed`. The chain's
    /// exhaustion is a provider-plane fact; whether to fail the run is a
    /// caller decision. The caller observes `Some(Event::RunProgress)`
    /// when `Exhausted` fires and chooses whether to mark the run failed.
    pub fn to_event(&self, run_id: &str) -> Option<pantheon_api::events::Event> {
        use pantheon_api::events::Event;
        match self {
            ModelEvent::Attempt { model, .. } => Some(Event::ModelRequested {
                run_id: run_id.to_string(),
                model: model.clone(),
            }),
            ModelEvent::TextDelta { text } => Some(Event::ModelDelta {
                run_id: run_id.to_string(),
                delta: text.clone(),
            }),
            ModelEvent::Completed { .. } => Some(Event::ModelCompleted {
                run_id: run_id.to_string(),
            }),
            ModelEvent::Fallback {
                from_provider,
                from_model,
                from_code,
                from_cause,
                to_index,
                to_model,
                to_provider,
                ..
            } => Some(Event::RunProgress {
                run_id: run_id.to_string(),
                detail: format!(
                    "fallback {from_provider}/{from_model} ({from_code}: {from_cause}) -> {to_provider}/{to_model} (chain index {to_index})"
                ),
            }),
            ModelEvent::AttemptFailed {
                provider,
                model,
                code,
                cause,
                ..
            } => Some(Event::RunProgress {
                run_id: run_id.to_string(),
                detail: format!("model attempt failed: {code} ({provider}/{model}): {cause}"),
            }),
            ModelEvent::RetryAttempt {
                provider,
                model,
                attempt,
                max_attempts,
                code,
                wait_secs,
                ..
            } => Some(Event::RunProgress {
                run_id: run_id.to_string(),
                detail: format!(
                    "model retrying: {provider}/{model} ({attempt}/{max_attempts}) after {code}; waiting {wait_secs}s"
                ),
            }),
            ModelEvent::Exhausted { code } => Some(Event::RunProgress {
                run_id: run_id.to_string(),
                detail: format!("provider chain exhausted: {code}"),
            }),
            ModelEvent::ReasoningDelta { .. } | ModelEvent::ToolCall { .. } => None,
            // Persisted as `Event::UsageRecorded` so `pantheon stats` can
            // aggregate historical spend from the ledger. Model/provider are
            // empty here: the persisting sink fills them from the most recent
            // `Attempt` it saw (this event shape is provider-facing and
            // carries no model identity of its own).
            ModelEvent::Usage { usage } => Some(Event::UsageRecorded {
                run_id: run_id.to_string(),
                model: String::new(),
                provider: String::new(),
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                total_tokens: usage.total_tokens,
                cost_usd: usage.cost_usd,
            }),
        }
    }
}

/// Sink for normalized model events. `&self` so adapters can take `&dyn`;
/// stateful sinks use interior mutability.
pub trait ModelEventSink {
    fn emit(&self, event: ModelEvent);
}

/// Default sink: drops everything.
pub struct NoopModelSink;
impl ModelEventSink for NoopModelSink {
    fn emit(&self, _event: ModelEvent) {}
}

impl<F: Fn(ModelEvent)> ModelEventSink for F {
    fn emit(&self, event: ModelEvent) {
        self(event)
    }
}
