//! Tests for `pantheon_storage::leases::tests` — sibling file so sources stay test-free.
use super::*;

#[test]
fn only_one_live_owner_and_expiry_takeover() {
    let s = RunLeaseStore::open_in_memory().unwrap();
    assert!(s.acquire("r", "a", 60_000).unwrap().is_some());
    assert!(s.acquire("r", "b", 60_000).unwrap().is_none());
    assert!(s.renew("r", "a", 60_000).is_ok());
    assert!(s.renew("r", "b", 60_000).is_err());
    assert!(!s.release("r", "b").unwrap());
    assert!(s.release("r", "a").unwrap());
    assert!(s.acquire("r", "b", 60_000).unwrap().is_some());
}

#[test]
fn cross_connection_acquire_has_one_winner() {
    use std::sync::{Arc, Barrier};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("leases.db");
    let left = Arc::new(RunLeaseStore::open(&path).unwrap());
    let right = Arc::new(RunLeaseStore::open(&path).unwrap());
    let barrier = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();
    for (store, owner) in [(Arc::clone(&left), "left"), (Arc::clone(&right), "right")] {
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            store.acquire("run", owner, 60_000).unwrap()
        }));
    }
    barrier.wait();
    let winners = handles
        .into_iter()
        .map(|h| h.join().unwrap().is_some())
        .filter(|won| *won)
        .count();
    assert_eq!(winners, 1);
}

#[test]
fn assert_owned_reports_lost_lease() {
    let s = RunLeaseStore::open_in_memory().unwrap();
    s.acquire("r", "a", 60_000).unwrap();
    let e = s.assert_owned("r", "b").unwrap_err();
    assert_eq!(e.run_id, "r");
}
