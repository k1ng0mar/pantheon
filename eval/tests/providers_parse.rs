//! Parsing invariants for the provider plane.
//!
//! Every parser below turns a model/provider reply into a typed value, and
//! every one must fail closed: an unrecognizable reply escalates (judge
//! gate), degrades (verifier), or errors — it never silently passes as a
//! success. These are the distilled, public-API-only versions of the
//! in-crate parse tests.

use pantheon_api::model::{
    DecisionAnswer, DecisionPoint, DecisionRequest, GateVerdict, TITLE_MAX_CHARS,
};
use pantheon_providers::distill::parse_facts;
use pantheon_providers::error_kind::{
    classify_provider_error, display_message, retry_after_secs_from_cause, short_snippet,
    ProviderErrorKind,
};
use pantheon_providers::http::{parse_retry_after, MAX_RETRY_AFTER_SECS};
use pantheon_providers::judge::parse_answer;
use pantheon_providers::title::bound_model_title;
use pantheon_providers::verify::{parse_verdict, VerifyVerdict};
use pantheon_providers::video::parse_native_response;

fn judge_req(point: DecisionPoint, choices: &[&str]) -> DecisionRequest {
    DecisionRequest {
        run_id: "run_t".into(),
        point,
        query: "q".into(),
        choices: choices.iter().map(|s| s.to_string()).collect(),
        context: Some("ctx".into()),
    }
}

#[test]
fn judge_gate_fails_closed_on_unrecognized_verdict() {
    let r = judge_req(DecisionPoint::ToolGate, &["shell.execute"]);
    // Gibberish verdict: escalate, never allow.
    let a = parse_answer(&r, "ANSWER MAYBE score=0.5 confidence=0.4").unwrap();
    assert!(matches!(
        a,
        DecisionAnswer::Gate {
            verdict: GateVerdict::NeedsApproval { .. },
            ..
        }
    ));
    // Empty reply is an error, not a verdict.
    assert!(parse_answer(&r, "   \n  ").is_err());
}

#[test]
fn judge_gate_parses_verdict_score_and_confidence() {
    let r = judge_req(DecisionPoint::ToolGate, &["shell.execute"]);
    let a = parse_answer(&r, "ANSWER DENY score=0.9 confidence=0.8").unwrap();
    match a {
        DecisionAnswer::Gate {
            verdict,
            score,
            confidence,
        } => {
            assert!(matches!(verdict, GateVerdict::Deny { .. }));
            assert!((score - 0.9).abs() < 1e-6);
            assert!((confidence - 0.8).abs() < 1e-6);
        }
        other => panic!("wrong answer: {other:?}"),
    }
}

#[test]
fn judge_route_matches_choice_and_rejects_abstention() {
    let r = judge_req(DecisionPoint::RouteSelect, &["provider=default"]);
    // Model answers with just the value after `=`: still the canonical choice.
    let a = parse_answer(&r, "ANSWER default confidence=0.9").unwrap();
    assert_eq!(
        a,
        DecisionAnswer::Route {
            choice: "provider=default".into(),
            confidence: 0.9
        }
    );
    // Abstention and mismatch are errors the host can fall back from.
    assert!(parse_answer(&r, "ANSWER NOUL confidence=0.2").is_err());
    assert!(parse_answer(&r, "ANSWER something-else").is_err());
}

#[test]
fn judge_task_verify_yes_no() {
    let r = judge_req(DecisionPoint::TaskVerify, &[]);
    let a = parse_answer(&r, "ANSWER YES confidence=0.95").unwrap();
    assert_eq!(
        a,
        DecisionAnswer::Threshold {
            passed: true,
            value: 0.95
        }
    );
    let b = parse_answer(&r, "ANSWER NO").unwrap();
    assert_eq!(
        b,
        DecisionAnswer::Threshold {
            passed: false,
            value: 0.0
        }
    );
}

#[test]
fn verify_verdict_fails_closed() {
    let v = parse_verdict("ANSWER HOLDS confidence=0.9 reason=evidence matches");
    assert!(matches!(v, VerifyVerdict::Holds { .. }));
    assert!(v.verified());

    let v = parse_verdict(
        "Some thinking...\nANSWER FALSIFIED confidence=0.8 reason=claim mentions tests, none ran",
    );
    match &v {
        VerifyVerdict::Falsified { reason } => assert!(reason.contains("none ran")),
        other => panic!("expected Falsified, got {other:?}"),
    }
    assert!(!v.verified());

    let v = parse_verdict("ANSWER INCONCLUSIVE confidence=0.4 reason=no evidence either way");
    assert!(!v.verified());

    // Not HOLDS, not FALSIFIED, not INCONCLUSIVE -> Inconclusive, never Holds.
    for raw in [
        "looks good to me!",
        "",
        "ANSWER MAYBE confidence=0.9",
        "```\n{\"status\": \"completed\"}\n```",
    ] {
        let v = parse_verdict(raw);
        assert!(
            matches!(v, VerifyVerdict::Inconclusive { .. }),
            "expected Inconclusive for {raw:?}, got {v:?}"
        );
        assert!(!v.verified());
    }
}

