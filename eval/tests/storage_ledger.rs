//! Behavioral / integration tests moved out of the crate per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval`.
use pantheon_api::events::Event;
use pantheon_storage::{Ledger, RunLeaseStore};
use rusqlite::Connection;
use tempfile::tempdir;

#[test]
fn artifacts_are_stored_in_the_ledger_database() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .put_artifact("task-1", "image/png", &[1, 2, 3])
        .unwrap();
    let a = ledger.artifact("task-1").unwrap().unwrap();
    assert_eq!(a.mime, "image/png");
    assert_eq!(a.bytes, vec![1, 2, 3]);
    assert!(ledger.artifact("task-2").unwrap().is_none());
    assert_eq!(
        ledger
            .put_artifact("bad", "text/plain\r\nX-Test: yes", b"x")
            .unwrap_err()
            .code,
        "ARTIFACT_MIME"
    );
    assert_eq!(
        ledger
            .put_artifact("bad/id", "text/plain", b"x")
            .unwrap_err()
            .code,
        "ARTIFACT_ID"
    );
}

#[test]
fn terminal_run_status_cannot_be_overwritten() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .append(&Event::RunStarted { run_id: "r".into() })
        .unwrap();
    ledger
        .append(&Event::RunCanceled {
            run_id: "r".into(),
            reason: "stop".into(),
        })
        .unwrap();
    ledger
        .append(&Event::RunFailed {
            run_id: "r".into(),
            code: "late".into(),
        })
        .unwrap();
    assert_eq!(ledger.status("r").unwrap().as_deref(), Some("canceled"));
}

#[test]
fn round_trip_and_render_log() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .append(&Event::RunStarted {
            run_id: "r1".into(),
        })
        .unwrap();
    ledger
        .append(&Event::ToolStarted {
            run_id: "r1".into(),
            call_id: "call_0_0".into(),
            tool: "shell".into(),
            args: String::new(),
            provenance: pantheon_api::provenance::Provenance::system("test"),
        })
        .unwrap();
    ledger
        .append(&Event::RunCompleted {
            run_id: "r1".into(),
        })
        .unwrap();
    assert_eq!(ledger.replay("r1").unwrap().len(), 3);
    assert!(ledger.render_run_log("r1").unwrap().contains("completed"));
    assert_eq!(ledger.status("r1").unwrap().as_deref(), Some("completed"));
}

#[test]
fn context_trimmed_round_trips_and_explains() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .append(&Event::RunStarted {
            run_id: "r1".into(),
        })
        .unwrap();
    ledger
        .append(&Event::ContextTrimmed {
            run_id: "r1".into(),
            estimated: 11_000,
            window: 16_000,
            dropped_rows: 4,
            compacted_rows: 1,
        })
        .unwrap();
    // run_id_of must attribute it to the run: replay finds it.
    let entries = ledger.replay("r1").unwrap();
    assert_eq!(entries.len(), 2);
    assert!(matches!(
        entries[1].event,
        Event::ContextTrimmed {
            dropped_rows: 4,
            ..
        }
    ));
    let explain = ledger.render_run_log("r1").unwrap();
    assert!(explain.contains("context trimmed"), "explain: {explain}");
}

#[test]
fn context_compressed_round_trips_and_explains() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .append(&Event::RunStarted {
            run_id: "r1".into(),
        })
        .unwrap();
    ledger
        .append(&Event::ContextCompressed {
            run_id: "r1".into(),
            model: "summarizer".into(),
            exchanges: 2,
            rows: 4,
            chars_before: 20_000,
            chars_after: 400,
        })
        .unwrap();
    let entries = ledger.replay("r1").unwrap();
    assert_eq!(entries.len(), 2);
    assert!(matches!(
        entries[1].event,
        Event::ContextCompressed { exchanges: 2, .. }
    ));
    let explain = ledger.render_run_log("r1").unwrap();
    assert!(
        explain.contains("context compressed by summarizer"),
        "explain: {explain}"
    );
}

#[test]
fn session_titled_drives_the_run_title_and_last_write_wins() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .append(&Event::RunStarted {
            run_id: "r1".into(),
        })
        .unwrap();
    // Untitled until a title event lands.
    assert_eq!(ledger.run_title("r1").unwrap(), None);
    ledger
        .append(&Event::SessionTitled {
            run_id: "r1".into(),
            title: "Fix login bug".into(),
            model: "namer".into(),
            source: "model".into(),
        })
        .unwrap();
    assert_eq!(
        ledger.run_title("r1").unwrap().as_deref(),
        Some("Fix login bug")
    );
    // A newer title (manual rename, second pass) replaces the old one.
    ledger
        .append(&Event::SessionTitled {
            run_id: "r1".into(),
            title: "Renamed by hand".into(),
            model: "user".into(),
            source: "manual".into(),
        })
        .unwrap();
    let runs = ledger.list_runs(10).unwrap();
    assert_eq!(runs[0].0, "r1");
    assert_eq!(runs[0].3.as_deref(), Some("Renamed by hand"));
    assert_eq!(
        ledger.run_title("r1").unwrap().as_deref(),
        Some("Renamed by hand")
    );
    let explain = ledger.render_run_log("r1").unwrap();
    assert!(
        explain.contains("session titled \"Renamed by hand\" (manual by user)"),
        "explain: {explain}"
    );
    // The event itself replays for `pantheon logs` and the audit trail.
    assert!(ledger
        .replay("r1")
        .unwrap()
        .iter()
        .any(|e| matches!(e.event, Event::SessionTitled { .. })));
}

