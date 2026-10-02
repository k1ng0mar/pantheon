//! Watchdog + hard-kill tests for the runtime supervisor, moved out of
//! the crate's unit suite.
//!
//! Policy: only small deterministic unit tests live beside the code
//! (`cargo test -p <crate>`). Everything behavioral - SQLite stores,
//! threads, subprocesses, timing, filesystem - lives here and runs via
//! `cargo test -p pantheon-eval`.
//!
//! The watchdog tests drive `poll_at` with an explicit `Instant` rather
//! than sleeping, so the stall contract is tested exactly and the suite
//! is deterministic. The kill tests exercise `Supervisor::kill_run_turn`,
//! the hard-stop path behind `POST /api/runs/:id/kill`.

use pantheon_runtime::{TurnWatchdog, WatchdogAction};
use std::time::{Duration, Instant};

/// A fixed clock base. `Instant` has no public constructor, so the base is
/// captured once per thread and every sample is an exact offset from it.
/// What matters is that the offsets are precise, not the absolute value.
fn base() -> Instant {
    thread_local! {
        static BASE: Instant = Instant::now();
    }
    BASE.with(|b| *b)
}

/// A watchdog with a 1s stall budget, pinned to the current base instant.
fn wd() -> TurnWatchdog {
    TurnWatchdog::new_at(Duration::from_secs(1), base())
}

/// A clock sample `ms` milliseconds after the base.
fn at(ms: u64) -> Instant {
    base() + Duration::from_millis(ms)
}

/// The core rule: silence alone only earns a probe, and only a *failed*
/// probe kills. A successful probe is ordinary activity.
#[test]
fn only_failed_probe_kills() {
    let mut w = wd();
    // Under budget: nothing happens.
    assert_eq!(w.poll_at(at(500)), WatchdogAction::Continue);
    // Over budget: probe, not kill.
    assert_eq!(w.poll_at(at(1_500)), WatchdogAction::Probe);
    // Probe reports success: continue, and the probe is cleared.
    assert_eq!(w.probe_result_at(true, at(1_500)), WatchdogAction::Continue);
    // Still not killed, and still probing rather than killing on a second
    // silence without a new probe result.
    assert_eq!(w.poll_at(at(1_600)), WatchdogAction::Continue);
    assert_eq!(w.poll_at(at(2_600)), WatchdogAction::Probe);
    // A failed probe escalates, and once killed the watchdog stays killed.
    assert_eq!(w.probe_result_at(false, at(2_600)), WatchdogAction::Kill);
    assert_eq!(w.poll_at(at(3_000)), WatchdogAction::Kill);
}

/// Merely observing silence can never escalate to a kill: only an *expired*
/// probe does. This is the invariant the `probe_timed_out` doc states.
#[test]
fn probe_timeout_alone_never_kills() {
    let mut w = wd().with_probe_timeout(Duration::from_millis(1));
    assert_eq!(w.poll_at(at(1_500)), WatchdogAction::Probe);
    // Second poll with the probe still in flight: still just a probe.
    assert_eq!(w.poll_at(at(2_000)), WatchdogAction::Probe);
    // The probe has now been outstanding for 500ms, past its 1ms timeout,
    // so treating it as failed is correct.
    assert_eq!(w.probe_timed_out_at(at(2_000)), WatchdogAction::Kill);
}

/// A human pause is explicit state, not inactivity: time spent paused must
/// not be charged to the stall budget, and resuming starts a fresh window
/// so a long approval wait can never cause a post-resume kill.
#[test]
fn pause_does_not_eat_clock() {
    let mut w = wd();
    w.pause();
    // Ten seconds of "silence" while paused, with no real waiting.
    assert_eq!(w.poll_at(at(10_000)), WatchdogAction::Continue);
    // Resume at a point 10s after the base: the window restarts there, so
    // the ten paused seconds are not charged to the budget.
    w.resume_at(at(10_000));
    assert_eq!(w.stall_for_at(at(10_500)), Duration::from_millis(500));
    assert_eq!(w.poll_at(at(10_500)), WatchdogAction::Continue);
}

/// `activity()` is the runtime reporting progress, and it must clear both the
/// stall window and any in-flight probe. Without the probe reset, a probe
/// started before the activity would still be counted as outstanding.
#[test]
fn activity_clears_both_the_window_and_the_probe() {
    let mut w = wd();
    assert_eq!(w.poll_at(at(1_500)), WatchdogAction::Probe);
    w.activity_at(at(1_600));
    // Under budget again after the reset.
    assert_eq!(w.poll_at(at(1_600)), WatchdogAction::Continue);
    // And the cleared probe means an immediate timeout check is a no-op
    // rather than a kill of a turn that is demonstrably making progress.
    assert_eq!(w.probe_timed_out_at(at(1_600)), WatchdogAction::Continue);
}

/// `stall_for` is what the runtime reports, so it must be the same quantity
/// `poll_at` decides on, not a separate wall-clock reading.
#[test]
fn stall_for_matches_the_window_poll_decides_on() {
    let mut w = wd();
    assert_eq!(w.stall_for_at(at(900)), Duration::from_millis(900));
    assert_eq!(w.poll_at(at(900)), WatchdogAction::Continue);
    assert_eq!(w.stall_for_at(at(1_000)), Duration::from_millis(1_000));
    // Budget is 1s, so exactly at the budget is over it.
    assert_eq!(w.poll_at(at(1_000)), WatchdogAction::Probe);
}

