//! Scheduler (spec section 21): durable jobs - cron, one-shot, and
//! interval - with idempotency keys, missed-occurrence catch-up, locks,
//! pause/resume. Jobs enqueue durable run requests; execution itself stays
//! in pantheon-runtime.
//!
//! Jobs persist in `<data_dir>/schedule.json` via [`load_jobs`] /
//! [`save_jobs`]; [`TemplateStore`] manages the prompt templates a job's
//! task is rendered from at fire time (see [`templates`]).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub mod cron;
pub mod durable;
pub mod repair_budget;
pub mod retention;
pub mod run_history;
pub mod templates;
pub mod tick;
pub mod webhook;

pub use cron::{normalize_cron_expr, CronError, CronSchedule};
pub use durable::DurableClaimLedger;
pub use run_history::{run_history_path, JobRunStats, RunHistory};
pub use templates::{
    apply_defaults, builtin_templates, expand_template, is_reserved_var, render_prompt,
    ScheduleTemplate, TemplateSchedule, TemplateStore, TemplateVar,
};
pub use tick::{occurrence_key, ClaimLedger, PausedCheck, RunOutcome, TickDecision, TickDriver};
pub use webhook::{
    accept as accept_webhook, route as route_webhook, sign as sign_webhook, verify_signature, Fire,
    SignatureError, WebhookAuth, WebhookReject, SECRET_ENV_VAR, SIGNATURE_HEADER,
};

/// Default ceiling for one job run: 10 minutes. A run that outlives it is
/// abandoned (the tick stops waiting; Rust cannot kill the thread, so the
/// run finishes detached). Override per job with [`Job::timeout_secs`].
pub const DEFAULT_JOB_TIMEOUT_SECS: u64 = 600;

/// Parse a duration string like "30m", "6h", "1d" into milliseconds.
/// Units are `ms`, `s`, `m`, `h`, `d`, matched longest-suffix-first so
/// `ms` wins over `s`. Case-insensitive ("1H" works); surrounding
/// whitespace is ignored.
///
/// A zero duration is never a valid schedule: `Interval { every_ms: 0 }`
/// reads as "due constantly" while producing no occurrence stamp, so the
/// job would sit "due now" forever without ever firing - reject it here,
/// at parse time, not at 3am.
pub fn parse_duration(s: &str) -> Result<u64, String> {
    let s = s.trim().to_lowercase();
    // Longest suffix first: "ms" must win over "s", and slicing a char
    // boundary that ends_with already matched is safe.
    let (num, unit) = if let Some(n) = s.strip_suffix("ms") {
        (n, 1)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1000)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60_000)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3_600_000)
    } else if let Some(n) = s.strip_suffix('d') {
        (n, 86_400_000)
    } else {
        return Err(format!("unknown unit in {s} (use ms, s, m, h, d)"));
    };
    let n: u64 = num.parse().map_err(|_| format!("not a number: {num}"))?;
    if n == 0 {
        return Err(format!("duration must be greater than zero: {s}"));
    }
    // Fail closed, never panic on a huge-but-parseable number.
    n.checked_mul(unit)
        .ok_or_else(|| format!("duration too large: {s}"))
}

/// What a tick does when it finds the job still running from an earlier
/// fire. `Skip` (the default) drops the fire and logs it; `Replace`
/// abandons the in-flight run and starts a fresh one; `Queue` runs once
/// more after the in-flight run finishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OverlapPolicy {
    #[default]
    Skip,
    Replace,
    Queue,
}

impl std::str::FromStr for OverlapPolicy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "skip" => Ok(Self::Skip),
            "replace" => Ok(Self::Replace),
            "queue" => Ok(Self::Queue),
            other => Err(format!(
                "unknown overlap policy '{other}' (use skip, replace, or queue)"
            )),
        }
    }
}

impl std::fmt::Display for OverlapPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Skip => "skip",
            Self::Replace => "replace",
            Self::Queue => "queue",
        };
        write!(f, "{s}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScheduleKind {
    Cron { expr: String },
    Interval { every_ms: u64 },
    OneShot { at_ms: i64 },
    Webhook { path: String },
}

