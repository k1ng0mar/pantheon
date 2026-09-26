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
#[path = "idempotency_tests.rs"]
mod tests;