// ── kill_run_turn ─────────────────────────────────────────────────────

/// A run row plus a live lease, as if a turn were in flight. The lease
/// row's TTL (30s default) keeps it live for the test - no heartbeat
/// thread needed.
fn busy_supervisor(dir: &std::path::Path, run_id: &str) -> pantheon_runtime::Supervisor {
    let sup = pantheon_runtime::Supervisor::open(dir.to_path_buf()).unwrap();
    sup.start_run(run_id).unwrap();
    let _lease = sup.acquire_lease(run_id).unwrap();
    sup
}

/// No lease, no turn: the kill refuses instead of signaling blindly.
#[test]
fn kill_without_lease_is_no_turn() {
    let dir = tempfile::tempdir().unwrap();
    let sup = pantheon_runtime::Supervisor::open(dir.path().to_path_buf()).unwrap();
    let err = sup.kill_run_turn("run-missing", 12345).unwrap_err();
    assert_eq!(err.code, "RT_NO_TURN");
}

/// A lease but a PID that identifies nothing: the kill refuses and
/// records no cancel intent.
#[test]
fn kill_bogus_pid_is_no_turn() {
    let dir = tempfile::tempdir().unwrap();
    let sup = busy_supervisor(dir.path(), "run-bogus");
    let err = sup.kill_run_turn("run-bogus", u32::MAX).unwrap_err();
    assert_eq!(err.code, "RT_NO_TURN");
    assert_ne!(
        sup.ledger_status("run-bogus").unwrap().as_deref(),
        Some("canceled"),
        "refused kill must not record intent"
    );
}

/// A stand-in for the dashboard's turn child: its own session/process
/// group (setsid) and `--taskID <run_id>` in its cmdline, like
/// `pantheon run --taskID <run_id> --say ...`.
///
/// The stand-in writes its real PID to `pidfile`: when setsid(1) forks
/// (the test binary was started as a process-group leader), the spawned
/// PID is a reaped intermediate, so the PID must come from the stand-in
/// itself. `read` blocks on piped stdin with no children, so the cmdline
/// argv is never exec'd away - dash replaces `sh -c 'sleep 60'` with
/// `sleep`, which would lose the `--taskID` marker.
#[cfg(target_os = "linux")]
fn spawn_turn_standin(pidfile: &std::path::Path, run_id: &str) -> std::process::Child {
    let mut cmd = std::process::Command::new("setsid");
    cmd.args([
        "sh",
        "-c",
        &format!("echo $$ > {}; read dummy", pidfile.display()),
        "turn-standin",
        "--taskID",
        run_id,
    ]);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.spawn().expect("spawn turn stand-in")
}

/// Read the stand-in's self-reported PID, bounded. Panics if the
/// stand-in never starts - the fixture failed, not the kill.
#[cfg(target_os = "linux")]
fn standin_pid(pidfile: &std::path::Path) -> u32 {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Ok(text) = std::fs::read_to_string(pidfile) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                return pid;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "stand-in never wrote its pidfile"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Poll until the stand-in is gone, bounded. The child handle may be a
/// reaped setsid intermediate, so `/proc/<pid>` is the source of truth;
/// `try_wait` reaps the direct child in the common no-fork case.
/// Panics if the process survives - the kill failed.
#[cfg(target_os = "linux")]
fn wait_dead(child: &mut std::process::Child, pid: u32) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let _ = child.try_wait();
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "turn child survived the hard kill"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Best-effort cleanup for a stand-in the test did not kill: SIGKILL the
/// real PID (`kill` is a shell builtin, always present) and reap.
#[cfg(target_os = "linux")]
fn cleanup_standin(child: &mut std::process::Child, pid: u32) {
    let _ = std::process::Command::new("sh")
        .args(["-c", &format!("kill -9 {pid}")])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let _ = child.wait();
}

/// A live process whose cmdline does not carry this run's `--taskID`
/// is never signaled: the kill refuses and the process stays alive.
/// This is the PID-recycling guard.
#[cfg(target_os = "linux")]
#[test]
fn kill_wrong_cmdline_pid_leaves_process_alive() {
    let dir = tempfile::tempdir().unwrap();
    let sup = busy_supervisor(dir.path(), "run-guard");
    let pidfile = dir.path().join("standin.pid");
    let mut other = spawn_turn_standin(&pidfile, "some-other-run");
    let pid = standin_pid(&pidfile);
    let err = sup.kill_run_turn("run-guard", pid).unwrap_err();
    assert_eq!(err.code, "RT_NO_TURN");
    assert!(
        std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "the foreign process must survive the refused kill"
    );
    cleanup_standin(&mut other, pid);
}

/// The real thing: a live turn child with the run's `--taskID` in its
/// cmdline is TERM/KILLed and the run is marked canceled.
#[cfg(target_os = "linux")]
#[test]
fn kill_terminates_turn_child() {
    let dir = tempfile::tempdir().unwrap();
    let sup = busy_supervisor(dir.path(), "run-killme");
    let pidfile = dir.path().join("standin.pid");
    let mut child = spawn_turn_standin(&pidfile, "run-killme");
    let pid = standin_pid(&pidfile);
    sup.kill_run_turn("run-killme", pid).unwrap();
    wait_dead(&mut child, pid);
    assert_eq!(
        sup.ledger_status("run-killme").unwrap().as_deref(),
        Some("canceled"),
        "hard kill records the cancel intent first"
    );
}
