//! Crash-safe claim ledger (§21).
//!
//! The pure [`ClaimLedger`] decides whether an occurrence may run, but its
//! memory is lost on restart. [`DurableClaimLedger`] makes the same decision
//! from `pantheon-storage`'s `ClaimStore`, which is the single source of
//! truth: every claim is written with an atomic first-wins INSERT before it
//! is honoured, so a crash mid-run cannot cause a fired job to fire again,
//! and a recovered process needs no separate rebuild step — it just asks the
//! store whether the occurrence was already claimed.

use pantheon_core::error::PantheonError;
use pantheon_storage::ClaimStore;

/// A claim ledger whose claims survive restarts.
///
/// No in-memory copy: the store is the truth, so there is nothing to desync
/// and nothing to hydrate after a restart.
#[derive(Debug)]
pub struct DurableClaimLedger {
    store: ClaimStore,
}

impl DurableClaimLedger {
    /// Open the durable ledger on a fresh `ClaimStore`.
    pub fn new(store: ClaimStore) -> Self {
        Self { store }
    }

    /// Open (creating if missing) on a storage path.
    pub fn open(path: &std::path::Path) -> Result<Self, PantheonError> {
        Ok(Self::new(ClaimStore::open(path)?))
    }

    /// Claim an occurrence. `Ok(true)` means this call won and the run
    /// should start; `Ok(false)` is a replay; `Err` means the claim could
    /// not be persisted and the run must not start.
    pub fn claim(&self, key: &str) -> Result<bool, PantheonError> {
        self.store.claim(key)
    }

    /// Has this occurrence already been claimed?
    pub fn is_claimed(&self, key: &str) -> Result<bool, PantheonError> {
        self.store.is_claimed(key)
    }

    /// Explicitly reset a claim (admin/migration only).
    ///
    /// The tick loop never releases a claim when a run ends: a finished run
    /// keeps its key so a replayed delivery cannot fire again. Use this only
    /// to deliberately un-block a stuck key; growth is bounded by
    /// [`Self::prune_before`], not by releasing per run.
    pub fn release(&self, key: &str) -> Result<bool, PantheonError> {
        self.store.release(key)
    }

    pub fn prune_before(&self, cutoff_ms: i64) -> Result<usize, PantheonError> {
        self.store.prune_before(cutoff_ms)
    }

    /// Number of occurrences currently claimed.
    pub fn len(&self) -> Result<usize, PantheonError> {
        self.store.len()
    }

    pub fn is_empty(&self) -> Result<bool, PantheonError> {
        self.store.is_empty()
    }

    /// All currently claimed keys, sorted.
    pub fn names(&self) -> Result<Vec<String>, PantheonError> {
        self.store.names()
    }
}

#[cfg(test)]
#[path = "durable_tests.rs"]
mod tests;
