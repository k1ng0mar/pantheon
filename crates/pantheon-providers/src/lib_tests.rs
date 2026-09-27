//! Tests for `pantheon_providers::tests` — sibling file so sources stay test-free.
use super::*;
use pantheon_api::model::FallbackChain;
fn pol() -> ModelPolicy {
    ModelPolicy {
        reasoning_budget: Default::default(),
        reasoning: Default::default(),
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
fn default_is_policy_default() {
    let p = pol();
    assert_eq!(p.default.model, "sonnet");
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
