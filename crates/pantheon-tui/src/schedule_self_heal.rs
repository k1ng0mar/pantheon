//! Self-healing scheduled task runs.
//!
//! When a scheduled task's run fails, the outcome sink built by
//! [`SelfHealer`] investigates the failure in a bounded agent session,
//! retries the task exactly once when the cause was fixed, and always
//! leaves a durable, user-visible ledger event:
//! [`Event::ScheduledTaskFailed`] or [`Event::ScheduledTaskRecovered`].
//!
//! This is per-task runtime handling, separate from the nightly repair
//! sweep (which repairs MCP servers / tools / scheduled-task *configs*
//! themselves on a schedule). The investigator may fix the underlying
//! cause (config, broken dependency, stale state) but never touches
//! other scheduled jobs, never starts schedulers or daemons, and never
//! approves anything on the user's behalf.
//!
//! Kill switch: `PANTHEON_SCHEDULER_SELF_HEAL=0` (or `false`/`no`/`off`)
//! disables the investigate+retry pipeline. Failure recording and the
//! user-visible alert stay on - a silenced failure with no alert would
//! be worse than no self-heal at all.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use pantheon_agent::{Budget, LoopOutcome};
use pantheon_api::capability::Policy;
use pantheon_api::events::Event;
use pantheon_api::logging::now_ms;
use pantheon_gateway::scheduler::{OutcomeSink, TaskOutcome};
use pantheon_runtime::session::Session;
use pantheon_scheduler::{Job, JobRunStats, RunHistory, RunOutcome, TemplateStore};
use pantheon_storage::Ledger;

use crate::config;
use crate::schedule::{load_schedulable, run_job_now, JobRunReport};

/// Investigator wall-clock bound. A diagnosis that needs longer than
/// this is a hung investigation, not a diagnosis.
const INVESTIGATE_TIMEOUT: Duration = Duration::from_secs(300);
/// Cap on alert detail text so one bad run cannot bloat a ledger row.
const MAX_DETAIL_CHARS: usize = 2000;

/// Verdict from the investigator session.
#[derive(Debug, Clone)]
pub struct HealVerdict {
    /// True when the investigator found and fixed the root cause.
    pub fixed: bool,
    /// Short paragraph: what failed, what changed, what to watch.
    pub summary: String,
    /// The investigator run's id, so the alert can attach to a run even
    /// when the original job type creates none (e.g. the nightly pass).
    pub run_id: String,
}

/// Input to the investigator. Test seam: production builds a real
/// bounded [`Session`]; tests inject a fake.
pub struct InvestigateInput<'a> {
    pub data_dir: &'a Path,
    pub job: &'a Job,
    pub task_text: &'a str,
    pub error: &'a str,
    pub stats: Option<&'a JobRunStats>,
}

/// The investigator: diagnose + fix, return a verdict. Production
/// implementation runs a bounded agent session; tests inject a fake.
pub type InvestigateFn = Arc<dyn for<'a> Fn(&'a InvestigateInput<'a>) -> HealVerdict + Send + Sync>;

/// The retry: re-run the task once. Production delegates to
/// [`run_job_now`]; tests inject a fake. Takes `(job, task_text,
/// data_dir)`.
pub type RetryFn = Arc<dyn Fn(&Job, &str, &Path) -> JobRunReport + Send + Sync>;

