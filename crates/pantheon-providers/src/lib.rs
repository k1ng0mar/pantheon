//! Providers (spec section 5 + 14): default model, ordered fallbacks
//! (failure-only, runtime-controlled), auxiliary models for scoped
//! capabilities. NO routing — locked decision.

pub mod anthropic;
pub mod chain;
pub mod compress;
pub mod decision;
pub mod http;
pub mod mock;
pub mod openai;
pub mod voice;

use pantheon_core::model::{AuxiliaryKind, AuxiliaryModel, DefaultModel, ModelPolicy};

pub use crate::chain::ProviderChain;
pub use crate::http::{http_timeout, ChatTransport, HttpTransport, ResolvedModel, WireRequest};
pub use crate::compress::CompressionClient;
pub use crate::decision::{DecisionClient, parse_answer, prompt_for};
pub use crate::mock::MockTransport;
pub use crate::voice::{
    SttProvider, SttRequest, SttResult, TtsProvider, TtsRequest, TtsResult, open_stt, open_tts,
    stt_backends, tts_backends,
};
pub use pantheon_core::catalog::{ApiMode, ModelCost, ModelMeta, ProviderMeta};

/// Select the model for a run: always the default.
pub fn for_run(policy: &ModelPolicy) -> &DefaultModel {
    &policy.default
}

/// After a retryable default failure at `failed_index`, the next fallback.
/// `None` failed_index starts the chain; `None` return exhausts it.
pub fn on_retryable_failure<'a>(
    policy: &'a ModelPolicy,
    failed_index: Option<usize>,
) -> Option<(usize, &'a DefaultModel)> {
    policy.fallbacks.next_after(failed_index)
}

/// Auxiliary for a scoped *model* capability (embeddings, rerank, vision...).
/// Services like STT/TTS are provider-plane, not model policy.
pub fn auxiliary<'a>(policy: &'a ModelPolicy, kind: &AuxiliaryKind) -> Option<&'a AuxiliaryModel> {
    policy.auxiliary(kind)
}

pub use pantheon_core::model::ModelPolicy as Policy;

#[cfg(test)]
mod tests {
    use super::*;
    use pantheon_core::model::FallbackChain;
    fn pol() -> ModelPolicy {
        ModelPolicy {
            default: DefaultModel {
                provider: "anthropic".into(),
                model: "sonnet".into(),
            },
            fallbacks: FallbackChain {
                fallbacks: vec![DefaultModel {
                    provider: "openai".into(),
                    model: "gpt".into(),
                }],
            },
            auxiliaries: vec![AuxiliaryModel {
                kind: AuxiliaryKind::Embeddings,
                provider: "local".into(),
                model: "e5".into(),
            }],
        }
    }
    #[test]
    fn default_always_wins() {
        let p = pol();
        assert_eq!(for_run(&p).model, "sonnet");
    }
    #[test]
    fn fallback_chain_failure_only() {
        let p = pol();
        let (i, m) = on_retryable_failure(&p, None).unwrap();
        assert_eq!((i, m.model.as_str()), (0, "gpt"));
        assert!(on_retryable_failure(&p, Some(0)).is_none());
    }
    #[test]
    fn aux_by_capability() {
        let p = pol();
        assert!(auxiliary(&p, &AuxiliaryKind::Embeddings).is_some());
        assert!(auxiliary(&p, &AuxiliaryKind::Vision).is_none());
    }
}
