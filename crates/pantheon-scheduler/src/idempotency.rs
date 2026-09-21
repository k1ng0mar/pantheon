//! Idempotency and missed-run policy for durable jobs (§21).
//!
//! A durable scheduler fires, crashes, and recovers, so every fire carries a
//! key derived from (job, occurrence). Replaying the same occurrence resumes
//! or re-uses the existing run instead of starting a second one. The durable
//! store lives in `pantheon-storage`; the decision logic lives here so it can
//! be tested without a database.

use crate::{Job, MissedPolicy};
use std::collections::HashSet;

/// Key for one occurrence of a job.
///
/// `occurrence` must identify the fire instant (an ISO/millis stamp for cron
/// and interval jobs, the trigger id for webhooks) so that two fires inside
/// the same occurrence collapse to one key.
pub fn occurrence_key(job: &Job, occurrence: &str) -> String {
    format!("{}{}", job.idempotency_key, occurrence)
}

/// Which occurrences have already been claimed.
///
/// In-memory on purpose: the caller persists keys in the ledger. A
/// recovered process rebuilds this from the ledger before the first tick.
#[derive(Debug, Default)]
pub struct ClaimLedger {
    claimed: HashSet<String>,
}

impl ClaimLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim an occurrence. `true` means this is the first claim and the run
    /// should start; `false` means it is a replay and must not run twice.
    pub fn claim(&mut self, key: &str) -> bool {
        self.claimed.insert(key.to_string())
    }

    /// Has this occurrence already had a run started?
    pub fn is_claimed(&self, key: &str) -> bool {
        self.claimed.contains(key)
    }

    /// Release a claim after a run ends, so the key is not held forever by
    /// long-lived processes.
    pub fn release(&mut self, key: &str) -> bool {
        self.claimed.remove(key)
    }

    pub fn len(&self) -> usize {
        self.claimed.len()
    }

    pub fn is_empty(&self) -> bool {
        self.claimed.is_empty()
    }
}

/// How many runs a missed window should enqueue.
///
/// `missed_occurrences` counts occurrences the job slept through (e.g. the
/// host was off for six hours of an hourly job).
pub fn runs_for_missed(policy: MissedPolicy, missed_occurrences: usize) -> usize {
    match policy {
        MissedPolicy::Skip => 0,
        // One catch-up run, no matter how long the gap was.
        MissedPolicy::RunOnce => missed_occurrences.min(1),
        // Every missed occurrence: only safe for idempotent work.
        MissedPolicy::CatchUp => missed_occurrences,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cron::CronSchedule;
    use crate::ScheduleKind;

    fn job() -> Job {
        Job::new(
            "nightly",
            ScheduleKind::Cron {
                expr: "30 14 * * *".into(),
            },
            "nyx",
        )
    }

    #[test]
    fn a_replay_of_the_same_occurrence_does_not_run_twice() {
        let job = job();
        let mut ledger = ClaimLedger::new();
        let key = occurrence_key(&job, "1789914600000");

        assert!(ledger.claim(&key), "first fire must run");
        assert!(
            !ledger.claim(&key),
            "replay of the same fire must be skipped"
        );
        assert!(ledger.is_claimed(&key));
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn distinct_occurrences_each_run() {
        let job = job();
        let mut ledger = ClaimLedger::new();
        assert!(ledger.claim(&occurrence_key(&job, "1789914600000")));
        assert!(ledger.claim(&occurrence_key(&job, "1789914660000")));
        assert_eq!(ledger.len(), 2);
    }

    #[test]
    fn keys_are_namespaced_per_job() {
        let a = Job::new("a", ScheduleKind::Manual, "nyx");
        let b = Job::new("b", ScheduleKind::Manual, "nyx");
        assert_ne!(
            occurrence_key(&a, "1"),
            occurrence_key(&b, "1"),
            "two jobs firing at the same instant are different runs"
        );
        assert_eq!(occurrence_key(&a, "1"), "job:a:1");
    }

    #[test]
    fn a_claim_can_be_released_when_a_run_ends() {
        let mut ledger = ClaimLedger::new();
        assert!(ledger.claim("k"));
        assert!(ledger.release("k"));
        assert!(!ledger.release("k"), "second release is a no-op");
        assert!(ledger.is_empty());
    }

    #[test]
    fn missed_run_policy_decides_the_catch_up_count() {
        assert_eq!(runs_for_missed(MissedPolicy::Skip, 6), 0);
        assert_eq!(runs_for_missed(MissedPolicy::RunOnce, 6), 1);
        assert_eq!(runs_for_missed(MissedPolicy::RunOnce, 0), 0);
        assert_eq!(runs_for_missed(MissedPolicy::CatchUp, 6), 6);
    }

    #[test]
    fn crash_and_recover_inside_a_minute_fires_once() {
        // The full path: cron says due, the ledger says which occurrence.
        let job = job();
        let now: i64 = 1_789_914_600_000; // 2026-09-20T14:30:00Z
        assert!(job.due(now, None));
        assert!(CronSchedule::parse("30 14 * * *").unwrap().matches_ms(now));

        let mut ledger = ClaimLedger::new();
        let key = occurrence_key(&job, &now.to_string());

        assert!(ledger.claim(&key), "the fire runs");
        // Process dies mid-run and restarts a second later in the same minute.
        assert!(
            !ledger.claim(&key),
            "the recovery replay must not run again"
        );
    }
}
