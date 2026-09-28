//! Providers (spec section 5 + 14): default model, ordered fallbacks
//! (failure-only, runtime-controlled), auxiliary models for scoped
//! capabilities. NO routing — locked decision.

pub mod anthropic;
pub mod catalog;
pub mod chain;
pub mod compress;
pub mod distill;
pub mod embeddings;
pub mod http;
pub mod judge;
pub mod model_event;
pub mod openai;
pub mod title;
pub mod voice;

use pantheon_api::model::{AuxiliaryKind, AuxiliaryModel, DefaultModel, ModelPolicy};

pub use crate::catalog::{ApiMode, ModelCost, ModelMeta, ProviderMeta};
pub use crate::chain::ProviderChain;
pub use crate::compress::CompressionClient;
pub use crate::distill::{DistillClient, DISTILL_TIMEOUT_SECS};
pub use crate::http::{
    auth_header_pair, http_agent, http_timeout, parse_retry_after, retry_after_secs, ChatTransport,
    HttpTransport, ResolvedModel, ToolChoice, TurnOptions, WireRequest, MAX_RETRY_AFTER_SECS,
};
pub use crate::judge::{parse_answer, prompt_for, JudgeClient};
pub use crate::title::{TitleGenClient, TITLEGEN_TIMEOUT_SECS};
pub use crate::voice::{
    open_stt, open_tts, stt_backends, tts_backends, SttProvider, SttRequest, SttResult,
    TtsProvider, TtsRequest, TtsResult,
};

/// After a retryable default failure at `failed_index`, the next fallback.
/// `None` failed_index starts the chain; `None` return exhausts it.
pub fn on_retryable_failure(
    policy: &ModelPolicy,
    failed_index: Option<usize>,
) -> Option<(usize, &DefaultModel)> {
    policy.fallbacks.next_after(failed_index)
}

/// Auxiliary for a scoped *model* capability (embeddings, rerank, vision...).
/// Services like STT/TTS are provider-plane, not model policy.
pub fn auxiliary<'a>(policy: &'a ModelPolicy, kind: &AuxiliaryKind) -> Option<&'a AuxiliaryModel> {
    policy.auxiliary(kind)
}

pub use pantheon_api::model::ModelPolicy as Policy;

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
