//! Tests for `pantheon_scheduler::webhook::tests` — sibling file so sources stay test-free.
use super::*;

const SECRET: &[u8] = b"test-webhook-secret";

fn hook(id: &str, path: &str) -> Job {
    Job::new(id, ScheduleKind::Webhook { path: path.into() }, "nyx")
}

fn auth() -> WebhookAuth {
    WebhookAuth::new(SECRET).expect("non-empty test secret")
}

/// A correctly signed request.
fn signed(
    job: &Job,
    path: &str,
    request_id: &str,
    body: &[u8],
    ledger: &mut ClaimLedger,
) -> Result<Fire, WebhookReject> {
    let sig = sign(SECRET, body);
    accept(job, path, request_id, body, Some(&sig), &auth(), ledger)
}

// ---------------------------------------------------------------------------
// signature verification
// ---------------------------------------------------------------------------

#[test]
fn a_valid_signature_is_accepted() {
    let job = hook("deploy-hook", "hook/deploy");
    let mut ledger = ClaimLedger::new();
    let fire = signed(
        &job,
        "hook/deploy",
        "req-1",
        b"{\"ref\":\"main\"}",
        &mut ledger,
    )
    .expect("valid signature");
    assert_eq!(fire.job_id, "deploy-hook");
    assert_eq!(fire.target_agent, "nyx");
    assert_eq!(fire.occurrence_key, "job:deploy-hook:req-1");
}

#[test]
fn a_missing_signature_is_rejected_with_401() {
    let job = hook("deploy-hook", "hook/deploy");
    let mut ledger = ClaimLedger::new();
    let err = accept(
        &job,
        "hook/deploy",
        "req-1",
        b"{}",
        None,
        &auth(),
        &mut ledger,
    )
    .expect_err("missing signature must not trigger");
    assert_eq!(err, WebhookReject::Unauthorized(SignatureError::Missing));
    assert_eq!(err.http_status(), 401);
    assert!(
        ledger.is_empty(),
        "a rejected request must not consume an occurrence"
    );
}

#[test]
fn a_wrong_secret_is_rejected() {
    let job = hook("deploy-hook", "hook/deploy");
    let mut ledger = ClaimLedger::new();
    let sig = sign(b"some-other-secret", b"{}");
    let err = accept(
        &job,
        "hook/deploy",
        "req-1",
        b"{}",
        Some(&sig),
        &auth(),
        &mut ledger,
    )
    .expect_err("wrong secret must not trigger");
    assert_eq!(err, WebhookReject::Unauthorized(SignatureError::Mismatch));
    assert_eq!(err.http_status(), 401);
}

#[test]
fn a_tampered_body_is_rejected() {
    let job = hook("deploy-hook", "hook/deploy");
    let mut ledger = ClaimLedger::new();
    let sig = sign(SECRET, b"{\"ref\":\"main\"}");
    // The signature was minted over a different body.
    let err = accept(
        &job,
        "hook/deploy",
        "req-1",
        b"{\"ref\":\"evil\"}",
        Some(&sig),
        &auth(),
        &mut ledger,
    )
    .expect_err("tampered body must not trigger");
    assert_eq!(err, WebhookReject::Unauthorized(SignatureError::Mismatch));
}

#[test]
fn malformed_signatures_are_rejected() {
    let job = hook("deploy-hook", "hook/deploy");
    for bad in [
        "sha256=zzzz",
        "sha256=abc",
        "md5=d41d8cd98f00b204e9800998ecf8427e",
        "sha256=",
        "sha256=" as &str,
    ] {
        let mut ledger = ClaimLedger::new();
        let err = accept(
            &job,
            "hook/deploy",
            "req-1",
            b"{}",
            Some(bad),
            &auth(),
            &mut ledger,
        )
        .expect_err("malformed signature must not trigger");
        assert_eq!(
            err,
            WebhookReject::Unauthorized(SignatureError::Malformed),
            "header {bad:?}"
        );
    }
    // Uppercase hex is still hex: it must verify like lowercase.
    let mut ledger = ClaimLedger::new();
    let upper = sign(SECRET, b"{}")
        .replace("sha256=", "sha256=")
        .to_ascii_uppercase();
    let upper = upper.replacen("SHA256=", "sha256=", 1);
    assert!(
        accept(
            &job,
            "hook/deploy",
            "req-9",
            b"{}",
            Some(&upper),
            &auth(),
            &mut ledger
        )
        .is_ok(),
        "uppercase hex digest must verify"
    );
}