/// Self-heal kill switch. On by default; `PANTHEON_SCHEDULER_SELF_HEAL`
/// set to `0`/`false`/`no`/`off` disables investigate+retry. Failure
/// recording and the user-visible alert are unaffected.
pub(crate) fn scheduler_self_heal_enabled() -> bool {
    match std::env::var("PANTHEON_SCHEDULER_SELF_HEAL") {
        Err(_) => true,
        Ok(v) => !matches!(
            v.to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
    }
}

/// Builds the outcome sink that records every fired run and heals the
/// failed ones. Cheap to construct; share one per scheduler loop.
pub struct SelfHealer {
    data_dir: PathBuf,
    heal_enabled: bool,
    investigator: InvestigateFn,
    retry: RetryFn,
}

impl SelfHealer {
    /// Production constructor: real investigator session, real retry,
    /// kill switch read from the environment.
    pub fn new(data_dir: &Path) -> Arc<Self> {
        Self::with_options(
            data_dir,
            scheduler_self_heal_enabled(),
            Arc::new(investigate_real),
            Arc::new(|job, task, dd| run_job_now(task, job, dd)),
        )
    }

    /// Test seam: explicit kill switch, investigator, and retry.
    pub fn with_options(
        data_dir: &Path,
        heal_enabled: bool,
        investigator: InvestigateFn,
        retry: RetryFn,
    ) -> Arc<Self> {
        Arc::new(Self {
            data_dir: data_dir.to_path_buf(),
            heal_enabled,
            investigator,
            retry,
        })
    }

    /// The outcome sink: records every fired run into run history,
    /// then investigates+retries+alerts on failures. Runs on the
    /// watcher's thread (one per fired run); the one-shot `tick` path
    /// joins those watchers, so healing completes before exit there.
    pub fn outcome_sink(self: &Arc<Self>) -> OutcomeSink {
        let me = Arc::clone(self);
        Arc::new(
            move |job_id: &str, driver_outcome: RunOutcome, task: Option<TaskOutcome>| {
                me.handle_outcome(job_id, driver_outcome, task);
            },
        )
    }

    fn handle_outcome(&self, job_id: &str, driver: RunOutcome, task: Option<TaskOutcome>) {
        // Task-level truth wins when the executor reported one: a task
        // that errored while its thread returned normally is a failure,
        // and its error text is what history (and the alert) carries.
        // No task report (panic / timeout abandonment) falls back to the
        // driver's outcome.
        let (record_outcome, error, run_id) = match &task {
            Some(t) if t.error.is_some() => (RunOutcome::Failed, t.error.clone(), t.run_id.clone()),
            Some(t) => (driver, None, t.run_id.clone()),
            None => (driver, None, String::new()),
        };

        if let Err(e) = self.record(job_id, record_outcome, error.clone()) {
            eprintln!("self-heal: could not record run history for {job_id}: {e}");
        }

        let failed = matches!(
            record_outcome,
            RunOutcome::Failed | RunOutcome::TimedOut | RunOutcome::Panicked
        );
        if !failed {
            return;
        }
        let error_text = error.unwrap_or_default();

        if !self.heal_enabled {
            self.alert_failed(
                job_id,
                &run_id,
                &error_text,
                "self-heal is disabled (PANTHEON_SCHEDULER_SELF_HEAL). Manual attention needed.",
            );
            return;
        }

        // A panic inside the pipeline must not swallow the failure:
        // it becomes the not-fixed alert.
        let healed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.heal(job_id, &run_id, &error_text)
        }));
        match healed {
            Ok(outcome) => self.alert(job_id, &run_id, &error_text, &outcome),
            Err(_) => self.alert_failed(
                job_id,
                &run_id,
                &error_text,
                "the self-heal pipeline panicked; the original failure is unhandled.",
            ),
        }
    }

    fn record(
        &self,
        job_id: &str,
        outcome: RunOutcome,
        error: Option<String>,
    ) -> Result<(), String> {
        let mut history = RunHistory::open(&self.data_dir)?;
        history.record(job_id, outcome, error, now_ms())
    }

    /// Investigate the failure, fix if possible, retry once, and
    /// describe the result. Never silently skips: every path returns a
    /// [`HealOutcome`] that the caller turns into a ledger alert.
    fn heal(&self, job_id: &str, run_id: &str, error: &str) -> HealOutcome {
        let jobs = match load_schedulable(&self.data_dir) {
            Ok(j) => j,
            Err(e) => {
                return HealOutcome::not_fixed(
                    format!("could not load the schedule to investigate: {e}"),
                    run_id,
                );
            }
        };
        let Some(entry) = jobs.iter().find(|j| j.job.id == job_id) else {
            return HealOutcome::not_fixed(
                format!("job {job_id} is gone from the schedule; nothing to investigate"),
                run_id,
            );
        };
        let job = &entry.job;
        // Repair spend cap: `[repair].max_repairs_per_task_per_day`
        // (default 3). A flapping task must not burn API budget on
        // investigator sessions without bound. Exhausted -> skip the
        // investigation and return not-fixed, which the caller turns into
        // the same disable-with-escalation alert as an unfixable failure.
        let max_repairs = crate::config::Config::load_or_report(&self.data_dir)
            .map(|c| {
                c.repair
                    .as_ref()
                    .map(|r| r.repair_cap_per_day())
                    .unwrap_or(3)
            })
            .unwrap_or(3);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let mut budget = match pantheon_scheduler::repair_budget::RepairBudget::open(&self.data_dir)
        {
            Ok(b) => b,
            Err(e) => {
                return HealOutcome::not_fixed(
                    format!("repair budget unavailable, skipping investigation: {e}"),
                    run_id,
                );
            }
        };
        if !budget.try_consume(job_id, max_repairs, now_ms) {
            return HealOutcome::not_fixed(
                format!(
                    "repair budget exhausted: job {job_id} already used its {max_repairs} repair investigations today"
                ),
                run_id,
            );
        }
        let task_text = job.resolve_task(&TemplateStore::open(&self.data_dir));
        let stats = match RunHistory::open(&self.data_dir) {
            Ok(h) => h.stats(job_id).cloned(),
            Err(_) => None,
        };

        // The investigation itself is fallible: a broken model config,
        // a parked-on-approval turn, a panic - all become the not-fixed
        // alert, never a silent skip.
        let verdict = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            (self.investigator)(&InvestigateInput {
                data_dir: &self.data_dir,
                job,
                task_text: &task_text,
                error,
                stats: stats.as_ref(),
            })
        }));
        let verdict = match verdict {
            Ok(v) => v,
            Err(_) => HealVerdict {
                fixed: false,
                summary: "the investigator session panicked.".to_string(),
                run_id: String::new(),
            },
        };

        if !verdict.fixed {
            return HealOutcome::not_fixed(verdict.summary, run_id);
        }

        // Exactly one retry, as a direct call - not through the tick
        // driver - so the retry can never re-enter self-heal.
        let report = (self.retry)(job, &task_text, &self.data_dir);
        let retry_outcome = if report.error.is_none() {
            RunOutcome::Completed
        } else {
            RunOutcome::Failed
        };
        if let Err(e) = self.record(job_id, retry_outcome, report.error.clone()) {
            eprintln!("self-heal: could not record retry outcome for {job_id}: {e}");
        }
        match report.error {
            None => HealOutcome::recovered(verdict.summary, &verdict.run_id),
            Some(retry_error) => HealOutcome::not_fixed(
                format!(
                    "{}. A single retry was attempted and also failed: {}",
                    verdict.summary, retry_error
                ),
                &verdict.run_id,
            ),
        }
    }

    fn alert(&self, job_id: &str, run_id: &str, error: &str, outcome: &HealOutcome) {
        // Prefer the original run id; the investigator's run is the
        // fallback for job types that create no run of their own.
        let rid = pick_run_id(run_id, &outcome.run_id);
        if outcome.recovered {
            let detail = format!(
                "The task failed with: {}. Investigation: {}. The cause was fixed and the retry ran cleanly.",
                truncate(error, 500),
                truncate(&outcome.detail, MAX_DETAIL_CHARS)
            );
            self.append_event(Event::ScheduledTaskRecovered {
                run_id: rid,
                job_id: job_id.to_string(),
                detail,
            });
            eprintln!("self-heal: job {job_id} failed, was fixed, and the retry succeeded");
        } else {
            self.alert_failed(job_id, &rid, error, &outcome.detail);
        }
    }

    fn alert_failed(&self, job_id: &str, run_id: &str, error: &str, detail: &str) {
        let detail = truncate(
            &format!(
                "The task failed with: {}. Investigation: {}",
                truncate(error, 500),
                detail
            ),
            MAX_DETAIL_CHARS,
        );
        self.append_event(Event::ScheduledTaskFailed {
            run_id: run_id.to_string(),
            job_id: job_id.to_string(),
            error: truncate(error, 500),
            detail,
        });
        eprintln!("self-heal: job {job_id} failed and needs attention");
    }

    fn append_event(&self, event: Event) {
        match Ledger::open(&self.data_dir.join("ledger.db")) {
            Ok(ledger) => {
                if let Err(e) = ledger.append(&event) {
                    eprintln!("self-heal: could not append alert event: {e}");
                }
            }
            Err(e) => eprintln!("self-heal: could not open ledger for alert: {e}"),
        }
    }
}

