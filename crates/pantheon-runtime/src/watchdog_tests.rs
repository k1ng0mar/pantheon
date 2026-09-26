//! Tests for `pantheon_runtime::watchdog::tests` — sibling file so sources stay test-free.
use super::*;

#[test]
fn only_failed_probe_kills() {
    let mut w = TurnWatchdog::new(Duration::from_millis(1));
    std::thread::sleep(Duration::from_millis(3));
    assert_eq!(w.poll(), WatchdogAction::Probe);
    assert_eq!(w.probe_result(true), WatchdogAction::Continue);
    std::thread::sleep(Duration::from_millis(3));
    assert_eq!(w.poll(), WatchdogAction::Probe);
    assert_eq!(w.probe_result(false), WatchdogAction::Kill);
    assert_eq!(w.poll(), WatchdogAction::Kill);
}

#[test]
fn probe_timeout_alone_never_kills() {
    let mut w =
        TurnWatchdog::new(Duration::from_millis(1)).with_probe_timeout(Duration::from_millis(1));
    std::thread::sleep(Duration::from_millis(3));
    assert_eq!(w.poll(), WatchdogAction::Probe);
    std::thread::sleep(Duration::from_millis(3));
    assert_eq!(w.poll(), WatchdogAction::Probe);
    assert_eq!(w.probe_timed_out(), WatchdogAction::Kill);
}

#[test]
fn pause_does_not_eat_clock() {
    let mut w = TurnWatchdog::new(Duration::from_millis(1));
    w.pause();
    std::thread::sleep(Duration::from_millis(5));
    assert_eq!(w.poll(), WatchdogAction::Continue);
    w.resume();
    assert!(w.stall_for() < Duration::from_millis(3));
}
