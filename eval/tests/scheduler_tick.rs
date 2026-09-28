//! Behavioral tests for the scheduler tick driver: barrier-synced threads,
//! timing waits, and an in-memory SQLite claim ledger. Moved here from
//! `pantheon-scheduler/src/tick_tests.rs`; runs under
//! `cargo test -p pantheon-eval`, not beside the code.
use pantheon_scheduler::{
    DurableClaimLedger, Job, OverlapPolicy, RunOutcome, ScheduleKind, TickDecision, TickDriver,
};
use pantheon_storage::ClaimStore;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Barrier;
use std::sync::{mpsc, Arc};
use std::time::Duration;

fn driver() -> Arc<TickDriver> {
    let ledger = DurableClaimLedger::new(ClaimStore::open_in_memory().expect("in-memory claims"));
    Arc::new(TickDriver::new(ledger))
}

fn every_minute(id: &str) -> Job {
    Job::new(
        id,
        ScheduleKind::Cron {
            expr: "* * * * *".into(),
        },
        "nyx",
    )
}

/// 2026-09-20T14:30:00Z — a Sunday.
const NOW: i64 = 1_789_914_600_000;

fn fired_outcome(d: TickDecision) -> mpsc::Receiver<RunOutcome> {
    match d {
        TickDecision::Fired { completion } => completion,
        other => panic!("expected Fired, got {other:?}"),
    }
}

fn wait_for(runs: &AtomicUsize, want: usize) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while runs.load(Ordering::SeqCst) < want {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {want} runs"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn racing_ticks_execute_exactly_once() {
    // Two ticks racing the same due job: exactly one wins the durable
    // claim and runs; the loser skips instead of double-executing.
    let driver = driver();
    let job = every_minute("racer");
    let runs = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(2));

    let handles: Vec<_> = (0..2)
        .map(|_| {
            let (driver, job, runs, barrier) = (
                Arc::clone(&driver),
                job.clone(),
                Arc::clone(&runs),
                Arc::clone(&barrier),
            );
            std::thread::spawn(move || {
                barrier.wait(); // line the racers up
                let r = Arc::clone(&runs);
                driver.tick_job(
                    &job,
                    NOW,
                    None,
                    Arc::new(move || {
                        r.fetch_add(1, Ordering::SeqCst);
                    }),
                )
            })
        })
        .collect();
    let decisions: Vec<TickDecision> = handles
        .into_iter()
        .map(|h| h.join().expect("tick thread"))
        .collect();

    let mut fired = 0;
    let mut skipped = 0;
    let mut completion = None;
    for d in decisions {
        match d {
            TickDecision::Fired { completion: rx } => {
                fired += 1;
                completion = Some(rx);
            }
            // Either layer may win the race: the in-process overlap gate
            // or the durable claim. Both mean "did not run twice".
            TickDecision::SkippedClaimLost | TickDecision::SkippedOverlap => skipped += 1,
            other => panic!("unexpected decision {other:?}"),
        }
    }
    assert_eq!(fired, 1, "exactly one tick must fire");
    assert_eq!(skipped, 1, "the loser must skip");

    let rx = completion.unwrap();
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        RunOutcome::Completed
    );
    // Give the loser every chance to (incorrectly) run anyway.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(runs.load(Ordering::SeqCst), 1, "must not double-execute");
}

