//! Tests for `pantheon_runtime::watchdog` — sibling file so sources stay
//! test-free.
//!
//! Every test drives `poll_at` with an explicit `Instant` rather than
//! sleeping. The previous version slept 1-5ms and asserted against a 1ms
//! stall budget, so each assertion was a race that could only be lost on a
//! loaded machine. The clock is injected here, so the contract is tested
//! exactly and the suite is deterministic.
use super::*;

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
