//! Scheduler (spec section 21): durable jobs — cron/one-shot/interval/
//! webhook/conditional/manual — with idempotency keys, missed-run policy,
//! locks, pause/resume. Jobs enqueue durable run requests; execution
//! itself stays in pantheon-runtime.
use serde::{Deserialize, Serialize};

pub mod cron;
pub mod durable;
pub mod idempotency;
pub mod tick;
pub mod webhook;

pub use cron::{civil_from_ms, CronError, CronSchedule};
pub use durable::DurableClaimLedger;
pub use idempotency::{occurrence_key, runs_for_missed, ClaimLedger};
pub use tick::{RunOutcome, TickDecision, TickDriver};
pub use webhook::{
    accept as accept_webhook, route as route_webhook, sign as sign_webhook, verify_signature, Fire,
    SignatureError, WebhookAuth, WebhookReject, SECRET_ENV_VAR, SIGNATURE_HEADER,
};

/// Default ceiling for one job run: 10 minutes. A run that outlives it is
/// abandoned (the tick stops waiting; Rust cannot kill the thread, so the
/// run finishes detached). Override per job with [`Job::timeout_secs`].
pub const DEFAULT_JOB_TIMEOUT_SECS: u64 = 600;

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
    OneShot { at_ms: i64 },
    Interval { every_ms: u64 },
    Webhook { path: String },
    Conditional { expr: String },
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MissedPolicy {
    Skip,
    RunOnce,
    CatchUp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub kind: ScheduleKind,
    pub idempotency_key: String,
    pub missed: MissedPolicy,
    pub paused: bool,
    pub target_agent: String,
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
}

impl Job {
    pub fn new(id: &str, kind: ScheduleKind, target: &str) -> Self {
        Self {
            id: id.into(),
            kind,
            target_agent: target.into(),
            idempotency_key: format!("job:{id}:"),
            paused: false,
            missed: MissedPolicy::RunOnce,
            model: None,
            provider: None,
            timeout_secs: None,
            overlap: OverlapPolicy::default(),
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

    /// What fires this job should resolve the model from: the pin, else the
    /// runtime default the caller passes in.
    pub fn effective_model<'a>(&'a self, default: &'a str) -> &'a str {
        self.model.as_deref().unwrap_or(default)
    }
    /// Next fire decision: pure function of now vs last fire (testable).
    pub fn due(&self, now_ms: i64, last_fire_ms: Option<i64>) -> bool {
        if self.paused {
            return false;
        }
        match &self.kind {
            ScheduleKind::Manual
            | ScheduleKind::Webhook { .. }
            | ScheduleKind::Conditional { .. } => false,
            ScheduleKind::OneShot { at_ms } => now_ms >= *at_ms && last_fire_ms.is_none(),
            ScheduleKind::Interval { every_ms } => match last_fire_ms {
                None => true,
                Some(last) => now_ms.saturating_sub(last) >= *every_ms as i64,
            },
            // A cron job fires on its matching minute, at most once per
            // minute, so a restart inside that minute cannot refire it.
            // An unparseable expression never fires; `validate` reports it
            // at registration instead of silently going quiet.
            ScheduleKind::Cron { expr } => match cron::CronSchedule::parse(expr) {
                Err(_) => false,
                Ok(schedule) => {
                    schedule.matches_ms(now_ms)
                        && last_fire_ms
                            .is_none_or(|last| last.div_euclid(60_000) != now_ms.div_euclid(60_000))
                }
            },
        }
    }

    /// Validate before the job is persisted. A broken expression must be an
    /// error at registration, never a silent no-show at fire time.
    pub fn validate(&self) -> Result<(), cron::CronError> {
        match &self.kind {
            ScheduleKind::Cron { expr } => cron::CronSchedule::validate(expr),
            _ => Ok(()),
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
    /// - interval: the scheduled fire instant (`last + every`), or the
    ///   quantum containing now for a first fire, so racing ticks agree;
    /// - one-shot: its fixed fire time.
    /// Returns `None` for kinds the tick loop never fires.
    pub fn occurrence_stamp(&self, now_ms: i64, last_fire_ms: Option<i64>) -> Option<i64> {
        match &self.kind {
            ScheduleKind::Cron { .. } => Some(now_ms.div_euclid(60_000)),
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
            ScheduleKind::Manual
            | ScheduleKind::Webhook { .. }
            | ScheduleKind::Conditional { .. } => None,
        }
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