fn default_agent() -> String {
    "nyx".to_string()
}

fn default_catch_up() -> bool {
    true
}

/// Marker task for scheduled nightly jobs. `pantheon schedule nightly`
/// (and the dashboard job list) tag the unified nightly pass with this
/// task string; whoever fires the job intercepts it and runs the pass
/// instead of a chat turn. Status surfaces (`/nightly status`,
/// `GET /api/nightly/status`) scan the job store for this marker to
/// report the next scheduled run.
pub const NIGHTLY_TASK_MARKER: &str = "__pantheon_nightly__";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub kind: ScheduleKind,
    /// Rendered prompt snapshot, moved in from the clients at creation
    /// time. Fallback when no template is set or the template's render
    /// fails (see [`Job::resolve_task`]).
    pub task: String,
    /// Template name for re-render at fire time. `None` means the task
    /// snapshot is used as-is.
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub template_vars: HashMap<String, String>,
    #[serde(default = "default_agent")]
    pub agent: String,
    pub idempotency_key: String,
    /// Catch up a missed cron occurrence on the next tick instead of
    /// dropping it. Default: true.
    #[serde(default = "default_catch_up")]
    pub catch_up: bool,
    #[serde(default)]
    pub paused: bool,
    /// Hermes jobs carry a per-job model (+ provider snapshot): a scheduled
    /// digest can run on a cheap model while the interactive default stays
    /// big. `None` means "inherit the runtime default at fire time".
    /// `Some` is honored by whoever fires the job (the CLI's `run` path
    /// records it; end-to-end live driving is still open, §21).
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    /// Per-job run ceiling in seconds. `None` (the default) means
    /// [`DEFAULT_JOB_TIMEOUT_SECS`]. A non-positive value is treated as
    /// unset rather than as "abandon immediately".
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// What a tick does when the job is still running. Default: skip.
    #[serde(default)]
    pub overlap: OverlapPolicy,
    /// Where the job's result goes after the run completes (`log` |
    /// `telegram` | `discord` | `notify` | `file:<path>`). `None` = `log`,
    /// today's behavior. Interpreted by the delivery layer, never by the
    /// tick driver itself.
    #[serde(default)]
    pub deliver: Option<String>,
}

impl Job {
    pub fn new(id: &str, kind: ScheduleKind, agent: &str) -> Self {
        Self {
            id: id.into(),
            kind,
            task: String::new(),
            template: None,
            template_vars: HashMap::new(),
            agent: agent.into(),
            idempotency_key: format!("job:{id}:"),
            catch_up: true,
            paused: false,
            model: None,
            provider: None,
            timeout_secs: None,
            overlap: OverlapPolicy::default(),
            deliver: None,
        }
    }

    /// Pin this job to a model (and optionally a provider). Blank pins are
    /// rejected: an empty string would read as "pinned" while behaving as
    /// "inherit", which is exactly the silent-default confusion Hermes'
    /// `provider_snapshot` exists to avoid.
    pub fn pin_model(&mut self, model: &str, provider: Option<&str>) -> Result<(), String> {
        let m = model.trim();
        if m.is_empty() {
            return Err("job model pin cannot be blank (omit --model to inherit)".into());
        }
        if let Some(p) = provider {
            if p.trim().is_empty() {
                return Err("job provider pin cannot be blank (omit --provider to inherit)".into());
            }
            self.provider = Some(p.trim().to_string());
        }
        self.model = Some(m.to_string());
        Ok(())
    }

    /// The prompt to run: the template re-rendered at fire time when the
    /// job names one that still exists, else the stored task snapshot.
    /// Template defaults fill vars the job did not set; a render failure
    /// falls back to the snapshot rather than failing the fire.
    pub fn resolve_task(&self, store: &TemplateStore) -> String {
        match self.template.as_deref().and_then(|name| store.get(name)) {
            Some(template) => {
                let mut vars = self.template_vars.clone();
                apply_defaults(template, &mut vars);
                render_prompt(template, &vars).unwrap_or_else(|_| self.task.clone())
            }
            None => self.task.clone(),
        }
    }

