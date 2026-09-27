//! Tests for `pantheon_storage::ledger::tests` — sibling file so sources stay test-free.
use super::*;
use tempfile::tempdir;

#[test]
fn durability_pragmas_set_busy_timeout_and_synchronous() {
    let conn = Connection::open_in_memory().unwrap();
    crate::configure_durability(&conn, "TEST").unwrap();
    let timeout: i64 = conn
        .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        timeout, 5_000,
        "second process must wait, not hit SQLITE_BUSY"
    );
    let sync: i64 = conn
        .query_row("PRAGMA synchronous", [], |r| r.get(0))
        .unwrap();
    assert_eq!(sync, 1, "1 = NORMAL");
}

#[test]
fn lease_is_live_propagates_storage_errors_instead_of_failing_open() {
    // No run_leases table at all: the old `.ok()` swallow reported Ok(false)
    // ("lease dead"), which let settle_stuck_run repair a possibly-live run.
    let conn = Connection::open_in_memory().unwrap();
    let err = lease_is_live(&conn, "run_x").unwrap_err();
    assert_eq!(err.code, "LEDGER_LEASE_CHECK");
}

#[test]
fn lease_is_live_reports_false_when_no_lease_row_exists() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE run_leases (
           run_id TEXT PRIMARY KEY,
           lease_id TEXT NOT NULL,
           lease_until_ms INTEGER NOT NULL,
           heartbeat_ms INTEGER NOT NULL
         );",
    )
    .unwrap();
    assert!(!lease_is_live(&conn, "run_x").unwrap());
}
