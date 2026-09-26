//! Tests for `pantheon_scheduler::webhook::tests` — sibling file so sources stay test-free.
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