#[test]
fn claim_first_wins_across_threads() {
    // The raw guarantee the race test above rests on: concurrent claims
    // for one key agree on exactly one winner.
    let ledger = DurableClaimLedger::new(ClaimStore::open_in_memory().expect("in-memory claims"));
    let ledger = Arc::new(ledger);
    let wins = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(8));
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let (ledger, wins, barrier) =
                (Arc::clone(&ledger), Arc::clone(&wins), Arc::clone(&barrier));
            std::thread::spawn(move || {
                barrier.wait();
                if ledger.claim("job:x:occurrence").unwrap() {
                    wins.fetch_add(1, Ordering::SeqCst);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("claim thread");
    }
    assert_eq!(wins.load(Ordering::SeqCst), 1);
}

#[test]
fn overlap_skip_drops_second_fire_while_running() {
    // Default policy: a tick that finds the job still running skips (and
    // the caller logs the SkippedOverlap decision).
    let driver = driver();
    let job = every_minute("slow");
    let runs = Arc::new(AtomicUsize::new(0));
    let released = Arc::new(AtomicBool::new(false));
    let blocking = || {
        let (runs, released) = (Arc::clone(&runs), Arc::clone(&released));
        Arc::new(move || {
            runs.fetch_add(1, Ordering::SeqCst);
            while !released.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };

    let rx = fired_outcome(driver.tick_job(&job, NOW, None, blocking()));
    wait_for(&runs, 1); // the first run is genuinely in flight now
    match driver.tick_job(&job, NOW, None, blocking()) {
        TickDecision::SkippedOverlap => {}
        other => panic!("expected SkippedOverlap, got {other:?}"),
    }
    released.store(true, Ordering::SeqCst);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        RunOutcome::Completed
    );
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(runs.load(Ordering::SeqCst), 1, "skipped fire must not run");
}

#[test]
fn overlap_replace_supersedes_inflight_run() {
    let driver = driver();
    let mut job = every_minute("repl");
    job.overlap = OverlapPolicy::Replace;
    let runs = Arc::new(AtomicUsize::new(0));
    let released = Arc::new(AtomicBool::new(false));
    let blocking = || {
        let (runs, released) = (Arc::clone(&runs), Arc::clone(&released));
        Arc::new(move || {
            runs.fetch_add(1, Ordering::SeqCst);
            while !released.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };

    let rx_old = fired_outcome(driver.tick_job(&job, NOW, None, blocking()));
    wait_for(&runs, 1);
    // A later tick (new occurrence) with Replace fires fresh...
    let rx_new = fired_outcome(driver.tick_job(&job, NOW + 60_000, None, blocking()));
    released.store(true, Ordering::SeqCst);

    // ...the disowned run reports Replaced, the new one completes.
    assert_eq!(
        rx_old.recv_timeout(Duration::from_secs(5)).unwrap(),
        RunOutcome::Replaced
    );
    assert_eq!(
        rx_new.recv_timeout(Duration::from_secs(5)).unwrap(),
        RunOutcome::Completed
    );
    assert_eq!(runs.load(Ordering::SeqCst), 2);
}

#[test]
fn overlap_queue_runs_once_more_after_inflight() {
    let driver = driver();
    let mut job = every_minute("queued");
    job.overlap = OverlapPolicy::Queue;
    let runs = Arc::new(AtomicUsize::new(0));
    let released = Arc::new(AtomicBool::new(false));
    let blocking = || {
        let (runs, released) = (Arc::clone(&runs), Arc::clone(&released));
        Arc::new(move || {
            runs.fetch_add(1, Ordering::SeqCst);
            while !released.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(5));
            }
        })
    };

    let rx = fired_outcome(driver.tick_job(&job, NOW, None, blocking()));
    wait_for(&runs, 1);
    match driver.tick_job(&job, NOW, None, blocking()) {
        TickDecision::Queued => {}
        other => panic!("expected Queued, got {other:?}"),
    }
    released.store(true, Ordering::SeqCst);

    // One Fired decision, two runs: the initial plus the drained queue.
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        RunOutcome::Completed
    );
    wait_for(&runs, 2);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        RunOutcome::Completed
    );
}

#[test]
fn run_past_timeout_is_abandoned() {
    let driver = driver();
    let mut job = every_minute("hangs");
    job.timeout_secs = Some(1); // short so the test stays fast
    let started = Arc::new(AtomicBool::new(false));
    let s = Arc::clone(&started);
    let rx = fired_outcome(driver.tick_job(
        &job,
        NOW,
        None,
        Arc::new(move || {
            s.store(true, Ordering::SeqCst);
            std::thread::sleep(Duration::from_secs(30)); // detached, abandoned
        }),
    ));
    wait_for_bool(&started);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        RunOutcome::TimedOut,
        "a run past its timeout must be abandoned, not awaited"
    );
    // The slot is released: a later tick may fire again.
    match driver.tick_job(&job, NOW + 60_000, None, Arc::new(|| {})) {
        TickDecision::Fired { .. } => {}
        other => panic!("slot should be free after abandon, got {other:?}"),
    }
}

fn wait_for_bool(flag: &AtomicBool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !flag.load(Ordering::SeqCst) {
        assert!(
            std::time::Instant::now() < deadline,
            "executor never started"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn not_due_never_claims_or_runs() {
    let driver = driver();
    let job = every_minute("idle");
    let ran = Arc::new(AtomicBool::new(false));
    let r = Arc::clone(&ran);
    // last_fire inside the same minute: not due.
    match driver.tick_job(
        &job,
        NOW,
        Some(NOW),
        Arc::new(move || r.store(true, Ordering::SeqCst)),
    ) {
        TickDecision::NotDue => {}
        other => panic!("expected NotDue, got {other:?}"),
    }
    assert!(!ran.load(Ordering::SeqCst));
}

#[test]
fn invalid_cron_never_fires_and_never_claims() {
    // An invalid expression is a registration error (Job::validate); the
    // tick treats it as not-due rather than panicking or claiming.
    let driver = driver();
    let job = Job::new(
        "broken",
        ScheduleKind::Cron {
            expr: "not a cron".into(),
        },
        "nyx",
    );
    assert!(job.validate().is_err());
    match driver.tick_job(&job, NOW, None, Arc::new(|| panic!("must not run"))) {
        TickDecision::NotDue => {}
        other => panic!("expected NotDue, got {other:?}"),
    }
}

#[test]
fn panicking_executor_is_reported_not_lost() {
    let driver = driver();
    let job = every_minute("panics");
    let rx = fired_outcome(driver.tick_job(&job, NOW, None, Arc::new(|| panic!("boom"))));
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        RunOutcome::Panicked
    );
    // The slot is released: the job can fire again next tick.
    match driver.tick_job(&job, NOW + 60_000, None, Arc::new(|| {})) {
        TickDecision::Fired { .. } => {}
        other => panic!("slot should be free after panic, got {other:?}"),
    }
}
