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
    AttemptFailed {
        provider: String,
        model: String,
        chain_index: usize,
        code: String,
        retryable: bool,
    },
    /// The chain moved from `from_index` to fallback `to_index`.
    Fallback {
        from_index: usize,
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
    pub fn to_event(&self, run_id: &str) -> Option<crate::events::Event> {
        use crate::events::Event;
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
                to_index,
                to_model,
                to_provider,
                ..
            } => Some(Event::RunProgress {
                run_id: run_id.to_string(),
                detail: format!("fallback to {to_provider}/{to_model} (chain index {to_index})"),
            }),
            ModelEvent::AttemptFailed { code, .. } => Some(Event::RunProgress {
                run_id: run_id.to_string(),
                detail: format!("model attempt failed: {code}"),
            }),
            ModelEvent::Exhausted { code } => Some(Event::RunFailed {
                run_id: run_id.to_string(),
                code: code.clone(),
            }),
            ModelEvent::ReasoningDelta { .. }
            | ModelEvent::ToolCall { .. }
            | ModelEvent::Usage { .. } => None,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attempt_projects_to_model_requested() {
        let ev = ModelEvent::Attempt {
            provider: "openai".into(),
            model: "gpt-test".into(),
            chain_index: 0,
            streaming: true,
        };
        match ev.to_event("r1") {
            Some(crate::events::Event::ModelRequested { run_id, model }) => {
                assert_eq!(run_id, "r1");
                assert_eq!(model, "gpt-test");
            }
            other => panic!("unexpected projection: {other:?}"),
        }
    }

    #[test]
    fn text_delta_projects_and_reasoning_does_not() {
        let delta = ModelEvent::TextDelta { text: "hi".into() };
        assert!(matches!(
            delta.to_event("r1"),
            Some(crate::events::Event::ModelDelta { .. })
        ));
        let think = ModelEvent::ReasoningDelta { text: "hmm".into() };
        assert!(think.to_event("r1").is_none());
    }

    #[test]
    fn fallback_projects_to_run_progress() {
        let ev = ModelEvent::Fallback {
            from_index: 0,
            to_index: 1,
            to_provider: "deepseek".into(),
            to_model: "ds".into(),
        };
        match ev.to_event("r1") {
            Some(crate::events::Event::RunProgress { detail, .. }) => {
                assert!(detail.contains("fallback"));
                assert!(detail.contains("deepseek"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn closure_sink_works() {
        use std::cell::RefCell;
        let seen: RefCell<Vec<String>> = RefCell::new(vec![]);
        let sink = |ev: ModelEvent| {
            if let ModelEvent::TextDelta { text } = ev {
                seen.borrow_mut().push(text);
            }
        };
        sink.emit(ModelEvent::TextDelta { text: "a".into() });
        sink.emit(ModelEvent::Completed {
            finish_reason: Some("stop".into()),
        });
        assert_eq!(*seen.borrow(), vec!["a".to_string()]);
    }
}
