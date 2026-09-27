//! Tests for `pantheon_providers::embeddings::tests` — sibling file so sources stay test-free.
use super::*;
use serde_json::json;

#[test]
fn no_embeddings_auxiliary_means_local() {
    let policy = pantheon_api::model::ModelPolicy {
        reasoning_budget: Default::default(),
        reasoning: Default::default(),
        default: pantheon_api::model::DefaultModel {
            provider: "x".into(),
            model: "y".into(),
        },
        fallbacks: Default::default(),
        auxiliaries: vec![],
    };
    let c = EmbedClient::from_policy(&policy, None);
    assert_eq!(c.which(), Embedder::Local);
    assert_eq!(c.model_name(), "local-hash");
}

#[test]
fn embeddings_auxiliary_selects_provider() {
    let policy = pantheon_api::model::ModelPolicy {
        reasoning: Default::default(),
        reasoning_budget: Default::default(),
        default: pantheon_api::model::DefaultModel {
            provider: "x".into(),
            model: "y".into(),
        },
        fallbacks: Default::default(),
        auxiliaries: vec![pantheon_api::model::AuxiliaryModel {
            kind: AuxiliaryKind::Embeddings,
            provider: "router".into(),
            model: "text-embed".into(),
        }],
    };
    let c = EmbedClient::from_policy(&policy, None);
    assert_eq!(
        c.which(),
        Embedder::Provider(DefaultModel {
            provider: "router".into(),
            model: "text-embed".into()
        })
    );
}

#[test]
fn local_embed_is_deterministic_and_normalised() {
    let a = local_embed("debugging the Figma MCP server");
    let b = local_embed("debugging the Figma MCP server");
    assert_eq!(a.vec, b.vec, "deterministic");
    let n: f32 = a.vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((n - 1.0).abs() < 1e-4, "unit norm, got {n}");
    assert_eq!(a.dim, LOCAL_DIM);
}

#[test]
fn local_embed_similarity_ranks_related_text() {
    let q = local_embed("Figma MCP debugging");
    let hit = local_embed("debugging the Figma MCP server connection timeout");
    let miss = local_embed("schedule a dentist appointment next tuesday");
    assert!(
        cosine(&q.vec, &hit.vec) > cosine(&q.vec, &miss.vec),
        "related text outranks unrelated"
    );
}

#[test]
fn remote_embed_parses_openai_shape() {
    // Pure parser: real response shapes, no transport.
    let out = EmbedClient::parse_embeddings_response(
        &json!({"data": [{"embedding": [1.0, 0.0, 0.0]}, {"embedding": [0.0, 1.0, 0.0]}]})
            .to_string(),
        2,
    )
    .unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].dim, 3);
    assert_eq!(out[1].vec, vec![0.0, 1.0, 0.0]);
}