    /// Next fire decision: pure function of now vs last fire (testable).
    pub fn due(&self, now_ms: i64, last_fire_ms: Option<i64>) -> bool {
        if self.paused {
            return false;
        }
        match &self.kind {
            ScheduleKind::OneShot { at_ms } => now_ms >= *at_ms && last_fire_ms.is_none(),
            // Webhook jobs fire on inbound calls, never on the tick.
            ScheduleKind::Webhook { .. } => false,
            ScheduleKind::Interval { every_ms } => match last_fire_ms {
                // every_ms == 0 is rejected at parse time, but rows can
                // arrive via the dashboard API or templates: a zero
                // interval must never read as "due constantly".
                None => *every_ms > 0,
                Some(last) => *every_ms > 0 && now_ms.saturating_sub(last) >= *every_ms as i64,
            },
            // A cron job fires on its matching minute, at most once per
            // minute, so a restart inside that minute cannot refire it.
            // With `catch_up`, a missed occurrence (the host was down, the
            // tick was late) fires once on the next tick instead of being
            // dropped. An unparseable expression never fires; `validate`
            // reports it at registration instead of silently going quiet.
            ScheduleKind::Cron { expr } => {
                let Ok(schedule) = cron::CronSchedule::parse(expr) else {
                    return false;
                };
                let this_bucket = now_ms.div_euclid(60_000);
                if last_fire_ms.is_some_and(|last| last.div_euclid(60_000) == this_bucket) {
                    return false;
                }
                if schedule.matches_ms(now_ms) {
                    return true;
                }
                self.missed_catchup_ms(now_ms, last_fire_ms).is_some()
            }
        }
    }

    /// The missed occurrence (in ms) the catch-up arm of [`Job::due`]
    /// would fire for, or `None` when the job is not cron, catch-up is
    /// off, the fire is natural, or nothing was missed.
    fn missed_catchup_ms(&self, now_ms: i64, last_fire_ms: Option<i64>) -> Option<i64> {
        let ScheduleKind::Cron { expr } = &self.kind else {
            return None;
        };
        if self.paused || !self.catch_up {
            return None;
        }
        let schedule = cron::CronSchedule::parse(expr).ok()?;
        let this_bucket = now_ms.div_euclid(60_000);
        if last_fire_ms.is_some_and(|last| last.div_euclid(60_000) == this_bucket) {
            return None;
        }
        if schedule.matches_ms(now_ms) {
            return None; // natural fire, not a catch-up
        }
        let prev = schedule.prev_fire_ms(now_ms)?;
        let last = last_fire_ms.unwrap_or(i64::MIN);
        (prev.div_euclid(60_000) > last.div_euclid(60_000)).then_some(prev)
    }

    /// Validate before the job is persisted. A broken expression must be an
    /// error at registration, never a silent no-show at fire time.
    pub fn validate(&self) -> Result<(), cron::CronError> {
        match &self.kind {
            ScheduleKind::Cron { expr } => cron::CronSchedule::validate(expr),
            _ => Ok(()),
        }
    }

    /// Next scheduled fire after `now_ms`, or `None` for paused jobs.
    /// Pure function of the job and the clock, for status displays and
    /// tests.
    ///
    /// Cron times are UTC (see [`cron`]). The forward scan is bounded to one
    /// year of minutes; an expression that never matches in that window
    /// (e.g. February 30th) reports no next fire rather than scanning
    /// forever.
    pub fn next_fire_ms(&self, now_ms: i64, last_fire_ms: Option<i64>) -> Option<i64> {
        if self.paused {
            return None;
        }
        match &self.kind {
            ScheduleKind::OneShot { at_ms } => {
                (last_fire_ms.is_none() && *at_ms > now_ms).then_some(*at_ms)
            }
            // No clock fire time: webhooks arrive from outside.
            ScheduleKind::Webhook { .. } => None,
            ScheduleKind::Interval { every_ms } => {
                // A zero interval is invalid (rejected at parse time, but
                // reachable via the API): report no future fire rather
                // than "due now" forever.
                if *every_ms == 0 {
                    return None;
                }
                let next = match last_fire_ms {
                    None => now_ms, // never fired: due immediately
                    Some(last) => last.saturating_add(*every_ms as i64),
                };
                Some(next.max(now_ms))
            }
            ScheduleKind::Cron { expr } => {
                let schedule = cron::CronSchedule::parse(expr).ok()?;
                // Start at the next minute boundary: the current minute's
                // fire (if any) is "now", handled by `due`.
                let mut t = (now_ms.div_euclid(60_000) + 1) * 60_000;
                for _ in 0..525_600 {
                    if schedule.matches_ms(t) {
                        return Some(t);
                    }
                    t += 60_000;
                }
                None
            }
        }
    }

