//! Tests for `pantheon_scheduler::idempotency::tests` — sibling file so sources stay test-free.
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
