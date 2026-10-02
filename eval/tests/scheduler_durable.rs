//! Behavioral tests for the durable claim ledger: SQLite (in-memory and
//! on-disk) claim/replay/release/prune semantics. Moved here from
//! `pantheon-scheduler/src/durable_tests.rs`; runs under
//! `cargo test -p pantheon-eval`, not beside the code.
use pantheon_scheduler::{cron::CronSchedule, DurableClaimLedger, Job, ScheduleKind};
use pantheon_storage::ClaimStore;
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
    // Restart: no hydrate needed - the store answers directly.
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
    // "Run ends" - nothing releases the claim.
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

    let key = pantheon_scheduler::occurrence_key(&job, &now.to_string());
    assert!(ledger.claim(&key).unwrap());
    assert!(
        !ledger.claim(&key).unwrap(),
        "restart inside the same minute must not run twice"
    );
}
