//! Scheduler state cannot be silently reset or lost between processes,
//! and startup recovery settles every stuck run (audit COR-07, COR-10).

use pantheon_scheduler::repair_budget::RepairBudget;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// Two holders (the gateway daemon and the TUI) share one budget: each
/// spend is seen by the other, so together they cannot exceed the cap.
#[test]
fn repair_budget_spends_are_shared_across_instances() {
    let dir = tempfile::tempdir().unwrap();
    let now = now_ms();
    let mut a = RepairBudget::open(dir.path()).unwrap();
    let mut b = RepairBudget::open(dir.path()).unwrap();
    assert!(a.try_consume("job", 2, now));
    assert!(b.try_consume("job", 2, now));
    assert!(
        !a.try_consume("job", 2, now),
        "a stale copy must not grant a third repair"
    );
    assert!(!b.try_consume("job", 2, now));
}

/// A corrupt budget file is an error, not an empty budget that the next
/// write would overwrite (which reset the daily cap).
#[test]
fn corrupt_repair_budget_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("repair-budget.json");
    std::fs::write(&file, b"{ not json").unwrap();
    let opened = RepairBudget::open(dir.path());
    assert!(opened.is_err(), "corrupt budget must not open as empty");
    assert_eq!(
        std::fs::read(&file).unwrap(),
        b"{ not json",
        "evidence must survive"
    );
}

/// One run that regained its lease between the scan and the settle must
/// not abort recovery of the others.
#[test]
fn startup_recovery_skips_a_live_run_and_settles_the_rest() {
    use pantheon_api::events::Event;
    use pantheon_storage::{Ledger, RunLeaseStore};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.db");
    let ledger = Ledger::open(&path).unwrap();
    for run in ["run-a", "run-live", "run-c"] {
        ledger
            .append(&Event::RunStarted { run_id: run.into() })
            .unwrap();
    }
    let leases = RunLeaseStore::open(&path).unwrap();
    let _held = leases.acquire("run-live", "holder-1", 60_000);
    let settled = ledger.settle_expired_runs().expect("sweep must not abort");
    assert!(!settled.contains(&"run-live".to_string()), "{settled:?}");
    assert!(
        settled.contains(&"run-a".to_string()) && settled.contains(&"run-c".to_string()),
        "{settled:?}"
    );
}
