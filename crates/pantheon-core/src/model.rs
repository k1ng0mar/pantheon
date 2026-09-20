//! Model policy: NO routing. Default + ordered fallbacks (failure-only,
//! runtime-controlled) + auxiliary models for scoped capabilities.
//!
//! Mirrors Hermes behavior: one configured default, a fallback chain, and
//! auxiliary models for helpers (embeddings, rerank, STT/TTS, vision...).
//! The agent never selects. The runtime does.

use serde::{Deserialize, Serialize};

/// Which model a run uses. Single configured default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefaultModel {
    pub provider: String,
    pub model: String,
}

/// Ordered fallback chain. Used ONLY on retryable default-model failure
/// (unavailable, quota, 5xx, timeout). Never agent-chosen, never scored.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FallbackChain {
    pub fallbacks: Vec<DefaultModel>,
}

impl FallbackChain {
    pub fn next_after(&self, failed_index: Option<usize>) -> Option<(usize, &DefaultModel)> {
        let i = match failed_index {
            Some(i) => i + 1,
            None => 0,
        };
        self.fallbacks.get(i).map(|m| (i, m))
    }
}

/// Auxiliary model for a scoped capability (NOT a chat substitute).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuxiliaryKind {
    Embeddings,
    Rerank,
    SpeechToText,
    TextToSpeech,
    Vision,
    Extraction,
    SearchSynthesis,
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuxiliaryModel {
    pub kind: AuxiliaryKind,
    pub provider: String,
    pub model: String,
}

/// Full model config for a run/policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPolicy {
    pub default: DefaultModel,
    pub fallbacks: FallbackChain,
    pub auxiliaries: Vec<AuxiliaryModel>,
}

impl ModelPolicy {
    pub fn auxiliary(&self, kind: &AuxiliaryKind) -> Option<&AuxiliaryModel> {
        self.auxiliaries.iter().find(|a| &a.kind == kind)
    }
}
