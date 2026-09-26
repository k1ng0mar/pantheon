//! Scheduler (spec section 21): durable jobs — cron/one-shot/interval/
//! webhook/conditional/manual — with idempotency keys, missed-run policy,
//! locks, pause/resume. Jobs enqueue durable run requests; execution
//! itself stays in pantheon-runtime.
use serde::{Deserialize, Serialize};

pub mod cron;
pub mod durable;
pub mod idempotency;
pub mod webhook;

pub use cron::{civil_from_ms, CronError, CronSchedule};
pub use durable::DurableClaimLedger;
pub use idempotency::{occurrence_key, runs_for_missed, ClaimLedger};
pub use webhook::{accept as accept_webhook, route as route_webhook, Fire};

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
            ScheduleKind::Cron { expr } => cron::CronSchedule::parse(expr).map(|_| ()),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
