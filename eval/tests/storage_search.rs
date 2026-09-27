//! Behavioral / integration tests moved out of the crate per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval`.
use pantheon_storage::search::{SessionChunk, SessionSearch};

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
fn prune_before_drops_stale_chunks_and_their_fts_rows() {
    let s = SessionSearch::open_in_memory().unwrap();
    let mut old = chunk("c-old", "ancient wisdom about flurbleolds");
    old.ts_ms = 1;
    let mut new = chunk("c-new", "recent wisdom about flurblenews");
    new.ts_ms = 9_999_999_999_999;
    s.index(&old).unwrap();
    s.index(&new).unwrap();
    assert_eq!(s.prune_before(1_000).unwrap(), 1);
    // The stale chunk's FTS row is gone too, not just the chunk table row.
    assert!(s.search("flurbleolds", 10).unwrap().is_empty());
    let hits = s.search("flurblenews", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].chunk.chunk_id, "c-new");
    // Pruning again with a past cutoff is a no-op.
    assert_eq!(s.prune_before(0).unwrap(), 0);
}