/// Prefer the investigator's run when the original run id is empty
/// (e.g. nightly-pass jobs create no run of their own).
fn pick_run_id(original: &str, investigator_run: &str) -> String {
    if !original.is_empty() {
        original.to_string()
    } else {
        investigator_run.to_string()
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    let mut out: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

/// What one heal attempt produced. `detail` is the human-readable
/// summary that goes into the ledger alert.
struct HealOutcome {
    recovered: bool,
    detail: String,
    /// The run to attach the alert to when the original has no run id.
    run_id: String,
}

impl HealOutcome {
    fn recovered(detail: String, run_id: &str) -> Self {
        Self {
            recovered: true,
            detail,
            run_id: run_id.to_string(),
        }
    }

    fn not_fixed(detail: String, run_id: &str) -> Self {
        Self {
            recovered: false,
            detail,
            run_id: run_id.to_string(),
        }
    }
}

/// The production investigator: a bounded, non-spawning agent session
/// over the failing job's context. Verdict comes from a trailing
/// `VERDICT: FIXED` / `VERDICT: NOT_FIXED` line; anything unparseable
/// or erroring is NOT_FIXED - fail closed.
fn investigate_real(input: &InvestigateInput) -> HealVerdict {
    let file_cfg = config::Config::load_or_report(input.data_dir);
    let model_policy = config::build_scheduled_model_policy(
        file_cfg.as_ref(),
        input.job.provider.clone(),
        input.job.model.clone(),
    );
    let policy = Policy::coder();
    let secrets = config::chat_secrets(file_cfg.as_ref());
    let session = match Session::new(input.data_dir.to_path_buf(), policy, model_policy, secrets) {
        Ok(s) => s,
        Err(e) => {
            return HealVerdict {
                fixed: false,
                summary: format!("could not start the investigator session: {e}"),
                run_id: String::new(),
            };
        }
    };
    config::apply_tool_enablement(&session, file_cfg.as_ref());
    session.set_budget(Budget {
        max_turns: 12,
        max_tool_calls: 24,
        max_tokens: Some(32_000),
        max_delegate_depth: 0,
        allow_child_spawn: false,
    });

    let history_line = match input.stats {
        Some(s) => format!(
            "{} consecutive failure(s); last error: {}; {} total runs, {} total failures",
            s.consecutive_failures,
            s.last_error.as_deref().unwrap_or("none"),
            s.total_runs,
            s.total_failures
        ),
        None => "no run history available".to_string(),
    };
    let prompt = format!(
        "A scheduled task run just failed. Investigate the failure, fix the root cause if you can, and report a verdict.\n\
\n\
Job id: {job_id}\n\
Task (as configured):\n\
{task}\n\
\n\
Failure: {error}\n\
\n\
Recent history: {history}\n\
\n\
Relevant paths (data dir {data_dir}): schedule.json (job definitions), schedule-run-history.json (per-job outcomes). \
The event ledger is queryable through the `pantheon` CLI; do not write it directly.\n\
\n\
Rules:\n\
1. Diagnose before fixing. Only fix the root cause you actually found.\n\
2. You may repair config files, restart MCP servers, or correct schedule state - whatever the diagnosis supports.\n\
3. Do NOT create, modify, pause, or delete any OTHER scheduled jobs. Do NOT start schedulers, daemons, or background services. \
Do NOT approve anything on the user's behalf.\n\
4. End your final message with exactly one line `VERDICT: FIXED` or `VERDICT: NOT_FIXED`, then a short paragraph: \
what failed, what you changed, and what to watch.\n\
\n\
Begin.",
        job_id = input.job.id,
        task = input.task_text,
        error = input.error,
        history = history_line,
        data_dir = input.data_dir.display(),
    );

    let run_id = pantheon_runtime::new_run_id();
    let chat_run_id = run_id.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(session.chat(&chat_run_id, &prompt));
    });
    let outcome = match rx.recv_timeout(INVESTIGATE_TIMEOUT) {
        Ok(o) => o,
        Err(_) => {
            return HealVerdict {
                fixed: false,
                summary: "the investigator session hit the 5-minute wall clock and was abandoned."
                    .to_string(),
                run_id: String::new(),
            };
        }
    };

    match outcome {
        Ok(LoopOutcome::Answered { text, .. }) => {
            let fixed = parse_verdict(&text);
            HealVerdict {
                fixed: fixed.unwrap_or(false),
                run_id: run_id.clone(),
                summary: if fixed.is_some() {
                    verdict_summary(&text)
                } else {
                    format!(
                        "the investigator finished without a parseable verdict; treated as NOT_FIXED. Last words: {}",
                        truncate(&text, 500)
                    )
                },
            }
        }
        Ok(other) => HealVerdict {
            fixed: false,
            summary: format!("the investigator session ended without an answer ({other:?}); treated as NOT_FIXED"),
            run_id: run_id.clone(),
        },
        Err(e) => HealVerdict {
            fixed: false,
            summary: format!("the investigator session errored: {e}"),
            run_id,
        },
    }
}

/// The investigator's verdict: the last `VERDICT: FIXED` /
/// `VERDICT: NOT_FIXED` line wins; nothing found means fail closed.
fn parse_verdict(text: &str) -> Option<bool> {
    text.lines().rev().find_map(|line| match line.trim() {
        "VERDICT: FIXED" => Some(true),
        "VERDICT: NOT_FIXED" => Some(false),
        _ => None,
    })
}

/// The paragraph after the verdict line; the whole text when the
/// verdict is the last line.
fn verdict_summary(text: &str) -> String {
    let mut after = false;
    let mut out = Vec::new();
    for line in text.lines() {
        if after {
            out.push(line);
        } else if matches!(line.trim(), "VERDICT: FIXED" | "VERDICT: NOT_FIXED") {
            after = true;
        }
    }
    let summary = out.join("\n").trim().to_string();
    if summary.is_empty() {
        truncate(text.trim(), 500)
    } else {
        truncate(&summary, 500)
    }
}
