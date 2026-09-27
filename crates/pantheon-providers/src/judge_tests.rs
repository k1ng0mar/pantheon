//! Tests for `pantheon_providers::judge::tests` — sibling file so sources stay test-free.
use super::*;
use pantheon_api::model::DecisionPoint;

fn req(point: DecisionPoint, choices: &[&str]) -> DecisionRequest {
    DecisionRequest {
        run_id: "run_t".into(),
        point,
        query: "q".into(),
        choices: choices.iter().map(|s| s.to_string()).collect(),
        context: Some("ctx".into()),
    }
}

#[test]
fn prompt_carries_options_and_protocol() {
    let r = req(DecisionPoint::RouteSelect, &["provider=a", "provider=b"]);
    let p = prompt_for(&r);
    assert!(p.contains("\"provider=a\""));
    assert!(p.contains("ANSWER <option>"));
    let g = prompt_for(&req(DecisionPoint::ToolGate, &["shell.execute"]));
    assert!(g.contains("ALLOW|DENY|APPROVE"));
    assert!(g.contains("\"shell.execute\""));
}

#[test]
fn gate_parses_verdict_score_confidence() {
    let r = req(DecisionPoint::ToolGate, &["shell.execute"]);
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
fn gate_verdict_in_prose_and_fences() {
    let r = req(DecisionPoint::ToolGate, &["shell.execute"]);
    let a = parse_answer(
        &r,
        "Sure — I checked the args.\n```\nANSWER APPROVE score=0.1 confidence=0.7\n```",
    )
    .unwrap();
    assert!(matches!(
        a,
        DecisionAnswer::Gate {
            verdict: GateVerdict::NeedsApproval { .. },
            ..
        }
    ));
}

#[test]
fn gate_fails_closed_on_gibberish() {
    let r = req(DecisionPoint::ToolGate, &["shell.execute"]);
    let a = parse_answer(&r, "ANSWER MAYBE score=0.5 confidence=0.4").unwrap();
    assert!(matches!(
        a,
        DecisionAnswer::Gate {
            verdict: GateVerdict::NeedsApproval { .. },
            ..
        }
    ));
    let empty = parse_answer(&r, "   \n  ");
    assert!(empty.is_err());
}

#[test]
fn route_matches_canonical_choice_from_shorthand() {
    let r = req(DecisionPoint::RouteSelect, &["provider=default"]);
    // Model answers with just the value after `=`.
    let a = parse_answer(&r, "ANSWER default confidence=0.9").unwrap();
    assert_eq!(
        a,
        DecisionAnswer::Route {
            choice: "provider=default".into(),
            confidence: 0.9
        }
    );
    // And with the full option verbatim.
    let b = parse_answer(&r, "ANSWER provider=default").unwrap();
    assert_eq!(
        b,
        DecisionAnswer::Route {
            choice: "provider=default".into(),
            confidence: 0.0
        }
    );
}

#[test]
fn route_noul_or_mismatch_is_an_error_the_host_can_fall_back_from() {
    let r = req(DecisionPoint::RouteSelect, &["provider=a"]);
    assert!(parse_answer(&r, "ANSWER NOUL confidence=0.2").is_err());
    assert!(parse_answer(&r, "ANSWER something-else").is_err());
}

#[test]
fn verify_yes_no() {
    let r = req(DecisionPoint::TaskVerify, &[]);
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
fn delegate_noul_is_rejection_not_error() {
    let r = req(DecisionPoint::DelegateSelect, &["researcher"]);
    let a = parse_answer(&r, "ANSWER NOUL confidence=0.3").unwrap();
    assert_eq!(
        a,
        DecisionAnswer::Binary {
            accepted: false,
            confidence: 0.3
        }
    );
    let b = parse_answer(&r, "ANSWER researcher").unwrap();
    assert_eq!(
        b,
        DecisionAnswer::Route {
            choice: "researcher".into(),
            confidence: 0.0
        }
    );
}
