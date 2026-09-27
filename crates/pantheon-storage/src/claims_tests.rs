//! Tests for `pantheon_storage::claims::tests` — sibling file so sources stay test-free.
use super::*;
use tempfile::tempdir;

#[test]
fn first_claim_wins_and_replay_is_rejected() {
    let store = ClaimStore::open_in_memory().unwrap();
    assert!(store.claim("job:nightly:1789914600000").unwrap());
    assert!(!store.claim("job:nightly:1789914600000").unwrap());
    assert!(store.is_claimed("job:nightly:1789914600000").unwrap());
    assert_eq!(store.len().unwrap(), 1);
}

#[test]
fn distinct_occurrences_are_distinct_claims() {
    let store = ClaimStore::open_in_memory().unwrap();
    assert!(store.claim("job:a:1").unwrap());
    assert!(store.claim("job:a:2").unwrap());
    assert!(store.claim("job:b:1").unwrap());
    assert_eq!(store.len().unwrap(), 3);
}

#[test]
fn release_makes_a_key_claimable_again() {
    let store = ClaimStore::open_in_memory().unwrap();
    assert!(store.claim("k").unwrap());
    assert!(store.release("k").unwrap());
    assert!(!store.release("k").unwrap(), "second release is a no-op");
    assert!(
        store.claim("k").unwrap(),
        "released keys are claimable again"
    );
    assert_eq!(store.len().unwrap(), 1);
}

#[test]
fn claims_survive_a_reopen_like_a_crash_restart() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("claims.sqlite");
    {
        let store = ClaimStore::open(&path).unwrap();
        assert!(store.claim("job:nightly:100").unwrap());
    }
    // New handle over the same file: what was claimed stays claimed.
    let store = ClaimStore::open(&path).unwrap();
    assert!(store.is_claimed("job:nightly:100").unwrap());
    assert!(!store.claim("job:nightly:100").unwrap());
    assert_eq!(store.names().unwrap(), vec!["job:nightly:100"]);
}

#[test]
fn names_are_sorted() {
    let store = ClaimStore::open_in_memory().unwrap();
    store.claim("b").unwrap();
    store.claim("a").unwrap();
    store.claim("c").unwrap();
    assert_eq!(store.names().unwrap(), vec!["a", "b", "c"]);
}

#[test]
fn prune_before_bounds_growth_and_keeps_recent() {
    let store = ClaimStore::open_in_memory().unwrap();
    let now = now_ms();
    store.claim("fresh").unwrap();
    store.claim("newer").unwrap();
    // A cutoff in the past keeps everything: recent claims survive.
    assert_eq!(store.prune_before(now - 1000).unwrap(), 0);
    assert_eq!(store.len().unwrap(), 2);
    // A cutoff ahead of now evicts everything claimed before it.
    assert_eq!(store.prune_before(now + 1_000).unwrap(), 2);
    assert_eq!(store.len().unwrap(), 0);
    assert_eq!(store.names().unwrap(), Vec::<String>::new());
}

#[test]
fn claim_store_open_enables_wal_journal_mode() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("claims.sqlite");
    let store = ClaimStore::open(&path).unwrap();
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
}

#[test]
fn claim_once_is_shared_between_claim_store_and_ledger_claim() {
    use crate::Ledger;
    let dir = tempdir().unwrap();
    let path = dir.path().join("shared.sqlite");
    let ledger = Ledger::open(&path).unwrap();
    let store = ClaimStore::open(&path).unwrap();
    // Claim through the Ledger API: the ClaimStore API must see it.
    assert!(ledger.claim("job:x:1").unwrap());
    assert!(
        !store.claim("job:x:1").unwrap(),
        "Ledger claim must block a ClaimStore replay"
    );
    assert!(store.is_claimed("job:x:1").unwrap());
    // And the other direction.
    assert!(store.claim("job:y:2").unwrap());
    assert!(
        !ledger.claim("job:y:2").unwrap(),
        "ClaimStore claim must block a Ledger replay"
    );
    assert_eq!(store.len().unwrap(), 2);
    // Release is visible to both APIs too.
    assert!(store.release("job:x:1").unwrap());
    assert!(ledger.claim("job:x:1").unwrap());
}

#[test]
fn legacy_occurrence_claims_table_migrates_into_shared_claims() {
    use crate::Ledger;
    let dir = tempdir().unwrap();
    let path = dir.path().join("legacy.sqlite");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE occurrence_claims (key TEXT PRIMARY KEY, claimed_ms INTEGER NOT NULL); \
             INSERT INTO occurrence_claims (key, claimed_ms) VALUES ('job:old:1', 111), ('job:old:2', 222);",
        )
        .unwrap();
    }
    let store = ClaimStore::open(&path).unwrap();
    assert!(store.is_claimed("job:old:1").unwrap());
    assert!(store.is_claimed("job:old:2").unwrap());
    assert_eq!(store.len().unwrap(), 2);
    // The legacy table is gone after migration.
    let conn = rusqlite::Connection::open(&path).unwrap();
    let legacy: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'occurrence_claims'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(legacy, 0);
    drop(conn);
    drop(store);
    // And the migrated rows are claim-visible through the Ledger API.
    let ledger = Ledger::open(&path).unwrap();
    assert!(!ledger.claim("job:old:1").unwrap());
    assert!(!ledger.claim("job:old:2").unwrap());
}