#[test]
fn pre_title_ledger_files_migrate_on_open() {
    let dir = std::env::temp_dir().join(format!(
        "pantheon-ledger-migrate-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("ledger.db");
    // A ledger written before the title column existed.
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE runs (
                   run_id TEXT PRIMARY KEY,
                   created_ms INTEGER NOT NULL,
                   status TEXT NOT NULL DEFAULT 'running'
                 );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO runs (run_id, created_ms, status) VALUES ('old-run', 1, 'completed')",
            [],
        )
        .unwrap();
    }
    let ledger = Ledger::open(&path).unwrap();
    let runs = ledger.list_runs(10).unwrap();
    assert_eq!(runs[0].0, "old-run");
    assert_eq!(runs[0].3, None, "old rows stay untitled, not broken");
    assert_eq!(ledger.run_title("old-run").unwrap(), None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A lease row outlives `kill -9`, because nothing gets to release it and its
/// TTL keeps counting. Treating "unexpired lease" as "a session is working"
/// means `repair check` reports a crashed run as healthy for a full TTL after
/// the crash - precisely when the operator needs it most.
///
/// A lease counts as live only while it is being heartbeated. This is a
/// regression test for exactly that: the first implementation of
/// `has_active_lease` checked only `lease_until_ms > now` and reported the
/// corpse below as healthy.

#[test]
fn an_unexpired_lease_with_a_dead_heartbeat_is_not_active() {
    let dir = std::env::temp_dir().join(format!("pantheon-stale-lease-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("ledger.db");
    let ledger = Ledger::open(&db).unwrap();
    let leases = RunLeaseStore::open(&db).unwrap();
    let run = "run_crashed";
    // The run row is what makes it "stuck": a crash after RunStarted leaves
    // status 'running' with no terminal event ever to clear it.
    ledger
        .append(&pantheon_api::events::Event::RunStarted { run_id: run.into() })
        .unwrap();
    assert_eq!(ledger.status(run).unwrap().as_deref(), Some("running"));
    let _held = leases
        .acquire(run, "lease-1", 60_000)
        .unwrap()
        .expect("lease should be free");
    assert!(
        ledger.has_active_lease(run).unwrap(),
        "fresh lease is active"
    );

    // Simulate the crash: the row stays, the heartbeat stops moving.
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE run_leases SET heartbeat_ms = heartbeat_ms - 30000 WHERE run_id = ?1",
            [run],
        )
        .unwrap();

    // The lease is still inside its TTL, so a TTL-only check would call this
    // active. Assert that precondition, or the test cannot fail.
    let (until, beat): (i64, i64) = rusqlite::Connection::open(&db)
        .unwrap()
        .query_row(
            "SELECT lease_until_ms, heartbeat_ms FROM run_leases WHERE run_id = ?1",
            [run],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    assert!(
        until > now,
        "precondition: lease must still be unexpired for this test to bite"
    );
    assert!(
        now - beat > 5_000,
        "precondition: heartbeat must look stale"
    );

    assert!(
        !ledger.has_active_lease(run).unwrap(),
        "a lease nobody is renewing must not count as an active session"
    );
    // Which means the crashed run is repairable rather than invisible.
    assert_eq!(ledger.stuck_runs().unwrap().len(), 1);
}

/// The metrics fold is now the only user-visible replacement for
/// `pantheon-otel`, so it has to be right about the cases that were folded
/// into a single "finished" number before and quietly lost.

#[test]
fn run_metrics_count_each_outcome_separately() {
    let led = Ledger::open_in_memory().unwrap();
    let start = |run: &str| {
        led.append(&Event::RunStarted { run_id: run.into() })
            .unwrap()
    };
    let prov = pantheon_api::provenance::Provenance::system("test");

    start("r_ok");
    led.append(&Event::RunCompleted {
        run_id: "r_ok".into(),
    })
    .unwrap();

    start("r_bad");
    led.append(&Event::RunFailed {
        run_id: "r_bad".into(),
        code: "BOOM".into(),
    })
    .unwrap();

    start("r_give");
    led.append(&Event::RunCanceled {
        run_id: "r_give".into(),
        reason: "user".into(),
    })
    .unwrap();

    for run in ["r_ok", "r_bad", "r_give"] {
        led.append(&Event::ToolStarted {
            run_id: run.into(),
            call_id: "c0".into(),
            tool: "shell".into(),
            args: String::new(),
            provenance: prov.clone(),
        })
        .unwrap();
        led.append(&Event::ApprovalRequested {
            run_id: run.into(),
            scope: "GitPush".into(),
        })
        .unwrap();
    }
    // Only r_ok got its approval, and only r_ok ran a model turn and trimmed.
    led.append(&Event::ApprovalGranted {
        run_id: "r_ok".into(),
        scope: "GitPush".into(),
    })
    .unwrap();
    led.append(&Event::ApprovalDenied {
        run_id: "r_ok".into(),
        scope: "Net".into(),
    })
    .unwrap();
    led.append(&Event::ModelCompleted {
        run_id: "r_ok".into(),
    })
    .unwrap();
    led.append(&Event::ContextTrimmed {
        run_id: "r_ok".into(),
        estimated: 10,
        window: 20,
        dropped_rows: 3,
        compacted_rows: 1,
    })
    .unwrap();
    led.append(&Event::ContextCompressed {
        run_id: "r_ok".into(),
        model: "m".into(),
        exchanges: 2,
        rows: 4,
        chars_before: 100,
        chars_after: 20,
    })
    .unwrap();

    let m = led.metrics("r_ok").unwrap();
    assert_eq!(m.runs_started, 1);
    assert_eq!(m.runs_completed, 1);
    assert_eq!(m.runs_failed, 0, "a completed run must not count as failed");
    assert_eq!(m.runs_canceled, 0);
    assert_eq!(m.tool_calls, 1);
    assert_eq!(m.model_turns, 1);
    assert_eq!(
        (
            m.approvals_requested,
            m.approvals_granted,
            m.approvals_denied
        ),
        (1, 1, 1)
    );
    assert_eq!((m.context_trims, m.context_compressions), (1, 1));

    // The three outcomes stay distinguishable. This is the whole reason the
    // fold exists rather than one "finished" counter.
    assert_eq!(led.metrics("r_bad").unwrap().runs_failed, 1);
    assert_eq!(led.metrics("r_bad").unwrap().runs_completed, 0);
    assert_eq!(led.metrics("r_give").unwrap().runs_canceled, 1);
    assert_eq!(led.metrics("r_give").unwrap().runs_completed, 0);
    // Counters are per-run, not global.
    assert_eq!(led.metrics("r_ok").unwrap().tool_calls, 1);
    assert_eq!(led.metrics("nope").unwrap().runs_started, 0);
}

/// The rendered form has to name the number that matters when someone is
/// scanning it. A missing `failed` count is how a bad run looks fine.

#[test]
fn ledger_open_enables_wal_journal_mode() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let ledger = Ledger::open(&path).unwrap();
    drop(ledger);
    let conn = Connection::open(&path).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
}

#[test]
fn prune_events_before_drops_old_and_keeps_new() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let ledger = Ledger::open(&path).unwrap();
    ledger
        .append(&Event::RunStarted {
            run_id: "r_old".into(),
        })
        .unwrap();
    ledger
        .append(&Event::RunStarted {
            run_id: "r_new".into(),
        })
        .unwrap();
    // Age the first run's event through a second connection; the ledger API
    // itself always stamps `now`, so a mixed-age history needs this.
    {
        let raw = Connection::open(&path).unwrap();
        raw.execute("UPDATE events SET ts_ms = 1 WHERE run_id = 'r_old'", [])
            .unwrap();
    }
    let pruned = ledger.prune_events_before(1_000).unwrap();
    assert_eq!(pruned, 1);
    assert!(
        ledger.replay("r_old").unwrap().is_empty(),
        "old event must be gone"
    );
    assert_eq!(
        ledger.replay("r_new").unwrap().len(),
        1,
        "new event must survive"
    );
    // A cutoff in the past prunes nothing.
    assert_eq!(ledger.prune_events_before(0).unwrap(), 0);
}

#[test]
fn append_returns_the_stored_global_seq_not_the_rowid() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("ledger.sqlite");
    let ledger = Ledger::open(&path).unwrap();
    let a = ledger
        .append(&Event::RunStarted { run_id: "r".into() })
        .unwrap();
    let b = ledger
        .append(&Event::RunStarted { run_id: "r".into() })
        .unwrap();
    assert_eq!((a.seq, b.seq), (1, 2));
    // Delete the NEWEST row: stored seq becomes MAX(seq)+1 = 2 while the
    // AUTOINCREMENT rowid moves on to 4. Returning `seq: id` would report 4.
    {
        let raw = Connection::open(&path).unwrap();
        raw.execute("DELETE FROM events WHERE seq = 2", []).unwrap();
    }
    let c = ledger
        .append(&Event::RunStarted { run_id: "r".into() })
        .unwrap();
    assert_eq!(c.id, 3, "rowid keeps climbing");
    assert_eq!(c.seq, 2, "stored seq is MAX(seq)+1 over remaining rows");
}
