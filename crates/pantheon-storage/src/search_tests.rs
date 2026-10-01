//! Fresh coverage for the `session_search` sidecar's core path: FTS
//! indexing + lexical search over temp-dir file DBs.
//!
//! (A sibling module in `ledger.rs` covers `delete_run`'s FTS cleanup from
//! the ledger side; the no-ghost-hits case here asserts it from the search
//! side.)
use super::*;
use crate::ledger::Ledger;
use pantheon_api::events::Event;
use tempfile::TempDir;

fn file_search() -> (SessionSearch, TempDir) {
    let dir = TempDir::new().expect("temp dir");
    let db = dir.path().join("ledger.db");
    (SessionSearch::open(&db).expect("search sidecar"), dir)
}

fn chunk(run_id: &str, id: &str, seq: i64, text: &str) -> SessionChunk {
    SessionChunk {
        chunk_id: id.into(),
        run_id: run_id.into(),
        seq,
        kind: "message".into(),
        text: text.into(),
        ts_ms: 1_700_000_000_000,
    }
}

#[test]
fn search_returns_the_chunks_whose_text_matches() {
    let (s, _dir) = file_search();
    s.index(&chunk(
        "run_1",
        "c1",
        1,
        "the pantry stocks alkaline batteries",
    ))
    .unwrap();
    s.index(&chunk(
        "run_1",
        "c2",
        2,
        "debugging the websocket handshake failure",
    ))
    .unwrap();
    s.index(&chunk(
        "run_1",
        "c3",
        3,
        "a quiet evening with no incidents",
    ))
    .unwrap();

    let hits = s.search("websocket handshake", 10).unwrap();
    assert_eq!(hits.len(), 1, "only the matching chunk should hit");
    assert_eq!(hits[0].chunk.chunk_id, "c2");
    assert_eq!(hits[0].chunk.run_id, "run_1");
    assert_eq!(hits[0].chunk.seq, 2);
    assert_eq!(hits[0].lexical_rank, 0);

    // A term indexed nowhere returns nothing, not an error.
    assert!(s.search("xylophone", 10).unwrap().is_empty());
}

#[test]
fn search_returns_no_ghost_hits_for_deleted_runs() {
    let dir = TempDir::new().expect("temp dir");
    let db = dir.path().join("ledger.db");
    let ledger = Ledger::open(&db).expect("ledger");
    let search = SessionSearch::open(&db).expect("search");
    ledger
        .append(&Event::RunStarted {
            run_id: "run-a".into(),
        })
        .unwrap();
    ledger
        .append(&Event::RunStarted {
            run_id: "run-b".into(),
        })
        .unwrap();
    search
        .index(&chunk(
            "run-a",
            "a1",
            1,
            "deploying the canary to production",
        ))
        .unwrap();
    search
        .index(&chunk(
            "run-b",
            "b1",
            1,
            "canary analysis shows no regression",
        ))
        .unwrap();
    assert_eq!(search.search("canary", 10).unwrap().len(), 2);

    ledger.delete_run("run-a").unwrap();

    let hits = search.search("canary", 10).unwrap();
    assert_eq!(hits.len(), 1, "deleted run must leave no searchable rows");
    assert_eq!(hits[0].chunk.run_id, "run-b");
    // The deleted run's own unique term is gone too.
    assert!(search.search("deploying", 10).unwrap().is_empty());
}

#[test]
fn search_across_runs_returns_only_matching_runs() {
    let (s, _dir) = file_search();
    s.index(&chunk(
        "run-a",
        "a1",
        1,
        "quantum tunneling in the diode model",
    ))
    .unwrap();
    s.index(&chunk(
        "run-b",
        "b1",
        1,
        "nebula photography with the new lens",
    ))
    .unwrap();
    s.index(&chunk(
        "run-c",
        "c1",
        1,
        "quantum error correction thresholds",
    ))
    .unwrap();
    s.index(&chunk("run-b", "b2", 2, "calibrating the nebula timer"))
        .unwrap();

    let hits = s.search("quantum", 10).unwrap();
    assert_eq!(hits.len(), 2);
    let mut runs: Vec<&str> = hits.iter().map(|h| h.chunk.run_id.as_str()).collect();
    runs.sort_unstable();
    assert_eq!(runs, ["run-a", "run-c"]);
    assert!(hits.iter().all(|h| h.chunk.text.contains("quantum")));
}