#[test]
fn sign_matches_rfc_4231_test_vector() {
    // RFC 4231 test case 1: key = 20 x 0x0b, data = "Hi There".
    let key = [0x0bu8; 20];
    assert_eq!(
        sign(&key, b"Hi There"),
        "sha256=b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
    );
}

#[test]
fn an_empty_secret_builds_no_auth() {
    assert!(WebhookAuth::new(b"").is_none());
    assert!(WebhookAuth::new(vec![]).is_none());
    assert!(WebhookAuth::new(b"x").is_some());
}

#[test]
fn auth_roundtrips_through_the_env_var() {
    let prev = std::env::var(SECRET_ENV_VAR).ok();
    std::env::set_var(SECRET_ENV_VAR, "env-secret-123");
    let a = WebhookAuth::from_env().expect("env secret");
    let sig = sign(b"env-secret-123", b"{}");
    assert!(verify_signature(b"env-secret-123", b"{}", Some(&sig)).is_ok());
    assert!(a.key() == b"env-secret-123");
    match prev {
        Some(v) => std::env::set_var(SECRET_ENV_VAR, v),
        None => std::env::remove_var(SECRET_ENV_VAR),
    }
}

#[test]
fn auth_from_env_is_none_when_unset_or_empty() {
    let prev = std::env::var(SECRET_ENV_VAR).ok();
    std::env::remove_var(SECRET_ENV_VAR);
    assert!(WebhookAuth::from_env().is_none());
    std::env::set_var(SECRET_ENV_VAR, "");
    assert!(WebhookAuth::from_env().is_none());
    match prev {
        Some(v) => std::env::set_var(SECRET_ENV_VAR, v),
        None => std::env::remove_var(SECRET_ENV_VAR),
    }
}

// ---------------------------------------------------------------------------
// routing / claiming (behind a valid signature)
// ---------------------------------------------------------------------------

#[test]
fn a_path_prefix_is_not_a_match() {
    let job = hook("deploy-hook", "hook/deploy");
    let mut ledger = ClaimLedger::new();
    assert_eq!(
        signed(&job, "hook", "req-1", b"{}", &mut ledger).unwrap_err(),
        WebhookReject::NoRoute
    );
    assert_eq!(
        signed(&job, "hook/deploy/extra", "req-1", b"{}", &mut ledger).unwrap_err(),
        WebhookReject::NoRoute
    );
    assert!(
        ledger.is_empty(),
        "a rejected request must not consume an occurrence"
    );
}

#[test]
fn slash_variants_are_the_same_path() {
    let job = hook("deploy-hook", "/hook/deploy/");
    let mut ledger = ClaimLedger::new();
    assert!(signed(&job, "/hook/deploy", "req-1", b"{}", &mut ledger).is_ok());
}

#[test]
fn a_retried_delivery_does_not_run_twice() {
    let job = hook("deploy-hook", "hook/deploy");
    let mut ledger = ClaimLedger::new();
    assert!(signed(&job, "hook/deploy", "req-1", b"{}", &mut ledger).is_ok());
    assert_eq!(
        signed(&job, "hook/deploy", "req-1", b"{}", &mut ledger).unwrap_err(),
        WebhookReject::AlreadyClaimed,
        "the sender's retry must collapse onto the claimed run"
    );
    // A genuinely new request still runs.
    assert!(signed(&job, "hook/deploy", "req-2", b"{}", &mut ledger).is_ok());
}

#[test]
fn paused_jobs_accept_nothing() {
    let mut job = hook("deploy-hook", "hook/deploy");
    job.paused = true;
    let mut ledger = ClaimLedger::new();
    assert_eq!(
        signed(&job, "hook/deploy", "req-1", b"{}", &mut ledger).unwrap_err(),
        WebhookReject::Paused
    );
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
    assert_eq!(
        signed(&job, "hook/deploy", "req-1", b"{}", &mut ledger).unwrap_err(),
        WebhookReject::NotWebhookJob
    );

    let jobs = vec![job, hook("deploy-hook", "hook/deploy")];
    let routed = route(&jobs, "hook/deploy").expect("the webhook job matches");
    assert_eq!(routed.id, "deploy-hook");
    assert!(route(&jobs, "hook/other").is_none());
}

#[test]
fn reject_statuses_map_to_http() {
    assert_eq!(
        WebhookReject::Unauthorized(SignatureError::Missing).http_status(),
        401
    );
    assert_eq!(WebhookReject::NoRoute.http_status(), 404);
    assert_eq!(WebhookReject::Paused.http_status(), 404);
    assert_eq!(WebhookReject::NotWebhookJob.http_status(), 404);
    assert_eq!(WebhookReject::AlreadyClaimed.http_status(), 200);
}
