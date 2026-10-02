//! Scheduler loop for the gateway service.
//!
//! The gateway background service ticks due scheduled jobs on an interval
//! using `pantheon-scheduler`'s [`TickDriver`] and [`DurableClaimLedger`].
//! Job loading and execution are injected as closures so this crate stays
//! free of runtime/config dependencies - the app crate wires in its job
//! store and `run_job_now`.
//!
//! The loop and `pantheon schedule tick` open the same claim ledger
//! (`<data_dir>/claims.db`) and the same job store (`<data_dir>/schedule.json`),
//! so the two can never double-fire an occurrence: the claim is an atomic
//! first-wins INSERT, and a lost race simply skips.

use pantheon_scheduler::{
    tick::PausedCheck, DurableClaimLedger, RunOutcome, ScheduleKind, TickDecision, TickDriver,
};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// One job the loop knows how to fire: the scheduler core's shared shape
/// the [`pantheon_scheduler::Job`] plus the last fire time driving the due
/// check. Re-exported here so callers (e.g. the app crate building the
/// `ExecuteFn`) name one type. Task text is resolved by the app crate via
/// `job.resolve_task(&TemplateStore::open(data_dir))` before running.
pub use pantheon_scheduler::ScheduledJob;

/// How one job fared in a tick pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FireOutcome {
    /// The claim was won and the run started.
    Fired,
    /// The job was still running and its overlap policy queued another run.
    Queued,
    /// Not fired; the human-readable reason is carried along.
    Skipped(String),
}

/// One job's result from a tick pass.
#[derive(Debug, Clone)]
pub struct TickReport {
    pub id: String,
    pub outcome: FireOutcome,
}

/// Job execution, injected by the app crate. Returns the task-level
/// outcome: the tick driver still reports its own [`RunOutcome`]
/// (executor returned / panicked / timed out), but a task can fail while
/// its thread returns normally (e.g. the agent run errored), and the
/// outcome sink needs that detail for run history and self-heal.
/// `Arc` (not a borrow) because the tick driver runs jobs on detached
/// threads, so the closure must be `'static`.
pub type ExecuteFn = Arc<dyn Fn(&ScheduledJob) -> TaskOutcome + Send + Sync>;

/// What one fired run's task produced, as opposed to what its thread
/// did. The driver reports [`RunOutcome`]; this travels alongside so
/// the outcome sink can record the *task's* result - including the
/// run id (for ledger alerts) and the failure detail when the task
/// itself failed.
#[derive(Debug, Clone, Default)]
pub struct TaskOutcome {
    /// The job run's id ("" when the job type creates no run, e.g. the
    /// nightly pass, or when the run never started).
    pub run_id: String,
    /// Task-level failure detail. `None` means the task itself
    /// succeeded, even if the driver later reports something else.
    pub error: Option<String>,
}

/// Receives one fired run's outcome: called with the job id, the
/// driver's [`RunOutcome`], and the task-level [`TaskOutcome`] (None
/// when the executor thread never reported one - panic or timeout
/// abandon) when the run finishes (or is abandoned on timeout).
/// Installed via [`SchedulerLoop::set_outcome_sink`]; the run-history
/// recorder and the self-heal pipeline are the intended consumers.
/// `None` (the default) keeps the old behavior of dropping the
/// completion channel.
pub type OutcomeSink = Arc<dyn Fn(&str, RunOutcome, Option<TaskOutcome>) + Send + Sync>;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Short relative time for status lines: "now", "in 5m", "in 2h 5m",
/// "in 3d 2h". Pure, so tests assert it without a clock.
pub fn rel_time(delta_ms: i64) -> String {
    if delta_ms <= 0 {
        return "now".to_string();
    }
    let secs = delta_ms / 1000;
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3600;
    let mins = (secs % 3600) / 60;
    if days > 0 {
        format!("in {days}d {hours}h")
    } else if hours > 0 {
        format!("in {hours}h {mins}m")
    } else if mins > 0 {
        format!("in {mins}m")
    } else {
        format!("in {secs}s")
    }
}