    /// Effective run ceiling in seconds: the per-job setting, or the
    /// default when unset (or set to 0, which would otherwise abandon
    /// every run the instant it started).
    pub fn effective_timeout_secs(&self) -> u64 {
        self.timeout_secs
            .filter(|s| *s > 0)
            .unwrap_or(DEFAULT_JOB_TIMEOUT_SECS)
    }

    /// Stamp identifying this fire's occurrence for the claim ledger.
    ///
    /// Two ticks racing the same due fire must compute the same stamp so
    /// their claims collapse onto one key and exactly one of them runs:
    /// - cron: the minute bucket (a cron job fires at most once a minute);
    ///   a catch-up fire stamps the *missed* bucket, so a crash-replay
    ///   claims the same key instead of firing twice;
    /// - interval: the scheduled fire instant (`last + every`), or the
    ///   quantum containing now for a first fire, so racing ticks agree;
    /// - one-shot: its fixed fire time.
    pub fn occurrence_stamp(&self, now_ms: i64, last_fire_ms: Option<i64>) -> Option<i64> {
        match &self.kind {
            ScheduleKind::Cron { .. } => Some(
                self.missed_catchup_ms(now_ms, last_fire_ms)
                    .map(|prev| prev.div_euclid(60_000))
                    .unwrap_or_else(|| now_ms.div_euclid(60_000)),
            ),
            ScheduleKind::OneShot { at_ms } => Some(*at_ms),
            ScheduleKind::Interval { every_ms } => {
                let every = *every_ms as i64;
                if every <= 0 {
                    return None;
                }
                Some(match last_fire_ms {
                    None => now_ms.div_euclid(every) * every,
                    Some(last) => last.saturating_add(every),
                })
            }
            // Webhook occurrences are keyed by the caller's request id, not
            // by a clock stamp.
            ScheduleKind::Webhook { .. } => None,
        }
    }
}

/// A persisted job: the job plus its last fire instant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledJob {
    pub job: Job,
    #[serde(default)]
    pub last_run: Option<i64>,
}

/// Path of the job store: `<data_dir>/schedule.json`.
pub fn job_store_path(data_dir: &Path) -> PathBuf {
    data_dir.join("schedule.json")
}

/// Advisory lock guarding the job store. Every [`load_jobs`] takes a
/// shared lock and every [`save_jobs`] / [`update_jobs`] an exclusive
/// one, so the tick loop, the CLI, and the dashboard (separate
/// processes) serialize instead of tearing or silently clobbering each
/// other's writes.
fn job_store_lock_path(data_dir: &Path) -> PathBuf {
    data_dir.join("schedule.lock")
}

fn lock_store(data_dir: &Path, exclusive: bool) -> Result<std::fs::File, String> {
    std::fs::create_dir_all(data_dir)
        .map_err(|e| format!("cannot create {}: {e}", data_dir.display()))?;
    let path = job_store_lock_path(data_dir);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    use fs2::FileExt as _;
    if exclusive {
        file.lock_exclusive()
    } else {
        file.lock_shared()
    }
    .map_err(|e| format!("cannot lock {}: {e}", path.display()))?;
    Ok(file)
}

