//! Scheduler loop for the gateway service.
//!
//! The gateway background service ticks due scheduled jobs on an interval
//! using `pantheon-scheduler`'s [`TickDriver`] and [`DurableClaimLedger`].
//! Job loading and execution are injected as closures so this crate stays
//! free of runtime/config dependencies — the app crate wires in its job
//! store and `run_job_now`.
//!
//! The loop and `pantheon schedule tick` open the same claim ledger
//! (`<data_dir>/claims.db`) and the same job store (`<data_dir>/schedule.json`),
//! so the two can never double-fire an occurrence: the claim is an atomic
//! first-wins INSERT, and a lost race simply skips.

use pantheon_scheduler::{DurableClaimLedger, Job, TickDecision, TickDriver};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// One job the loop knows how to fire: the scheduler's view of the job,
/// the task text to execute, and the last fire time driving the due check.
#[derive(Debug, Clone)]
pub struct SchedulableJob {
    pub job: Job,
    pub task: String,
    pub last_run: Option<i64>,
}

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

/// Job execution, injected by the app crate. `Arc` (not a borrow) because
/// the tick driver runs jobs on detached threads, so the closure must be
/// `'static`.
pub type ExecuteFn = Arc<dyn Fn(&SchedulableJob) + Send + Sync>;

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
pub fn queue_summary(jobs: &[SchedulableJob], now_ms: i64) -> String {
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
}

impl SchedulerLoop {
    /// Open on `<data_dir>/claims.db`. Fails when the ledger cannot be
    /// opened — a tick loop that cannot claim must not fire.
    pub fn open(data_dir: &Path, tick_secs: u64) -> Result<Self, String> {
        let ledger = DurableClaimLedger::open(&data_dir.join("claims.db"))
            .map_err(|e| format!("cannot open claim ledger: {e}"))?;
        Ok(Self {
            driver: Arc::new(TickDriver::new(ledger)),
            interval: Duration::from_secs(tick_secs.max(1)),
        })
    }

    /// One pass over `jobs`: fire everything due. Returns a report per
    /// job that was due, paused jobs are skipped silently.
    pub fn tick_once(
        &self,
        now_ms: i64,
        jobs: &[SchedulableJob],
        execute: ExecuteFn,
    ) -> Vec<TickReport> {
        let mut out = Vec::new();
        for j in jobs.iter().filter(|j| !j.job.paused) {
            let job = j.clone();
            let ex = execute.clone();
            let thunk: Arc<dyn Fn() + Send + Sync + 'static> = Arc::new(move || ex(&job));
            let outcome = match self.driver.tick_job(&j.job, now_ms, j.last_run, thunk) {
                TickDecision::Fired { .. } => FireOutcome::Fired,
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
        out
    }

    /// Loop forever: reload jobs every pass (so `schedule` edits apply
    /// without restarting the service), tick, report, sleep.
    pub fn run_forever(
        &self,
        load: &(dyn Fn() -> Vec<SchedulableJob> + Send + Sync),
        execute: ExecuteFn,
        on_tick: &(dyn Fn(&[TickReport], i64) + Send + Sync),
    ) -> ! {
        loop {
            let now = now_ms();
            let jobs = load();
            let reports = self.tick_once(now, &jobs, execute.clone());
            on_tick(&reports, now);
            std::thread::sleep(self.interval);
        }
    }
}
