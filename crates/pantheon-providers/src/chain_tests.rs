//! Tests for `pantheon_providers::chain::tests` — sibling file so sources stay test-free.
//!
//! Pure/fake-transport tests live here. The live llm-router tests were
//! deleted (host-dependent skips), and the ~2s Retry-After backoff test
//! moved to `eval/tests/providers_chain_429.rs`.
use super::*;
use crate::catalog;
use crate::http::WireRequest;
use pantheon_api::model::{DefaultModel, FallbackChain, ModelPolicy};
use pantheon_secrets::SecretValue;

// --- Cost ceiling regression (no live router needed) ---

/// Fake transport: every POST returns one canned OpenAI chat-completion
/// response with fixed usage (100k prompt / 50k completion tokens).
struct StubTransport {
    body: String,
}
impl ChatTransport for StubTransport {
    fn post(&self, _req: &WireRequest) -> Result<String, PantheonError> {
        Ok(self.body.clone())
    }
    fn post_stream(
        &self,
        _req: &WireRequest,
        _on_payload: &mut dyn FnMut(&str) -> Result<(), PantheonError>,
    ) -> Result<(), PantheonError> {
        unimplemented!("single-shot test")
    }
}

#[test]
fn outcome_carries_catalog_cost_so_cost_ceiling_trips() {
    // Regression: the adapter bakes `cost_cents` at parse time from
    // `cost_usd: None` (always 0), so the agent loop's `max_cost_cents`
    // budget accumulated zeros and could never trip. The chain now
    // stamps the catalog estimate onto the outcome before returning it.
    // Uses a cataloged priced model (openai/gpt-4o-mini) so the estimate
    // is real; the stub transport means no network and no API key.
    let policy = ModelPolicy {
        reasoning_budget: Default::default(),
        reasoning: Default::default(),
        default: DefaultModel {
            provider: "openai".into(),
            model: "gpt-4o-mini".into(),
        },
        fallbacks: FallbackChain { fallbacks: vec![] },
        auxiliaries: vec![],
    };
    let body = serde_json::json!({
        "id": "chatcmpl-cost",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hello"},
            "finish_reason": "stop",
        }],
        "usage": {
            "prompt_tokens": 100_000,
            "completion_tokens": 50_000,
            "total_tokens": 150_000,
        },
    })
    .to_string();
    let chain = ProviderChain::new(policy, StubTransport { body }, vec![], SecretValue::new("k"));
    let out = chain
        .turn_messages(&[Message::user("hi")])
        .expect("stubbed turn should succeed");
    // Expected cost from the catalog itself, so price edits don't break
    // the test: (0.015 + 0.03) USD -> cents, same formula the chain uses.
    let expected = (catalog::model_meta("openai", "gpt-4o-mini")
        .cost
        .estimate(100_000, 50_000)
        .expect("catalog must price gpt-4o-mini")
        * 100.0) as u32;
    assert!(expected > 0, "test needs a priced model");
    assert_eq!(
        out.cost_cents(),
        expected,
        "outcome must carry the catalog estimate, got {out:?}"
    );
    // The accumulation the agent loop performs (`total_cost_cents +=
    // outcome.cost_cents()`, then `total_cost_cents >= max_cost_cents`):
    // a cap equal to this turn's cost trips on the single turn now.
    let mut total_cost_cents = 0u32;
    total_cost_cents += out.cost_cents();
    let cap = expected;
    assert!(
        total_cost_cents >= cap,
        "budget check (total {total_cost_cents} >= cap {cap}) must trip"
    );
}

/// Register (idempotent) a test provider whose key env is never exported,
/// so configured keys rule deterministically.
fn rate_provider(id: &str) {
    catalog::register_custom_provider(catalog::ProviderMeta {
        id: id.to_string(),
        label: id.to_string(),
        base_url: "http://127.0.0.1:9/v1".to_string(),
        api_mode: catalog::ApiMode::OpenAi,
        base_env: String::new(),
        key_env: format!("PANTHEON_KEY_{}", id.to_ascii_uppercase()),
        key_header: "Authorization".to_string(),
        models: Vec::new(),
        prominent: false,
        dev: false,
        tag: "rate-test".to_string(),
    });
}

#[test]
fn rate_limit_without_header_does_not_sleep() {
    // A 429 with no Retry-After header carries no marker: rotation and
    // fallback proceed immediately, exactly as before.
    rate_provider("ratelimit2");
    let policy = ModelPolicy {
        reasoning_budget: Default::default(),
        reasoning: Default::default(),
        default: DefaultModel {
            provider: "ratelimit2".into(),
            model: "chat".into(),
        },
        fallbacks: FallbackChain { fallbacks: vec![] },
        auxiliaries: vec![],
    };
    struct BareStub;
    impl ChatTransport for BareStub {
        fn post(&self, _req: &WireRequest) -> Result<String, PantheonError> {
            Err(PantheonError::new(
                "PROVIDER_HTTP",
                pantheon_api::error::Layer::Provider,
                true,
                "http://stub/v1: HTTP 429 slow down".to_string(),
                "wait and retry",
                "",
            ))
        }
        fn post_stream(
            &self,
            _req: &WireRequest,
            _on_payload: &mut dyn FnMut(&str) -> Result<(), PantheonError>,
        ) -> Result<(), PantheonError> {
            unimplemented!("single-shot test")
        }
    }
    let chain = ProviderChain::new(policy, BareStub, vec![], SecretValue::new("k"));
    let started = std::time::Instant::now();
    let err = chain.turn_messages(&[Message::user("go")]).unwrap_err();
    assert_eq!(err.code, "PROVIDER_EXHAUSTED");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "no Retry-After header must mean no sleep"
    );
}
