//! Providers (spec section 5 + 14): default model, ordered fallbacks
//! (failure-only, runtime-controlled), auxiliary models for scoped
//! capabilities. NO routing — locked decision.

mod answer_line;
pub mod anthropic;
pub mod catalog;
pub mod chain;
pub mod compress;
pub mod distill;
pub mod embeddings;
pub mod error_kind;
pub mod http;
pub mod judge;
pub mod model_event;
pub mod openai;
pub mod title;
pub mod verify;
pub mod video;
pub mod vision;
pub mod voice;

use pantheon_api::model::{AuxiliaryKind, AuxiliaryModel, DefaultModel, ModelPolicy};

pub use crate::catalog::{ApiMode, ModelCost, ModelMeta, ProviderMeta};
pub use crate::chain::ProviderChain;
pub use crate::compress::CompressionClient;
pub use crate::distill::{DistillClient, DISTILL_TIMEOUT_SECS};
pub use crate::error_kind::{
    classify_provider_error, display_message, retry_after_secs_from_cause, short_snippet,
    ProviderErrorKind,
};
pub use crate::http::{
    auth_header_pair, http_agent, http_timeout, parse_retry_after, retry_after_secs, ChatTransport,
    HttpTransport, ResolvedModel, ToolChoice, TurnOptions, WireRequest, MAX_RETRY_AFTER_SECS,
};
pub use crate::judge::{parse_answer, prompt_for, JudgeClient};
pub use crate::title::{TitleGenClient, TITLEGEN_TIMEOUT_SECS};
pub use crate::verify::{parse_verdict, VerifyClient, VerifyRequest, VerifyVerdict};
pub use crate::video::{
    bound_summary, extract_frames, native_request_body, parse_native_response,
    prompt_for as video_prompt_for, synthesis_prompt, VideoClient, VideoFrame, VideoRequest,
    VideoSummary, MAX_FRAMES, MAX_VIDEO_BYTES, VIDEO_MAX_TOKENS, VIDEO_SUMMARY_MAX_CHARS,
    VIDEO_TIMEOUT_SECS,
};
pub use crate::vision::{
    bound_description, pinned_vision_target, prompt_for as vision_prompt_for, VisionClient,
    VisionRequest, VisionResult, VISION_DESC_MAX_CHARS, VISION_MAX_TOKENS, VISION_TIMEOUT_SECS,
};
pub use crate::voice::{
    open_stt, open_tts, stt_backends, stt_from_config, stt_providers, tts_backends,
    tts_from_config, tts_providers, voice_api_key, voice_key_env, AuthRequirement, SttProvider,
    SttRequest, SttResult, TtsProvider, TtsRequest, TtsResult, VoiceBackendInfo, VoiceBackendKind,
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
