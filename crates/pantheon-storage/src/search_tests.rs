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
fn reindexing_a_chunk_does_not_duplicate_search_hits() {
    let s = SessionSearch::open_in_memory().unwrap();
    let c = chunk("c1", "rotate the mcp oauth token");
    s.index(&c).unwrap();
    s.index(&c).unwrap();
    s.index(&c).unwrap();
    let hits = s.search("mcp oauth token", 50).unwrap();
    assert_eq!(
        hits.len(),
        1,
        "three indexes of one chunk must yield one hit, got {}",
        hits.len()
    );
    assert_eq!(hits[0].chunk.chunk_id, "c1");
}

#[test]
fn reindexing_updated_text_replaces_rather_than_accumulates() {
    let s = SessionSearch::open_in_memory().unwrap();
    s.index(&chunk("c1", "the original wording")).unwrap();
    s.index(&chunk("c1", "the replacement wording")).unwrap();
    let old = s.search("original", 50).unwrap();
    assert!(
        old.is_empty(),
        "stale text must not stay searchable: {old:?}"
    );
    let new = s.search("replacement", 50).unwrap();
    assert_eq!(new.len(), 1);
    assert!(new[0].chunk.text.contains("replacement"));
}

#[test]
fn distinct_chunks_are_all_returned() {
    let s = SessionSearch::open_in_memory().unwrap();
    s.index(&chunk("c1", "alpha shared term")).unwrap();
    s.index(&chunk("c2", "beta shared term")).unwrap();
    s.index(&chunk("c3", "gamma shared term")).unwrap();
    let hits = s.search("shared", 50).unwrap();
    assert_eq!(hits.len(), 3);
}

#[test]
fn prefix_matching_lets_a_shorter_query_find_a_longer_word() {
    // The trailing `*` is on the *query* term, so the query must be the
    // shorter prefix: "websocket" finds the indexed "websockets". Asking for
    // the plural does not match the singular.
    let s = SessionSearch::open_in_memory().unwrap();
    s.index(&chunk("c1", "the websockets handler")).unwrap();
    assert_eq!(s.search("websocket", 10).unwrap().len(), 1);
    assert_eq!(s.search("websockets", 10).unwrap().len(), 1);
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
fn drop_run_removes_chunks_and_their_fts_rows() {
    let s = SessionSearch::open_in_memory().unwrap();
    s.index(&chunk("c1", "findable before the drop")).unwrap();
    assert_eq!(s.search("findable", 10).unwrap().len(), 1);
    s.drop_run("run_abc").unwrap();
    assert!(s.search("findable", 10).unwrap().is_empty());
    // And re-indexing after a drop works, with no residue.
    s.index(&chunk("c1", "findable again")).unwrap();
    assert_eq!(s.search("findable", 10).unwrap().len(), 1);
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