/// Load jobs, returning skipped-legacy-row warnings alongside. The
/// warnings name rows dropped because their trigger kind was removed
/// (webhook/conditional/manual) so a caller with a UI - `schedule list`,
/// the dashboard - can show them instead of letting jobs vanish silently.
pub fn load_jobs_with_warnings(
    data_dir: &Path,
) -> (Result<Vec<ScheduledJob>, String>, Vec<String>) {
    let _lock = match lock_store(data_dir, false) {
        Ok(l) => l,
        Err(e) => return (Err(e), Vec::new()),
    };
    let path = job_store_path(data_dir);
    let text = match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Ok(vec![]), Vec::new()),
        Err(e) => {
            return (
                Err(format!("cannot read {}: {e}", path.display())),
                Vec::new(),
            )
        }
        Ok(text) => text,
    };
    if let Ok(jobs) = serde_json::from_str::<Vec<ScheduledJob>>(&text) {
        return (Ok(jobs), Vec::new());
    }
    let legacy: Vec<LegacyStoredJob> = match serde_json::from_str(&text)
        .map_err(|e| format!("cannot parse {}: {e}", path.display()))
    {
        Ok(l) => l,
        Err(e) => return (Err(e), Vec::new()),
    };
    let mut jobs = Vec::with_capacity(legacy.len());
    let mut warnings = Vec::new();
    for row in legacy {
        if let Some(job) = row.into_scheduled_job(&mut warnings) {
            jobs.push(job);
        }
    }
    (Ok(jobs), warnings)
}

/// Load jobs from `<data_dir>/schedule.json`; see
/// [`load_jobs_with_warnings`] for the migration rules. Skipped-row
/// warnings are dropped here - use the `_with_warnings` variant when a
/// human will see the result.
pub fn load_jobs(data_dir: &Path) -> Result<Vec<ScheduledJob>, String> {
    load_jobs_with_warnings(data_dir).0
}

/// Save jobs to `<data_dir>/schedule.json` as pretty JSON, creating the
/// data dir when needed.
///
/// The write holds the store's exclusive lock and lands via temp file +
/// atomic rename, so a concurrent reader never sees a torn file and two
/// writers serialize. Note this is still last-writer-wins across the
/// load/mutate/save gap: callers that must not clobber a concurrent edit
/// should use [`update_jobs`], which holds the lock across all three steps.
pub fn save_jobs(data_dir: &Path, jobs: &[ScheduledJob]) -> Result<(), String> {
    let _lock = lock_store(data_dir, true)?;
    let path = job_store_path(data_dir);
    let text =
        serde_json::to_string_pretty(jobs).map_err(|e| format!("cannot serialize jobs: {e}"))?;
    // Crash-safe landing: a crash mid-write leaves the previous intact
    // file rather than a torn one. The pid suffix keeps a crashed
    // writer's temp file from colliding with the next writer's.
    let tmp = data_dir.join(format!("schedule.json.tmp.{}", std::process::id()));
    std::fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
}

/// Load the job store, apply `f` to it, and save the result, holding the
/// store's exclusive lock across all three steps. This is the race-free
/// primitive for tick-loop bookkeeping (`record_fires`) and any other
/// read-modify-write that must not clobber a concurrent dashboard/CLI
/// edit made between its load and its save.
pub fn update_jobs<T>(
    data_dir: &Path,
    f: impl FnOnce(&mut Vec<ScheduledJob>) -> Result<T, String>,
) -> Result<T, String> {
    let _lock = lock_store(data_dir, true)?;
    let path = job_store_path(data_dir);
    let mut jobs: Vec<ScheduledJob> = match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        Ok(text) => {
            if let Ok(jobs) = serde_json::from_str::<Vec<ScheduledJob>>(&text) {
                jobs
            } else {
                let mut warnings = Vec::new();
                let legacy: Vec<LegacyStoredJob> = serde_json::from_str(&text)
                    .map_err(|e| format!("cannot parse {}: {e}", path.display()))?;
                let mut jobs = Vec::with_capacity(legacy.len());
                for row in legacy {
                    if let Some(job) = row.into_scheduled_job(&mut warnings) {
                        jobs.push(job);
                    }
                }
                for w in warnings {
                    eprintln!("schedule: {w}");
                }
                jobs
            }
        }
    };
    let out = f(&mut jobs)?;
    let text =
        serde_json::to_string_pretty(&jobs).map_err(|e| format!("cannot serialize jobs: {e}"))?;
    let tmp = data_dir.join(format!("schedule.json.tmp.{}", std::process::id()));
    std::fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("cannot replace {}: {e}", path.display()))?;
    Ok(out)
}

