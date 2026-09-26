//! Tests for `pantheon_providers::embeddings::tests` — sibling file so sources stay test-free.
use super::*;
use serde_json::json;

#[test]
fn no_embeddings_auxiliary_means_local() {
    let policy = pantheon_core::model::ModelPolicy {
        default: pantheon_core::model::DefaultModel {
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
    let policy = pantheon_core::model::ModelPolicy {
        default: pantheon_core::model::DefaultModel {
            provider: "x".into(),
            model: "y".into(),
        },
        fallbacks: Default::default(),
        auxiliaries: vec![pantheon_core::model::AuxiliaryModel {
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