/// One-line scheduler queue summary for `pantheon gateway status` and
/// `pantheon init`: active jobs, how many are due right now, and the next
/// scheduled fire. Pure function of the jobs and the clock.
pub fn queue_summary(jobs: &[ScheduledJob], now_ms: i64) -> String {
    let mut active = 0usize;
    let mut due_now = 0usize;
    let mut next: Option<(&str, i64)> = None;
    for j in jobs {
        if j.job.paused {
            continue;
        }
        active += 1;
        let is_due = j.job.due(now_ms, j.last_run);
        if is_due {
            due_now += 1;
        }
        // A job due right now is already counted above; "next" is the next
        // future fire after the pending one.
        if !is_due {
            if let Some(t) = j.job.next_fire_ms(now_ms, j.last_run) {
                if next.is_none_or(|(_, nt)| t < nt) {
                    next = Some((j.job.id.as_str(), t));
                }
            }
        }
    }
    let mut s = format!("{active} active, {due_now} due now");
    match next {
        Some((id, t)) => {
            s.push_str(&format!("; next: {id} {}", rel_time(t - now_ms)));
        }
        None => {
            if active > 0 {
                s.push_str("; no future fire scheduled");
            }
        }
    }
    s
}

/// Owns the tick loop. Share the claim ledger path with every other
/// ticker; the ledger is the single source of truth.
pub struct SchedulerLoop {
    driver: Arc<TickDriver>,
    interval: Duration,
    outcome_sink: Option<OutcomeSink>,
    /// Completion observer for one-shot jobs. One-shot rows are removed
    /// only when the run actually completed - deleting on fire would lose
    /// panicked/timed-out runs silently. Kept separate from
    /// [`SchedulerLoop::outcome_sink`] so installing a run-history sink
    /// never disables one-shot lifecycle handling.
    oneshot_sink: Option<OutcomeSink>,
}

impl SchedulerLoop {
    /// Open on `<data_dir>/claims.db`. Fails when the ledger cannot be
    /// opened - a tick loop that cannot claim must not fire.
    pub fn open(data_dir: &Path, tick_secs: u64) -> Result<Self, String> {
        let ledger = DurableClaimLedger::open(&data_dir.join("claims.db"))
            .map_err(|e| format!("cannot open claim ledger: {e}"))?;
        Ok(Self {
            driver: Arc::new(TickDriver::new(ledger)),
            interval: Duration::from_secs(tick_secs.max(1)),
            outcome_sink: None,
            oneshot_sink: None,
        })
    }

    /// Install the outcome sink: every fired run's [`RunOutcome`] plus
    /// the task-level [`TaskOutcome`] is delivered to it on a watcher
    /// thread when the run finishes. Opt-in; without it the completion
    /// channel is dropped as before.
    pub fn set_outcome_sink(&mut self, sink: OutcomeSink) {
        self.outcome_sink = Some(sink);
    }

    /// Install the one-shot completion observer: called with the job id,
    /// its [`RunOutcome`], and the task-level [`TaskOutcome`] when a
    /// one-shot run finishes (or is abandoned on timeout). The app crate
    /// uses this to remove one-shot rows only on
    /// [`RunOutcome::Completed`].
    pub fn set_oneshot_sink(&mut self, sink: OutcomeSink) {
        self.oneshot_sink = Some(sink);
    }

    /// Install the paused-state predicate the tick driver consults before
    /// draining a queued fire (see [`TickDriver::set_paused_check`]).
    pub fn set_paused_check(&self, check: PausedCheck) {
        self.driver.set_paused_check(check);
    }

