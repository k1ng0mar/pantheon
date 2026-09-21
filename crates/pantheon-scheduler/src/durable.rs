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

    /// All currently claimed keys, sorted.
    pub fn names(&self) -> Result<Vec<String>, PantheonError> {
        self.store.names()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron::CronSchedule;
    use crate::{Job, ScheduleKind};
    use tempfile::tempdir;

    fn job() -> Job {
        Job::new(
            "nightly",
            ScheduleKind::Cron {
                expr: "30 14 * * *".into(),
            },
            "nyx",
        )
    }

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
    }

    #[test]
    fn first_claim_wins_and_replay_is_rejected() {
        let ledger = DurableClaimLedger::new(ClaimStore::open_in_memory().unwrap());
        let key = "job:nightly:1789914600000";
        assert!(ledger.claim(key).unwrap(), "first fire must run");
        assert!(!ledger.claim(key).unwrap(), "replay must be skipped");
        assert!(ledger.is_claimed(key).unwrap());
        assert_eq!(ledger.len().unwrap(), 1);
    }

    #[test]
    fn a_claim_survives_a_crash_restart_without_any_rebuild() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");

        let key = "job:nightly:1789914600000";
        {
            let ledger = DurableClaimLedger::open(&path).unwrap();
            assert!(ledger.claim(key).unwrap());
            // Process dies here, mid-run.
        }
        // Restart: no hydrate needed — the store answers directly.
        let ledger = DurableClaimLedger::open(&path).unwrap();
        assert!(ledger.is_claimed(key).unwrap());
        assert!(!ledger.claim(key).unwrap());
        assert_eq!(ledger.len().unwrap(), 1);
    }

    #[test]
    fn release_is_durable_and_reclaims() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("ledger.sqlite");
        let ledger = DurableClaimLedger::open(&path).unwrap();
        ledger.claim("k").unwrap();
        assert!(ledger.release("k").unwrap());
        assert!(!ledger.release("k").unwrap());
        assert_eq!(ledger.len().unwrap(), 0);

        let ledger = DurableClaimLedger::open(&path).unwrap();
        assert_eq!(ledger.len().unwrap(), 0, "a released claim stays released");
    }

    #[test]
    fn the_tick_loop_never_releases_so_replays_keep_collapsing() {
        let ledger = DurableClaimLedger::new(ClaimStore::open_in_memory().unwrap());
        // A webhook sender delivers, gets no 2xx, retries the same request id.
        assert!(ledger.claim("job:hook:req-42").unwrap());
        // "Run ends" — nothing releases the claim.
        assert!(ledger.is_claimed("job:hook:req-42").unwrap());
        // The retry still collapses onto the already-run occurrence.
        assert!(!ledger.claim("job:hook:req-42").unwrap());
    }

    #[test]
    fn prune_before_bounds_growth_without_opening_replays() {
        let ledger = DurableClaimLedger::new(ClaimStore::open_in_memory().unwrap());
        ledger.claim("job:cron:2026-09-20T13:00Z").unwrap();
        ledger.claim("job:cron:2026-09-20T14:00Z").unwrap();
        let now = now_ms();
        // A past cutoff prunes nothing if everything was claimed at "now".
        assert_eq!(ledger.prune_before(now - 1000).unwrap(), 0);
        assert_eq!(ledger.len().unwrap(), 2);
        // A cutoff comfortably ahead of now evicts everything claimed before it.
        assert_eq!(ledger.prune_before(now + 5000).unwrap(), 2);
        assert_eq!(ledger.len().unwrap(), 0);
    }

    #[test]
    fn claim_agrees_with_the_tick_loop_contract() {
        let ledger = DurableClaimLedger::new(ClaimStore::open_in_memory().unwrap());
        let job = job();
        let now: i64 = 1_789_914_600_000; // 2026-09-20T14:30:00Z
        assert!(job.due(now, None));
        assert!(CronSchedule::parse("30 14 * * *").unwrap().matches_ms(now));

        let key = crate::idempotency::occurrence_key(&job, &now.to_string());
        assert!(ledger.claim(&key).unwrap());
        assert!(
            !ledger.claim(&key).unwrap(),
            "restart inside the same minute must not run twice"
        );
    }
}
