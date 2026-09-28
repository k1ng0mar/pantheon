//! Automatic storage retention: the pruning primitives honor the policy.
//!
//! Old events of finished runs are pruned at the 90-day policy cutoff;
//! events of open runs (running, awaiting approval) are never pruned,
//! recent events always survive, and finished runs keep their status rows.
//! Behavioral / integration tests per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval`.
use pantheon_api::events::Event;
use pantheon_scheduler::DurableClaimLedger;
use pantheon_storage::ledger::Ledger;
use pantheon_storage::search::{SessionChunk, SessionSearch};
use rusqlite::Connection;
use tempfile::tempdir;

const DAY_MS: i64 = 86_400_000;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The retention policy cutoff: keep 90 days of history.
fn cutoff_90d() -> i64 {
    now_ms() - 90 * DAY_MS
}

fn backdate_events(db: &std::path::Path, run_id: &str, ts_ms: i64) {
    let conn = Connection::open(db).unwrap();
    conn.execute(
        "UPDATE events SET ts_ms = ?1 WHERE run_id = ?2",
        rusqlite::params![ts_ms, run_id],
    )
    .unwrap();
}

fn append_completed_run(ledger: &Ledger, run: &str) {
    ledger
        .append(&Event::RunStarted {
            run_id: run.into(),
        })
        .unwrap();
    ledger
        .append(&Event::TurnStarted {
            run_id: run.into(),
            turn_id: "t1".into(),
        })
        .unwrap();
    ledger
        .append(&Event::TurnCompleted {
            run_id: run.into(),
            turn_id: "t1".into(),
            outcome: "ok".into(),
        })
        .unwrap();
    ledger
        .append(&Event::RunCompleted {
            run_id: run.into(),
        })
        .unwrap();
}

#[test]
fn old_finished_run_events_are_pruned_but_status_row_survives() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("ledger.db");
    {
        let ledger = Ledger::open(&db).unwrap();
        append_completed_run(&ledger, "old");
        append_completed_run(&ledger, "new");
    }
    backdate_events(&db, "old", now_ms() - 100 * DAY_MS);

    let ledger = Ledger::open(&db).unwrap();
    let pruned = ledger.prune_events_before_active_safe(cutoff_90d()).unwrap();
    assert_eq!(pruned, 4, "the old run's four events");

    assert!(ledger.replay("old").unwrap().is_empty());
    // "keep titles and outcomes, drop transcripts": the runs row survives.
    assert_eq!(ledger.status("old").unwrap().as_deref(), Some("completed"));
    // The recent finished run is untouched.
    assert_eq!(ledger.replay("new").unwrap().len(), 4);
}

#[test]
fn active_run_events_survive_despite_age() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("ledger.db");
    {
        let ledger = Ledger::open(&db).unwrap();
        ledger
            .append(&Event::RunStarted {
                run_id: "active".into(),
            })
            .unwrap();
        ledger
            .append(&Event::TurnStarted {
                run_id: "active".into(),
                turn_id: "t1".into(),
            })
            .unwrap();
    }
    backdate_events(&db, "active", now_ms() - 100 * DAY_MS);

    let ledger = Ledger::open(&db).unwrap();
    let pruned = ledger.prune_events_before_active_safe(cutoff_90d()).unwrap();
    assert_eq!(pruned, 0, "an open run is never pruned");
    assert_eq!(ledger.replay("active").unwrap().len(), 2);
}

#[test]
fn awaiting_approval_run_survives() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("ledger.db");
    {
        let ledger = Ledger::open(&db).unwrap();
        ledger
            .append(&Event::RunStarted {
                run_id: "appr".into(),
            })
            .unwrap();
        ledger
            .append(&Event::ApprovalRequested {
                run_id: "appr".into(),
                scope: "call_1:shell:rm -rf /".into(),
            })
            .unwrap();
    }
    backdate_events(&db, "appr", now_ms() - 100 * DAY_MS);

    let ledger = Ledger::open(&db).unwrap();
    assert_eq!(
        ledger.status("appr").unwrap().as_deref(),
        Some("awaiting_approval")
    );
    let pruned = ledger.prune_events_before_active_safe(cutoff_90d()).unwrap();
    assert_eq!(pruned, 0, "a run parked on approval is never pruned");
    assert_eq!(ledger.replay("appr").unwrap().len(), 2);
}

#[test]
fn search_sidecar_is_pruned_in_step_with_the_ledger() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("ledger.db");
    {
        let search = SessionSearch::open(&db).unwrap();
        search
            .index(&SessionChunk {
                chunk_id: "old-chunk".into(),
                run_id: "old".into(),
                seq: 1,
                kind: "message".into(),
                text: "ancient wisdom about pruning".into(),
                ts_ms: now_ms() - 100 * DAY_MS,
            })
            .unwrap();
        search
            .index(&SessionChunk {
                chunk_id: "new-chunk".into(),
                run_id: "new".into(),
                seq: 1,
                kind: "message".into(),
                text: "fresh notes about pruning".into(),
                ts_ms: now_ms(),
            })
            .unwrap();
    }

    let search = SessionSearch::open(&db).unwrap();
    let pruned = search.prune_before(cutoff_90d()).unwrap();
    assert_eq!(pruned, 1);
    assert!(search.search("ancient wisdom", 10).unwrap().is_empty());
    assert_eq!(search.search("fresh notes", 10).unwrap().len(), 1);
}

#[test]
fn old_claims_are_pruned_per_policy() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("claims.db");
    {
        let ledger = DurableClaimLedger::open(&db).unwrap();
        assert!(ledger.claim("old-key").unwrap());
        assert!(ledger.claim("new-key").unwrap());
    }
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE claims SET ts_ms = ?1 WHERE key = 'old-key'",
            rusqlite::params![now_ms() - 100 * DAY_MS],
        )
        .unwrap();
    }

    let ledger = DurableClaimLedger::open(&db).unwrap();
    let pruned = ledger.prune_before(cutoff_90d()).unwrap();
    assert_eq!(pruned, 1);
    assert!(!ledger.is_claimed("old-key").unwrap());
    assert!(ledger.is_claimed("new-key").unwrap());
}