    /// One pass over `jobs`: fire everything due. Returns a report per
    /// job that was due, plus the completion-watcher thread handles:
    /// each watcher delivers one fired run's outcome to the sinks when
    /// the run settles. The daemon loop drops the handles (watchers are
    /// detached, as before); `pantheon schedule tick` (one-shot) joins
    /// them so run history and self-heal complete before the process
    /// exits.
    pub fn tick_once(
        &self,
        now_ms: i64,
        jobs: &[ScheduledJob],
        execute: ExecuteFn,
    ) -> (Vec<TickReport>, Vec<std::thread::JoinHandle<()>>) {
        let mut out = Vec::new();
        let mut watchers = Vec::new();
        for j in jobs.iter().filter(|j| !j.job.paused) {
            let job = j.clone();
            let ex = execute.clone();
            // Per-fire slot for the task-level outcome. The thunk stores
            // it; the watcher drains it. A panicked or timed-out run
            // never fills the slot, so the sink sees None - and a late
            // fill from a detached (abandoned) run lands in a dropped
            // slot, never in a later fire. No cross-fire leakage.
            let slot: Arc<std::sync::Mutex<Option<TaskOutcome>>> =
                Arc::new(std::sync::Mutex::new(None));
            let slot_w = Arc::clone(&slot);
            let thunk: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(move || {
                let r = ex(&job);
                if let Ok(mut g) = slot_w.lock() {
                    *g = Some(r);
                }
            });
            let outcome = match self.driver.tick_job(&j.job, now_ms, j.last_run, thunk) {
                TickDecision::Fired { completion } => {
                    // Completion observation fans out on one watcher
                    // thread, which owns the receiver: the outcome sink
                    // (run history + self-heal) gets every outcome, and
                    // the one-shot sink gets one-shot outcomes so
                    // one-shot rows are removed only when the run
                    // actually finished. With neither installed the
                    // channel is dropped as before.
                    let is_oneshot = matches!(j.job.kind, ScheduleKind::OneShot { .. });
                    if self.outcome_sink.is_some() || (is_oneshot && self.oneshot_sink.is_some()) {
                        let id = j.job.id.clone();
                        let sink = self.outcome_sink.clone();
                        let osink = self.oneshot_sink.clone();
                        watchers.push(std::thread::spawn(move || {
                            if let Ok(outcome) = completion.recv() {
                                let task = slot.lock().ok().and_then(|mut g| g.take());
                                if is_oneshot {
                                    if let Some(os) = osink {
                                        os(&id, outcome, task.clone());
                                    }
                                }
                                if let Some(s) = sink {
                                    s(&id, outcome, task);
                                }
                            }
                        }));
                    }
                    FireOutcome::Fired
                }
                TickDecision::Queued => FireOutcome::Queued,
                TickDecision::NotDue => continue,
                TickDecision::SkippedClaimLost => {
                    FireOutcome::Skipped("already claimed, skipping (replay)".to_string())
                }
                TickDecision::SkippedOverlap => {
                    FireOutcome::Skipped("still running, skipping (overlap=skip)".to_string())
                }
                TickDecision::ClaimFailed(e) => {
                    FireOutcome::Skipped(format!("claim failed, not run: {e}"))
                }
            };
            out.push(TickReport {
                id: j.job.id.clone(),
                outcome,
            });
        }
        (out, watchers)
    }

    /// Loop forever: reload jobs every pass (so `schedule` edits apply
    /// without restarting the service), tick, report, sleep.
    pub fn run_forever(
        &self,
        load: &(dyn Fn() -> Vec<ScheduledJob> + Send + Sync),
        execute: ExecuteFn,
        on_tick: &(dyn Fn(&[TickReport], i64) + Send + Sync),
    ) -> ! {
        loop {
            let now = now_ms();
            let jobs = load();
            let (reports, _watchers) = self.tick_once(now, &jobs, execute.clone());
            // Daemon loop: watchers are detached; outcomes reach the
            // sinks on their own threads, same as before.
            on_tick(&reports, now);
            std::thread::sleep(self.interval);
        }
    }
}
