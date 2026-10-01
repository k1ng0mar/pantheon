//! Activity-based turn watchdog.
//!
//! A turn is not killed because it ran for a long wall-clock duration.  The
//! runtime calls `activity()` whenever it observes progress, and the
//! watchdog probes only after a silence longer than the stall budget.  A
//! human pause is an explicit state, not inactivity.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchdogAction {
    Continue,
    Probe,
    Kill,
}

#[derive(Debug, Clone)]
pub struct TurnWatchdog {
    stall_budget: Duration,
    probe_timeout: Duration,
    last_activity: Instant,
    probe_started: Option<Instant>,
    paused: bool,
    killed: bool,
}

impl Default for TurnWatchdog {
    fn default() -> Self {
        Self::new(Duration::from_secs(30))
    }
}

impl TurnWatchdog {
    pub fn new(stall_budget: Duration) -> Self {
        Self {
            stall_budget,
            probe_timeout: Duration::from_secs(2),
            last_activity: Instant::now(),
            probe_started: None,
            paused: false,
            killed: false,
        }
    }

    /// Construct with an explicit clock base, so `poll_at` can be driven
    /// by a test without sleeping. Production uses `new`.
    pub fn new_at(stall_budget: Duration, now: Instant) -> Self {
        Self {
            stall_budget,
            probe_timeout: Duration::from_secs(2),
            last_activity: now,
            probe_started: None,
            paused: false,
            killed: false,
        }
    }

    pub fn with_probe_timeout(mut self, timeout: Duration) -> Self {
        self.probe_timeout = timeout;
        self
    }

    /// Configure from `PANTHEON_STALL_BUDGET_MS` and
    /// `PANTHEON_PROBE_TIMEOUT_MS`, with safe defaults.
    pub fn from_env() -> Self {
        let stall = std::env::var("PANTHEON_STALL_BUDGET_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_secs(30));
        let probe = std::env::var("PANTHEON_PROBE_TIMEOUT_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_secs(2));
        Self::new(stall).with_probe_timeout(probe)
    }

    pub fn activity(&mut self) {
        self.activity_at(Instant::now());
    }

    /// Deterministic `activity` at an explicit clock sample.
    pub fn activity_at(&mut self, now: Instant) {
        self.last_activity = now;
        self.probe_started = None;
    }

    /// Pause without charging elapsed time to the stall budget.  This is
    /// used while waiting for a human approval decision.
    pub fn pause(&mut self) {
        self.paused = true;
        self.probe_started = None;
    }

    /// Resume starts a fresh activity window, so a human pause never causes
    /// a post-resume kill.
    pub fn resume(&mut self) {
        self.paused = false;
        self.activity();
    }

    /// Deterministic `resume` at an explicit clock sample.
    pub fn resume_at(&mut self, now: Instant) {
        self.paused = false;
        self.activity_at(now);
    }

    pub fn stall_for(&self) -> Duration {
        self.last_activity.elapsed()
    }

    /// Deterministic `stall_for` at an explicit clock sample.
    pub fn stall_for_at(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.last_activity)
    }

    /// Report the result of the probe.  Only a failed probe escalates to a
    /// kill; a successful probe is ordinary activity.
    pub fn probe_result(&mut self, succeeded: bool) -> WatchdogAction {
        self.probe_result_at(succeeded, Instant::now())
    }

    /// Deterministic `probe_result` at an explicit clock sample, so a test
    /// can drive the full probe lifecycle on an injected timeline instead of
    /// sleeping and racing the real clock.
    pub fn probe_result_at(&mut self, succeeded: bool, now: Instant) -> WatchdogAction {
        if succeeded {
            self.last_activity = now;
            self.probe_started = None;
            WatchdogAction::Continue
        } else {
            self.killed = true;
            WatchdogAction::Kill
        }
    }

    /// Poll and run a lightweight liveness probe when the stall budget is
    /// exceeded. A successful probe records activity; only an error becomes
    /// `Kill`.
    pub fn poll_with_probe<F>(&mut self, probe: F) -> WatchdogAction
    where
        F: FnOnce() -> Result<(), String>,
    {
        match self.poll() {
            WatchdogAction::Probe => match probe() {
                Ok(()) => self.probe_result(true),
                Err(_) => self.probe_result(false),
            },
            other => other,
        }
    }

    pub fn poll(&mut self) -> WatchdogAction {
        if self.killed {
            return WatchdogAction::Kill;
        }
        if self.paused {
            return WatchdogAction::Continue;
        }
        if self.stall_for() < self.stall_budget {
            return WatchdogAction::Continue;
        }
        match self.probe_started {
            None => {
                self.probe_started = Some(Instant::now());
                WatchdogAction::Probe
            }
            Some(_) => WatchdogAction::Probe,
        }
    }

    /// Treat an expired in-flight probe as a failed probe. This is explicit
    /// so merely observing silence can never escalate to a kill.
    pub fn probe_timed_out(&mut self) -> WatchdogAction {
        self.probe_timed_out_at(Instant::now())
    }

    /// Deterministic `probe_timed_out` at an explicit clock sample.
    pub fn probe_timed_out_at(&mut self, now: Instant) -> WatchdogAction {
        match self.probe_started {
            Some(started) if now.saturating_duration_since(started) >= self.probe_timeout => {
                self.probe_result_at(false, now)
            }
            Some(_) => WatchdogAction::Probe,
            None => WatchdogAction::Continue,
        }
    }

    /// Deterministic variant for tests and supervisors that already have a
    /// monotonic clock sample.
    pub fn poll_at(&mut self, now: Instant) -> WatchdogAction {
        if self.killed {
            return WatchdogAction::Kill;
        }
        if self.paused {
            return WatchdogAction::Continue;
        }
        let silent = now.saturating_duration_since(self.last_activity);
        if silent < self.stall_budget {
            return WatchdogAction::Continue;
        }
        match self.probe_started {
            None => {
                self.probe_started = Some(now);
                WatchdogAction::Probe
            }
            Some(_) => WatchdogAction::Probe,
        }
    }
}
