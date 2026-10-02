//! The single LLM-gating contract for the nightly pass.
//!
//! Three LLM touchpoints - proposal refinement, memory distillation, and
//! fix-loop draft revision - go through this one trait, and every call is
//! routed through an explicitly resolved [`AuxiliaryModel`]: the proposal
//! refiner uses the `Reflection` slot, the memory distiller uses the
//! `Consolidation` slot, and ALL fix-loop draft revision (eval-reject and
//! replay-reject paths) uses the `Repair` slot - never Reflection. None
//! ever touches the chat/default model. All LLM steps are OFF unless the
//! pass config has `enabled = true` AND a model policy resolves the slot;
//! anything else is a deterministic-only pass with zero model calls.

// The trait and its DTOs live at the API layer (`pantheon_api::nightly`)
// so providers can implement the contract without depending on this
// pipeline crate. Re-exported here so `pantheon_nightly::NightlyLlm`
// keeps resolving.
use pantheon_api::model::{AuxiliaryKind, AuxiliaryModel, ModelPolicy};
pub use pantheon_api::nightly::{resolve_refiner, resolve_repair, NightlyLlm};

/// A [`DistillBackend`] bound to one resolved auxiliary model: the
/// adapter `run_pass` hands to the weigh phase. Construction is the only
/// place the `Consolidation` slot is resolved, so every distill call in
/// the pipeline provably routes through it.
pub struct ResolvedDistill<'a> {
    llm: &'a dyn NightlyLlm,
    aux: &'a AuxiliaryModel,
}

/// Backend trait for the weigh phase: takes candidate texts, returns
/// durable facts. The default is [`DeterministicDistill`] (identity).
pub trait DistillBackend: Send + Sync {
    fn distill(&self, texts: &[String]) -> Result<Vec<String>, String>;
    fn name(&self) -> &'static str {
        "distill"
    }
}

impl DistillBackend for ResolvedDistill<'_> {
    fn distill(&self, texts: &[String]) -> Result<Vec<String>, String> {
        self.llm.distill_memories(self.aux, texts)
    }
    fn name(&self) -> &'static str {
        "auxiliary"
    }
}

/// No-model distiller: identity. Used whenever LLM steps are disabled
/// zero model calls.
pub struct DeterministicDistill;

impl DistillBackend for DeterministicDistill {
    fn distill(&self, texts: &[String]) -> Result<Vec<String>, String> {
        Ok(texts.to_vec())
    }
    fn name(&self) -> &'static str {
        "deterministic"
    }
}

/// Resolve the memory distiller: `Some` only when the policy carries a
/// `Consolidation` entry. The caller additionally gates on
/// `config.enabled`.
pub fn resolve_distill<'a>(
    llm: &'a dyn NightlyLlm,
    policy: &'a ModelPolicy,
) -> Option<ResolvedDistill<'a>> {
    policy
        .auxiliary(&AuxiliaryKind::Consolidation)
        .map(|aux| ResolvedDistill { llm, aux })
}

// Small deterministic invariant tests only.
