//! Tests for the `session_search` sidecar.
//!
//! This module had no tests, which is how a real idempotency bug survived in
//! it: `session_fts` is a standalone FTS5 table, so `INSERT OR REPLACE` on it
//! cannot dedupe and every re-index appended a row, which `search`'s join then
//! returned as duplicate hits.
use super::*;

fn chunk(id: &str, text: &str) -> SessionChunk {
    SessionChunk {
        chunk_id: id.into(),
        run_id: "run_abc".into(),
        seq: 0,
        kind: "message".into(),
        text: text.into(),
        ts_ms: 1_700_000_000_000,
    }
}

#[test]
fn an_empty_or_punctuation_only_query_returns_nothing_rather_than_erroring() {
    let s = SessionSearch::open_in_memory().unwrap();
    s.index(&chunk("c1", "some text")).unwrap();
    for q in ["", "   ", "\"\"", "(){}[]:^*|,-"] {
        assert!(s.search(q, 10).unwrap().is_empty(), "query {q:?}");
    }
}

#[test]
fn a_tool_chunk_is_truncated_by_the_tool_limit() {
    let s = SessionSearch::open_in_memory().unwrap();
    let long = "t".repeat(TOOL_CHUNK_MAX + 500);
    let mut c = chunk("c1", &long);
    c.kind = "tool".into();
    s.index(&c).unwrap();
    let hits = s.search(&"t".repeat(10), 5).unwrap();
    assert!(!hits.is_empty());
    assert!(
        hits[0].chunk.text.chars().count() <= TOOL_CHUNK_MAX,
        "tool text must be clipped to the tool limit"
    );
}

#[test]
fn cosine_and_blob_round_trip() {
    let v = vec![0.1f32, 0.2, 0.3];
    let blob = embed_to_blob(Some(&v));
    let back = blob_to_embedding(blob);
    assert_eq!(back.unwrap(), v);
    assert!(blob_to_embedding(None).is_none());
    assert!((cosine(&v, &v) - 1.0).abs() < 1e-5);
    // A vector against its own negation is -1, not 0.
    assert!((cosine(&v, &[-0.1, -0.2, -0.3]) + 1.0).abs() < 1e-5);
    // A genuinely orthogonal vector: 0.1*0.2 + 0.2*(-0.1) + 0.3*0 == 0.
    assert!(cosine(&v, &[0.2, -0.1, 0.0]).abs() < 1e-5);
}
