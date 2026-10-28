//! Hung-gateway watchdog.
//!
//! A gateway poller thread that stops ticking is hung: its channel
//! stops delivering in both directions, and no in-process check can
//! unstick a thread blocked inside a transport call (Rust cannot kill
//! a thread). The supervisor therefore abandons the hung thread and
//! spawns a replacement poller - exactly ONCE per channel. A second
//! hang is not restarted: the supervisor reports the failure (loud log
//! + a persistent `watchdog-<channel>.json` report under the gateway
//! state dir, which the operator - and the agent, when asked - can
//! read) and stops supervising that channel. No infinite restart loop:
//! a channel that hangs, restarts, and hangs again is broken, and
//! restarting it forever would flap its cursors and duplicate
//! deliveries while hiding the breakage.
//!
//! This module is the decision logic only (clock-injectable, unit
//! tested). The heartbeat itself is a shared millisecond timestamp
//! the daemon loop touches each iteration
//! ([`ChannelDaemon::with_heartbeat`](crate::daemon::ChannelDaemon::with_heartbeat));
//! the supervisor thread in the gateway runner owns one
//! [`GatewayWatchdog`] per channel and acts on its verdicts.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long a poller may go without a heartbeat before it counts as
/// hung. Must exceed one Telegram long-poll cycle (25 s) plus
/// scheduling slack, or healthy long polls would false-positive.
pub const HANG_BUDGET: Duration = Duration::from_secs(90);

/// What the supervisor should do about a channel right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogAction {
    /// Heartbeat fresh (or not yet due): nothing to do.
    Ok,
    /// First hang: abandon the hung poller and spawn a replacement once.
    Restart,
    /// Second hang (or later): do NOT restart again. Report the
    /// failure and stop supervising this channel.
    Report,
}

/// Shared heartbeat timestamp (epoch millis) a daemon loop touches
/// each iteration. `0` = never ticked.
#[derive(Debug, Default)]
pub struct GatewayHeartbeat {
    last_ms: AtomicU64,
}

impl GatewayHeartbeat {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a tick at wall-clock now.
    pub fn touch(&self) {
        self.last_ms.store(now_ms(), Ordering::Release);
    }

    /// Last tick's epoch millis, or `0` when the loop never ticked.
    pub fn last_ms(&self) -> u64 {
        self.last_ms.load(Ordering::Acquire)
    }
}

/// Wall-clock epoch millis (test seam: tests drive `check_at` /
/// `heartbeat_at` with explicit timestamps instead).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One channel's restart budget: one restart, then report-only.
#[derive(Debug)]
pub struct GatewayWatchdog {
    budget_ms: u64,
    last_heartbeat_ms: u64,
    started_ms: u64,
    restarted: bool,
    reported: bool,
}

impl GatewayWatchdog {
    pub fn new(budget: Duration) -> Self {
        Self::new_at(budget, now_ms())
    }

    /// Deterministic constructor for tests: `now_ms` is the supervisor
    /// start time, which counts as the initial heartbeat (a freshly
    /// spawned poller has not ticked yet, but it is not hung).
    pub fn new_at(budget: Duration, now_ms: u64) -> Self {
        Self {
            budget_ms: budget.as_millis() as u64,
            last_heartbeat_ms: now_ms,
            started_ms: now_ms,
            restarted: false,
            reported: false,
        }
    }

    /// Record a heartbeat at wall-clock now.
    pub fn heartbeat(&mut self) {
        self.heartbeat_at(now_ms());
    }

    /// Record a heartbeat at an explicit timestamp.
    pub fn heartbeat_at(&mut self, at_ms: u64) {
        if at_ms > self.last_heartbeat_ms {
            self.last_heartbeat_ms = at_ms;
        }
    }

    /// Evaluate at wall-clock now.
    pub fn check(&mut self) -> WatchdogAction {
        self.check_at(now_ms())
    }

    /// Evaluate at an explicit timestamp.
    ///
    /// - Fresh heartbeat (or no heartbeat yet but still inside the
    ///   budget measured from start) → `Ok`.
    /// - First breach → `Restart` (consumes the one restart).
    /// - Any later breach → `Report`, sticky: the supervisor must not
    ///   restart again, and repeated checks keep reporting rather
    ///   than silently recovering.
    /// A heartbeat after a restart re-arms the budget: a recovered
    /// channel is healthy again, but the restart budget is NOT
    /// refunded - a channel that hangs, recovers, and hangs again
    /// still reports instead of restarting a second time.
    pub fn check_at(&mut self, at_ms: u64) -> WatchdogAction {
        if self.reported {
            return WatchdogAction::Report;
        }
        let silent = at_ms.saturating_sub(self.last_heartbeat_ms);
        if silent <= self.budget_ms {
            return WatchdogAction::Ok;
        }
        if !self.restarted {
            self.restarted = true;
            return WatchdogAction::Restart;
        }
        self.reported = true;
        WatchdogAction::Report
    }

    /// True once the one restart has been consumed (even if the
    /// channel recovered afterwards).
    pub fn restarted(&self) -> bool {
        self.restarted
    }

    /// True once the watchdog has escalated to report-only.
    pub fn reported(&self) -> bool {
        self.reported
    }

    /// Supervisor start time (epoch millis), for report files.
    pub fn started_ms(&self) -> u64 {
        self.started_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budget() -> Duration {
        Duration::from_secs(90)
    }

    #[test]
    fn fresh_heartbeat_is_ok_and_silence_within_budget_is_ok() {
        let mut w = GatewayWatchdog::new_at(budget(), 1_000);
        assert_eq!(w.check_at(1_000 + 89_000), WatchdogAction::Ok);
        w.heartbeat_at(1_000 + 89_000);
        assert_eq!(w.check_at(1_000 + 89_000 + 90_000), WatchdogAction::Ok);
    }

    #[test]
    fn first_breach_restarts_once_then_second_breach_reports() {
        let mut w = GatewayWatchdog::new_at(budget(), 1_000);
        assert_eq!(w.check_at(1_000 + 90_001), WatchdogAction::Restart);
        assert!(w.restarted());
        // The replacement poller heartbeats: healthy again, but the
        // restart budget is spent.
        w.heartbeat_at(1_000 + 91_000);
        assert_eq!(w.check_at(1_000 + 91_000 + 90_000), WatchdogAction::Ok);
        assert_eq!(w.check_at(1_000 + 91_000 + 90_001), WatchdogAction::Report);
        assert!(w.reported());
        // Sticky: no second restart, ever.
        assert_eq!(w.check_at(1_000 + 500_000), WatchdogAction::Report);
    }

    #[test]
    fn no_heartbeat_after_restart_reports_without_another_restart() {
        let mut w = GatewayWatchdog::new_at(budget(), 1_000);
        assert_eq!(w.check_at(1_000 + 90_001), WatchdogAction::Restart);
        // The replacement never ticks: next check reports, not restarts.
        assert_eq!(w.check_at(1_000 + 180_002), WatchdogAction::Report);
    }
}
