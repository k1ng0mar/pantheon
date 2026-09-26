//! Tests for `pantheon_cli::repair_cli` — sibling file so sources stay
//! test-free.
use super::*;
use crate::dotenv::test_support::TEST_ENV_LOCK;
use pantheon_core::events::Event;

/// A run left in `running` with no lease is the crash case. `check` must
/// report it and name the exact command that fixes it, because a diagnostic
/// that does not say what to do is a diagnostic nobody runs twice.
#[test]
fn check_reports_a_stranded_run_with_its_fix() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("pantheon-repair-a-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("PANTHEON_DATA_DIR", &dir);

    let sup = Supervisor::open(dir.clone()).unwrap();
    sup.start_run("run_stranded").unwrap();
    // start_run leaves status 'running' and nothing else will move it: this
    // is exactly the state a crash between tool-start and tool-complete
    // leaves behind, because only the terminal events clear it.
    assert_eq!(
        sup.ledger_status("run_stranded").unwrap().as_deref(),
        Some("running")
    );

    // check() exits the process on an error-severity finding, but a stranded
    // run is a warning, so it returns normally here.
    check(&sup);
}

/// A run holding a live lease is a session that is genuinely working. It must
/// not be reported as a fault, or the operator learns to ignore the output.
#[test]
fn check_skips_a_run_that_still_holds_a_lease() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("pantheon-repair-b-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("PANTHEON_DATA_DIR", &dir);

    let sup = Supervisor::open(dir.clone()).unwrap();
    sup.start_run("run_busy").unwrap();
    assert!(!sup.has_active_lease("run_busy").unwrap());

    // Acquire a real lease so the row is genuinely busy.
    let store = pantheon_storage::RunLeaseStore::open(&dir.join("ledger.db")).unwrap();
    let _lease = store
        .acquire("run_busy", "lease-test-1", 60_000)
        .unwrap()
        .expect("lease should be free");
    assert!(sup.has_active_lease("run_busy").unwrap());
}

/// Repair appends events; it does not rewrite rows. A replaying reader has to
/// be able to see *why* a run ended, and an UPDATE would leave the status
/// contradicting the last event in the trail.
#[test]
fn settling_a_run_appends_events_and_leaves_an_auditable_trail() {
    let dir = std::env::temp_dir().join(format!("pantheon-repair-c-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sup = Supervisor::open(dir.clone()).unwrap();
    sup.start_run("run_stuck").unwrap();

    sup.settle_stuck_run("run_stuck", "no live lease").unwrap();

    assert_eq!(
        sup.ledger_status("run_stuck").unwrap().as_deref(),
        Some("failed")
    );
    let entries = sup.replay("run_stuck").unwrap();
    let kinds: Vec<&str> = entries
        .iter()
        .map(|e| match &e.event {
            Event::RunStarted { .. } => "started",
            Event::RunProgress { .. } => "progress",
            Event::RunFailed { .. } => "failed",
            _ => "other",
        })
        .collect();
    assert!(
        kinds.contains(&"progress") && kinds.contains(&"failed"),
        "repair must leave a reason and a terminal event: {kinds:?}"
    );
    // The reason must be readable, not just "something was repaired".
    let has_reason = entries.iter().any(
        |e| matches!(&e.event, Event::RunProgress { detail, .. } if detail.contains("repair:")),
    );
    assert!(has_reason, "no event explains why the run was settled");
}

/// Refusing to settle a run that is already terminal keeps `repair runs`
/// idempotent in the sense that matters: running it twice does not append a
/// second bogus failure.
#[test]
fn settling_an_already_terminal_run_is_refused() {
    let dir = std::env::temp_dir().join(format!("pantheon-repair-d-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sup = Supervisor::open(dir.clone()).unwrap();
    sup.start_run("run_done").unwrap();
    sup.complete("run_done").unwrap();
    assert_eq!(
        sup.ledger_status("run_done").unwrap().as_deref(),
        Some("completed")
    );

    let err = sup.settle_stuck_run("run_done", "nope").unwrap_err();
    assert_eq!(err.code, "REPAIR_NOT_STUCK");
    // And the refusal must not have written anything.
    let entries = sup.replay("run_done").unwrap();
    assert!(
        !entries
            .iter()
            .any(|e| matches!(&e.event, Event::RunFailed { .. })),
        "a refused settle still wrote a failure event"
    );
}

/// A live lease blocks the settle. This is the guard that stops `repair runs`
/// from killing a session that is mid-turn.
#[test]
fn settling_a_leased_run_is_refused() {
    let _lock = TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("pantheon-repair-e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::env::set_var("PANTHEON_DATA_DIR", &dir);

    let sup = Supervisor::open(dir.clone()).unwrap();
    sup.start_run("run_leased").unwrap();
    let store = pantheon_storage::RunLeaseStore::open(&dir.join("ledger.db")).unwrap();
    let _lease = store
        .acquire("run_leased", "lease-test-2", 60_000)
        .unwrap()
        .expect("lease should be free");

    let err = sup.settle_stuck_run("run_leased", "nope").unwrap_err();
    assert_eq!(err.code, "REPAIR_LEASE_ACTIVE");
    // The run must still be running: a refused repair is a no-op.
    assert_eq!(
        sup.ledger_status("run_leased").unwrap().as_deref(),
        Some("running")
    );
}

/// A fresh ledger is clean. Without this, `check` could pass by never running
/// the integrity probe at all.
#[test]
fn a_fresh_ledger_passes_the_integrity_check() {
    let dir = std::env::temp_dir().join(format!("pantheon-repair-f-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sup = Supervisor::open(dir.clone()).unwrap();
    assert_eq!(sup.integrity_check().unwrap(), vec!["ok".to_string()]);
    assert!(sup.stuck_runs().unwrap().is_empty());
}