#[test]
fn remote_embed_rejects_bad_shapes() {
    assert!(EmbedClient::parse_embeddings_response(r#"{"data": []}"#, 2).is_err());
    assert!(EmbedClient::parse_embeddings_response(r#"{"nope": 1}"#, 1).is_err());
    assert!(EmbedClient::parse_embeddings_response("not json", 1).is_err());
    assert!(EmbedClient::parse_embeddings_response(
        &json!({"data": [{"embedding": []}]}).to_string(),
        1
    )
    .is_err());
}

#[test]
fn cosine_edge_cases() {
    assert_eq!(cosine(&[], &[]), 0.0);
    assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0, "dimension mismatch");
    assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
}

// --- Auth shape + wire-mode gating for embed_remote ---

use std::sync::{Arc, Mutex};

/// Fake transport capturing the wire request; replays a canned body.
struct CaptureTransport {
    captured: Arc<Mutex<Vec<WireRequest>>>,
    body: String,
}
impl ChatTransport for CaptureTransport {
    fn post(&self, req: &WireRequest) -> Result<String, PantheonError> {
        self.captured.lock().unwrap().push(req.clone());
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

fn embed_test_provider(id: &str, api_mode: catalog::ApiMode, key_header: &str) {
    catalog::register_custom_provider(catalog::ProviderMeta {
        id: id.to_string(),
        label: id.to_string(),
        base_url: "http://127.0.0.1:9/v1".to_string(),
        api_mode,
        base_env: String::new(),
        key_env: format!("PANTHEON_KEY_{}", id.to_ascii_uppercase()),
        key_header: key_header.to_string(),
        models: Vec::new(),
        prominent: false,
        dev: false,
        tag: "embed-test".to_string(),
    });
}

fn embed_policy(provider: &str) -> pantheon_api::model::ModelPolicy {
    pantheon_api::model::ModelPolicy {
        reasoning: Default::default(),
        reasoning_budget: Default::default(),
        default: pantheon_api::model::DefaultModel {
            provider: "x".into(),
            model: "y".into(),
        },
        fallbacks: Default::default(),
        auxiliaries: vec![pantheon_api::model::AuxiliaryModel {
            kind: AuxiliaryKind::Embeddings,
            provider: provider.into(),
            model: "text-embed".into(),
        }],
    }
}

fn capturing_client(
    provider: &str,
    key: Option<&str>,
    captured: &Arc<Mutex<Vec<WireRequest>>>,
) -> EmbedClient {
    EmbedClient::from_policy(
        &embed_policy(provider),
        key.map(pantheon_secrets::SecretValue::new),
    )
    .with_transport(Box::new(CaptureTransport {
        captured: captured.clone(),
        body: json!({"data": [{"embedding": [0.1, 0.2]}]}).to_string(),
    }))
}

#[test]
fn remote_embed_uses_catalog_key_header_not_hardcoded_bearer() {
    // Xiaomi-MiMo-style vendor header: raw key, no Bearer prefix, no
    // Authorization header at all.
    embed_test_provider("embedauth", catalog::ApiMode::OpenAi, "api-key");
    let captured = Arc::new(Mutex::new(Vec::new()));
    let c = capturing_client("embedauth", Some("sekret"), &captured);
    let out = c.embed(&["hi".to_string()]).expect("stubbed embed");
    assert_eq!(out.len(), 1);
    let reqs = captured.lock().unwrap();
    assert_eq!(reqs.len(), 1);
    let req = &reqs[0];
    assert!(req.url.ends_with("/embeddings"), "got {}", req.url);
    assert!(
        req.headers
            .iter()
            .any(|(k, v)| k == "api-key" && v == "sekret"),
        "raw key in the catalog header, got {:?}",
        req.headers
    );
    assert!(
        !req
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("authorization")),
        "no Authorization header when the catalog says otherwise: {:?}",
        req.headers
    );
}

#[test]
fn remote_embed_keeps_bearer_for_default_providers() {
    embed_test_provider("embedbearer", catalog::ApiMode::OpenAi, "Authorization");
    let captured = Arc::new(Mutex::new(Vec::new()));
    let c = capturing_client("embedbearer", Some("sekret"), &captured);
    c.embed(&["hi".to_string()]).expect("stubbed embed");
    let reqs = captured.lock().unwrap();
    assert!(
        reqs[0]
            .headers
            .iter()
            .any(|(k, v)| k == "Authorization" && v == "Bearer sekret"),
        "default providers keep Bearer auth, got {:?}",
        reqs[0].headers
    );
}

#[test]
fn remote_embed_rejects_non_openai_wire_mode_before_any_http() {
    // An Anthropic-Messages provider has no /embeddings endpoint: fail
    // fast instead of sending an OpenAI-shaped body at the wrong URL.
    embed_test_provider("embedmode", catalog::ApiMode::Anthropic, "Authorization");
    let captured = Arc::new(Mutex::new(Vec::new()));
    let c = capturing_client("embedmode", Some("sekret"), &captured);
    let err = c.embed(&["hi".to_string()]).unwrap_err();
    assert_eq!(err.code, "EMBED_MODE");
    assert!(
        captured.lock().unwrap().is_empty(),
        "mode gate must fire before any HTTP"
    );
}
