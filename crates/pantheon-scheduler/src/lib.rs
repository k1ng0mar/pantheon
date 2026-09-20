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
pub enum ScheduleKind { Cron { expr: String }, OneShot { at_ms: i64 }, Interval { every_ms: u64 }, Webhook { path: String }, Conditional { expr: String }, Manual }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MissedPolicy { Skip, RunOnce, CatchUp }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub kind: ScheduleKind,
    pub idempotency_key: String,
    pub missed: MissedPolicy,
    pub paused: bool,
    pub target_agent: String,
}

impl Job {
    pub fn new(id: &str, kind: ScheduleKind, target: &str) -> Self {
        Self { id: id.into(), kind, target_agent: target.into(),
            idempotency_key: format!("job:{id}:"), paused: false, missed: MissedPolicy::RunOnce }
    }
    /// Next fire decision: pure function of now vs last fire (testable).
    pub fn due(&self, now_ms: i64, last_fire_ms: Option<i64>) -> bool {
        if self.paused { return false; }
        match &self.kind {
            ScheduleKind::Manual | ScheduleKind::Webhook { .. } | ScheduleKind::Conditional { .. } => false,
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
                        && last_fire_ms.map_or(true, |last| {
                            last.div_euclid(60_000) != now_ms.div_euclid(60_000)
                        })
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
mod tests {
    use super::*;
    #[test]
    fn interval_and_oneshot() {
        let j = Job::new("a", ScheduleKind::Interval { every_ms: 1000 }, "nyx");
        assert!(j.due(5000, None));
        assert!(!j.due(5500, Some(5000)));
        assert!(j.due(6000, Some(5000)));
        let o = Job::new("b", ScheduleKind::OneShot { at_ms: 100 }, "nyx");
        assert!(o.due(200, None));
        assert!(!o.due(200, Some(150)));
    }
}