/// Post-success store mutation for a manual `schedule run <id>`: one-shot
/// jobs are removed (single occurrence, now fired); recurring jobs keep
/// their row with `last_run` refreshed. A failed run never reaches here
/// the job stays for retry. Pure on the job list; the caller saves.
pub fn after_manual_run(jobs: &mut Vec<ScheduledJob>, id: &str, now_ms: i64) {
    if let Some(pos) = jobs.iter().position(|j| j.job.id == id) {
        if matches!(jobs[pos].job.kind, ScheduleKind::OneShot { .. }) {
            jobs.remove(pos);
        } else {
            jobs[pos].last_run = Some(now_ms);
        }
    }
}

/// A schedule.json row from before the rework: flat fields, `missed` policy
/// instead of `catch_up`, `target_agent` instead of `agent`, and the six
/// old schedule kinds.
#[derive(Debug, Deserialize)]
struct LegacyStoredJob {
    id: String,
    task: String,
    kind: LegacyKind,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    missed: Option<MissedPolicy>,
    #[serde(default)]
    paused: bool,
    #[serde(default)]
    last_run: Option<i64>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    overlap: OverlapPolicy,
    #[serde(default)]
    deliver: Option<String>,
}

/// Missed-occurrence policy: how many runs a slept-through window enqueues.
/// Kept as a real policy type (not just a legacy shim): the idempotency
/// layer decides catch-up counts from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum MissedPolicy {
    Skip,
    RunOnce,
    CatchUp,
}

/// The old six-variant `ScheduleKind`, kept only for parsing legacy rows.
/// The three surviving variants must keep the old serialization shape so
/// legacy rows parse; the three removed ones are skipped, never loaded.
#[derive(Debug, Deserialize)]
enum LegacyKind {
    Cron { expr: String },
    Interval { every_ms: u64 },
    OneShot { at_ms: i64 },
    Webhook { path: String },
    Conditional { expr: String },
    Manual,
}

impl LegacyStoredJob {
    /// Convert to the new shape. Returns `None` for rows whose kind no
    /// longer exists, pushing a human-readable reason into `warnings`
    /// (surfaced by `schedule list` and the dashboard) instead of
    /// logging to stderr, where a dashboard user would never see it.
    fn into_scheduled_job(self, warnings: &mut Vec<String>) -> Option<ScheduledJob> {
        let kind = match self.kind {
            LegacyKind::Cron { expr } => ScheduleKind::Cron { expr },
            LegacyKind::Interval { every_ms } => ScheduleKind::Interval { every_ms },
            LegacyKind::OneShot { at_ms } => ScheduleKind::OneShot { at_ms },
            LegacyKind::Webhook { path } => ScheduleKind::Webhook { path },
            LegacyKind::Conditional { expr } => {
                warnings.push(format!(
                    "ignoring legacy job '{}': conditional triggers were removed (expr {expr:?})",
                    self.id
                ));
                return None;
            }
            LegacyKind::Manual => {
                warnings.push(format!(
                    "ignoring legacy job '{}': manual triggers were removed",
                    self.id
                ));
                return None;
            }
        };
        Some(ScheduledJob {
            job: Job {
                id: self.id.clone(),
                kind,
                task: self.task,
                template: None,
                template_vars: HashMap::new(),
                agent: self.agent.unwrap_or_else(default_agent),
                idempotency_key: format!("job:{}:", self.id),
                catch_up: self.missed.is_none_or(|m| m != MissedPolicy::Skip),
                paused: self.paused,
                model: self.model,
                provider: self.provider,
                timeout_secs: self.timeout_secs,
                overlap: self.overlap,
                deliver: self.deliver,
            },
            last_run: self.last_run,
        })
    }
}
