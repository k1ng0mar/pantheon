//! Inbound webhook triggers (§21).
//!
//! A webhook job does not fire on a clock — a surface calls in. So these jobs
//! are deliberately never "due": the trigger path identifies the job, and the
//! caller's request id is the occurrence. A caller that retries a delivery
//! (which webhook senders do on any non-2xx) collapses onto the run already
//! claimed for that request instead of starting a second one.

use crate::idempotency::occurrence_key;
use crate::{ClaimLedger, Job, ScheduleKind};

/// An accepted webhook request: the caller should enqueue this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fire {
    pub job_id: String,
    /// Key for the idempotency ledger, derived from the request id.
    pub occurrence_key: String,
    pub target_agent: String,
}

/// Compare a configured path with a request path, ignoring surrounding and
/// duplicate slashes so `/hook/nyx`, `hook/nyx` and `//hook/nyx/` agree.
fn same_path(configured: &str, requested: &str) -> bool {
    fn normalize(p: &str) -> Vec<&str> {
        p.split('/').filter(|s| !s.is_empty()).collect()
    }
    normalize(configured) == normalize(requested)
}

/// Which job a request path belongs to. First match wins, so a duplicate
/// path registered twice is resolved deterministically by registration order.
pub fn route<'a>(jobs: &'a [Job], path: &str) -> Option<&'a Job> {
    jobs.iter().find(|job| match &job.kind {
        ScheduleKind::Webhook { path: configured } => same_path(configured, path),
        _ => false,
    })
}

/// Accept an inbound webhook request for one job.
///
/// Returns `None` when the path does not match this job, the job is paused, or
/// the request id has already been claimed (a retried delivery).
pub fn accept(job: &Job, path: &str, request_id: &str, ledger: &mut ClaimLedger) -> Option<Fire> {
    let configured = match &job.kind {
        ScheduleKind::Webhook { path } => path,
        _ => return None,
    };
    if job.paused || !same_path(configured, path) {
        return None;
    }
    let key = occurrence_key(job, request_id);
    if !ledger.claim(&key) {
        return None;
    }
    Some(Fire {
        job_id: job.id.clone(),
        occurrence_key: key,
        target_agent: job.target_agent.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(id: &str, path: &str) -> Job {
        Job::new(id, ScheduleKind::Webhook { path: path.into() }, "nyx")
    }

    #[test]
    fn a_matching_request_fires_the_job() {
        let job = hook("deploy-hook", "hook/deploy");
        let mut ledger = ClaimLedger::new();
        let fire = accept(&job, "hook/deploy", "req-1", &mut ledger).expect("path matches");
        assert_eq!(fire.job_id, "deploy-hook");
        assert_eq!(fire.target_agent, "nyx");
        assert_eq!(fire.occurrence_key, "job:deploy-hook:req-1");
    }

    #[test]
    fn a_path_prefix_is_not_a_match() {
        let job = hook("deploy-hook", "hook/deploy");
        let mut ledger = ClaimLedger::new();
        assert!(accept(&job, "hook", "req-1", &mut ledger).is_none());
        assert!(accept(&job, "hook/deploy/extra", "req-1", &mut ledger).is_none());
        assert!(
            ledger.is_empty(),
            "a rejected request must not consume an occurrence"
        );
    }

    #[test]
    fn slash_variants_are_the_same_path() {
        let job = hook("deploy-hook", "/hook/deploy/");
        let mut ledger = ClaimLedger::new();
        assert!(accept(&job, "/hook/deploy", "req-1", &mut ledger).is_some());
    }

    #[test]
    fn a_retried_delivery_does_not_run_twice() {
        let job = hook("deploy-hook", "hook/deploy");
        let mut ledger = ClaimLedger::new();
        assert!(accept(&job, "hook/deploy", "req-1", &mut ledger).is_some());
        assert!(
            accept(&job, "hook/deploy", "req-1", &mut ledger).is_none(),
            "the sender's retry must collapse onto the claimed run"
        );
        // A genuinely new request still runs.
        assert!(accept(&job, "hook/deploy", "req-2", &mut ledger).is_some());
    }

    #[test]
    fn paused_jobs_accept_nothing() {
        let mut job = hook("deploy-hook", "hook/deploy");
        job.paused = true;
        let mut ledger = ClaimLedger::new();
        assert!(accept(&job, "hook/deploy", "req-1", &mut ledger).is_none());
    }

    #[test]
    fn clock_jobs_are_never_webhook_targets() {
        let job = Job::new(
            "nightly",
            ScheduleKind::Cron {
                expr: "0 3 * * *".into(),
            },
            "nyx",
        );
        let mut ledger = ClaimLedger::new();
        assert!(accept(&job, "hook/deploy", "req-1", &mut ledger).is_none());

        let jobs = vec![job, hook("deploy-hook", "hook/deploy")];
        let routed = route(&jobs, "hook/deploy").expect("the webhook job matches");
        assert_eq!(routed.id, "deploy-hook");
        assert!(route(&jobs, "hook/other").is_none());
    }
}