#[test]
fn verify_answer_line_wins_and_bad_confidence_defaults() {
    // The ANSWER line beats the prose around it.
    let v = parse_verdict(
        "The claim looks plausible at first glance.\nANSWER FALSIFIED reason=dates contradict the goal",
    );
    assert!(matches!(v, VerifyVerdict::Falsified { .. }));

    // Out-of-range confidence is dropped to the 0.5 default, not clamped.
    let v = parse_verdict("ANSWER HOLDS confidence=7 reason=ok");
    match v {
        VerifyVerdict::Holds { confidence } => assert_eq!(confidence, 0.5),
        other => panic!("expected Holds, got {other:?}"),
    }
}

#[test]
fn distill_parse_facts_skips_blank_lines() {
    let facts = parse_facts("one\n\n  \ntwo\n");
    assert_eq!(facts, vec!["one".to_string(), "two".to_string()]);
}

#[test]
fn video_parse_native_response_concatenates_parts_and_surfaces_blocks() {
    let body =
        r#"{"candidates":[{"content":{"parts":[{"text":"a person "},{"text":"walks in"}]}}]}"#;
    assert_eq!(parse_native_response(body).unwrap(), "a person walks in");

    let blocked = r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#;
    let e = parse_native_response(blocked).unwrap_err();
    assert_eq!(e.code, "VIDEO_NATIVE_BLOCKED");
    assert!(e.cause.contains("SAFETY"));

    let empty = r#"{"candidates":[]}"#;
    let e = parse_native_response(empty).unwrap_err();
    assert_eq!(e.code, "VIDEO_NATIVE_EMPTY");

    let bad = parse_native_response("not json").unwrap_err();
    assert_eq!(bad.code, "VIDEO_NATIVE_PARSE");
}

#[test]
fn retry_after_parses_delta_seconds_and_rejects_garbage() {
    assert_eq!(parse_retry_after("5"), Some(5));
    assert_eq!(parse_retry_after("  7  "), Some(7));
    assert_eq!(parse_retry_after("0"), Some(0));
    // The cap: a provider asking for two minutes gets sixty seconds.
    assert_eq!(parse_retry_after("120"), Some(MAX_RETRY_AFTER_SECS));
    assert_eq!(parse_retry_after("99999"), Some(MAX_RETRY_AFTER_SECS));
    // Garbage is not a wait.
    for raw in ["", "soon", "-3", "1.5", "Wed, 32 Foo 2099 00:00:00 GMT"] {
        assert_eq!(parse_retry_after(raw), None, "expected None for {raw:?}");
    }
}

#[test]
fn retry_after_http_date_is_capped_not_a_multiyear_sleep() {
    // Long past: no wait.
    assert_eq!(parse_retry_after("Sun, 06 Nov 1994 08:49:37 GMT"), Some(0));
    // Far future: capped.
    assert_eq!(
        parse_retry_after("Wed, 01 Jan 2099 00:00:00 GMT"),
        Some(MAX_RETRY_AFTER_SECS)
    );
    // The marker `send()` stamps on a 429 cause round-trips.
    assert_eq!(
        retry_after_secs_from_cause("HTTP 429 slow (retry-after: 12s)"),
        Some(12)
    );
    assert_eq!(retry_after_secs_from_cause("HTTP 429 slow"), None);
}

#[test]
fn provider_error_classification_and_display() {
    let c = |cause: &str| classify_provider_error("PROVIDER_HTTP", cause);
    assert_eq!(
        c("https://x/v1: HTTP 429 slow down (retry-after: 12s)"),
        ProviderErrorKind::RateLimited
    );
    // 429 must not be swallowed by the generic 4xx class.
    assert!(c("https://x/v1: HTTP 429 nope").is_rate_limited());
    assert_eq!(
        c("https://x/v1: HTTP 401 {\"error\":\"bad key\"}"),
        ProviderErrorKind::Auth
    );
    assert_eq!(c("https://x/v1: HTTP 500 boom"), ProviderErrorKind::Server);
    assert_eq!(
        c("https://x/v1: HTTP 400 bad request"),
        ProviderErrorKind::Client
    );
    assert_eq!(
        c("https://x/v1: Network Error: timed out reading response"),
        ProviderErrorKind::Timeout,
        "timeout wins over the generic network shape"
    );
    assert_eq!(
        classify_provider_error("PROVIDER_CONFIG", "missing key"),
        ProviderErrorKind::Client
    );
    assert_eq!(
        classify_provider_error("PROVIDER_PARSE", "bad json"),
        ProviderErrorKind::Unknown
    );
    // Rate limits render in the warning tone, not the failure tone.
    assert_eq!(ProviderErrorKind::RateLimited.label(), "rate limited");
    assert_ne!(
        ProviderErrorKind::RateLimited.label(),
        ProviderErrorKind::Server.label()
    );
    // Display helpers strip transport noise.
    assert_eq!(
        display_message("https://x/v1: HTTP 429 slow down (retry-after: 12s)"),
        "HTTP 429 slow down (retry-after: 12s)"
    );
    assert_eq!(short_snippet("a\nb", 10), "a");
    assert_eq!(short_snippet("abcdef", 4), "abcd");
}

#[test]
fn title_bound_model_title_normalizes_noise() {
    assert_eq!(bound_model_title("\"Ship v2\""), "Ship v2");
    assert_eq!(bound_model_title("Title:  Login fix\nignored"), "Login fix");
    let long = "y".repeat(500);
    assert_eq!(bound_model_title(&long).chars().count(), TITLE_MAX_CHARS);
    assert_eq!(bound_model_title("   \n  "), "");
}
