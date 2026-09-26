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
