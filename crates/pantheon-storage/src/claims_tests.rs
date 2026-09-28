//! Tests for `pantheon_storage::claims::tests` — sibling file so sources stay test-free.
use super::*;

#[test]
fn first_claim_wins_and_replay_is_rejected() {
    let store = ClaimStore::open_in_memory().unwrap();
    assert!(store.claim("job:nightly:1789914600000").unwrap());
    assert!(!store.claim("job:nightly:1789914600000").unwrap());
    assert!(store.is_claimed("job:nightly:1789914600000").unwrap());
    assert_eq!(store.len().unwrap(), 1);
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
