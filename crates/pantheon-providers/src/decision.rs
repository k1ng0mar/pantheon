//! Provider-agnostic decision-model adapter.
//!
//! The decision layer is an auxiliary model the *host* chooses — nothing in
//! here is provider-specific. Config `[decision]` (or the
//! `PANTHEON_DECISION_PROVIDER` / `PANTHEON_DECISION_MODEL` env pair) becomes
//! an `AuxiliaryKind::DecisionRouter` entry in `ModelPolicy`; the client
//! resolves base URL, wire mode, and API key from the core catalog, so any
//! OpenAI-compatible or Anthropic endpoint works — GPT-4o mini, a local
//! llama, Claude Haiku, or a small local classifier such as Laya pointed at
//! its own catalog row.
//!
//! Output protocol (temperature 0, one answer line, prose tolerated):
//!
//! ```text
//! RouteSelect / DelegateSelect:  ANSWER <option>|NOUL [confidence=<0..1>]
//! ToolGate:                      ANSWER <ALLOW|DENY|APPROVE> [score=<0..1>] [confidence=<0..1>]
//! TaskVerify:                    ANSWER <YES|NO> [confidence=<0..1>]
//! ```
//!
//! Parsing fails closed: an unrecognizable gate verdict becomes
//! `NeedsApproval`, never `Allow`. The host validates every answer against
//! live state before acting — confidence is a signal, not permission.

use crate::http::{ChatTransport, HttpTransport};
use crate::{anthropic, openai};
use pantheon_agent::TurnOutcome;
use pantheon_core::catalog::{self, ApiMode};
use pantheon_core::error::{Layer, PantheonError};
use pantheon_core::message::Message;
use pantheon_core::model::{
    DecisionAnswer, DecisionPoint, DecisionRequest, DecisionRouter, DefaultModel, GateVerdict,
};
use pantheon_core::model_event::NoopModelSink;
use pantheon_secrets::SecretValue;
use std::time::Duration;

/// Decision calls sit inline in the agent loop: short, bounded deadline.
pub const DECISION_TIMEOUT_SECS: u64 = 10;
/// Answers are a single label line; generous cap for chatty models.
pub const DECISION_MAX_TOKENS: u32 = 256;

fn derr(code: &str, cause: String, retryable: bool, remediation: &'static str) -> PantheonError {
    PantheonError::new(code, Layer::Provider, retryable, cause, remediation, "")
}

/// Prompt for one decision point: role, options, query, context, plus the
/// exact output protocol `parse_answer` expects.
pub fn prompt_for(req: &DecisionRequest) -> String {
    let choices = if req.choices.is_empty() {
        String::new()
    } else {
        format!(
            "Options: [{}].\n",
            req.choices
                .iter()
                .map(|c| format!("\"{c}\""))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let ctx = req.context.as_deref().unwrap_or("(none)");
    let protocol = match &req.point {
        DecisionPoint::RouteSelect => {
            "You are a routing classifier. Reply with exactly one line:\n\
             `ANSWER <option>` where <option> is copied verbatim from Options,\n\
             or `ANSWER NOUL` if none applies. Optionally append `confidence=<0..1>`."
        }
        DecisionPoint::ToolGate => {
            "You are a risk classifier for tool execution. Reply with exactly one line:\n\
             `ANSWER <ALLOW|DENY|APPROVE> score=<risk 0..1> confidence=<0..1>`.\n\
             DENY for clearly unsafe calls, APPROVE when a human should review."
        }
        DecisionPoint::TaskVerify => {
            "You are a verification classifier. Reply with exactly one line:\n\
             `ANSWER <YES|NO> confidence=<0..1>`."
        }
        DecisionPoint::DelegateSelect => {
            "You are a specialist selector. Reply with exactly one line:\n\
             `ANSWER <option>` where <option> is copied verbatim from Options,\n\
             or `ANSWER NOUL` if none applies. Optionally append `confidence=<0..1>`."
        }
        DecisionPoint::Other(name) => {
            return format!(
                "Decision point: {name}. {choices}Query: {query}\nContext: {ctx}\n\
                 Reply with exactly one line: `ANSWER <option>` or `ANSWER NOUL` \
                 (or YES/NO / ALLOW/DENY/APPROVE for yes-no and gate questions). \
                 Optionally append `confidence=<0..1>`.",
                choices = choices,
                query = req.query,
                ctx = ctx,
            );
        }
    };
    format!("{protocol}\n{choices}Query: {query}\nContext: {ctx}", query = req.query)
}

/// Reduce a model reply to the answer line: strip code fences, prefer a
/// line starting with `ANSWER`, fall back to the last non-empty line.
fn answer_line(raw: &str) -> String {
    let unfenced = raw
        .lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .collect::<Vec<_>>()
        .join("\n");
    let lines: Vec<&str> = unfenced
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    lines
        .iter()
        .rev()
        .find(|l| {
            l.trim_start_matches(['*', '-', ' '])
                .trim_start()
                .trim_start_matches(['`', '"', '\''])
                .trim_start()
                .trim_start_matches(|c: char| c.is_ascii_punctuation() && c != '<')
                .trim_start()
                .trim_start_matches(['`', '"', '\''])
                .eq_ignore_ascii_case("answer")
                || l.trim_start()
                    .trim_start_matches(['`', '"', '\''])
                    .to_ascii_lowercase()
                    .starts_with("answer:")
        })
        .map(|l| l.to_string())
        .unwrap_or_else(|| lines[lines.len() - 1].to_string())
        .trim()
        .trim_start_matches(['`', '"', '\''])
        .trim_end_matches(['`', '"', '\''])
        .trim()
        .to_string()
}

/// `key=<float>` / `key: <float>` anywhere in the text.
fn kv(text: &str, key: &str) -> Option<f32> {
    let lower = text.to_ascii_lowercase();
    let key = key.to_ascii_lowercase();
    let idx = lower.find(&key)?;
    let rest = &text[idx + key.len()..];
    let rest = rest.trim_start().trim_start_matches(['=', ':', ' ']);
    let token: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-' || *c == '+')
        .collect();
    token.parse::<f32>().ok().filter(|v| v.is_finite())
}

/// Every float literal in the text, in order of appearance.
fn floats(text: &str) -> Vec<f32> {
    text.split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
        .filter_map(|t| t.parse::<f32>().ok())
        .filter(|v| v.is_finite())
        .collect()
}

/// Whole-word, case-insensitive presence check.
fn has_word(text: &str, word: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .any(|t| t.eq_ignore_ascii_case(word))
}

/// Match the model's reply against the offered choices. Returns the
/// canonical choice string (host-side validation compares that, not the
/// raw reply). Step 3 handles `provider=default` style choices answered
/// as just `default`.
fn match_choice(text: &str, choices: &[String]) -> Option<String> {
    if choices.is_empty() {
        return None;
    }
    let lower = text.to_ascii_lowercase();
    let line = lower
        .lines()
        .map(|l| l.trim_start_matches(['*', '-', ' ']).trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string();
    for c in choices {
        if line == c.to_ascii_lowercase() {
            return Some(c.clone());
        }
    }
    for c in choices {
        if lower.contains(&c.to_ascii_lowercase()) {
            return Some(c.clone());
        }
    }
    for c in choices {
        if let Some((_, value)) = c.split_once('=') {
            if has_word(&lower, value) {
                return Some(c.clone());
            }
        }
    }
    None
}

/// Gate verdict from the answer line. Fail closed: anything unrecognized
/// escalates to a human instead of allowing the tool.
fn parse_verdict(text: &str) -> GateVerdict {
    if has_word(text, "APPROVE")
        || has_word(text, "APPROVAL")
        || has_word(text, "ESCALATE")
        || has_word(text, "NEEDS_APPROVAL")
    {
        return GateVerdict::NeedsApproval {
            reason: "decision model flagged for approval".to_string(),
        };
    }
    if has_word(text, "DENY")
        || has_word(text, "BLOCK")
        || has_word(text, "REFUSE")
        || has_word(text, "REJECT")
    {
        return GateVerdict::Deny {
            reason: "decision model flagged high risk".to_string(),
        };
    }
    if has_word(text, "ALLOW") || has_word(text, "SAFE") || has_word(text, "PERMIT") {
        return GateVerdict::Allow;
    }
    GateVerdict::NeedsApproval {
        reason: "unrecognized verdict from decision model (fail-closed)".to_string(),
    }
}

/// Parse a model reply into a typed answer for this decision point.
/// Never free text out; unrecognized answers fail conservatively.
pub fn parse_answer(req: &DecisionRequest, raw: &str) -> Result<DecisionAnswer, PantheonError> {
    let line = answer_line(raw);
    if line.is_empty() {
        return Err(derr(
            "DECISION_EMPTY",
            "decision model returned an empty answer".to_string(),
            true,
            "check the [decision] endpoint is healthy",
        ));
    }
    let upper = line.to_ascii_uppercase();
    let confidence = kv(&line, "confidence")
        .or_else(|| floats(&line).last().copied())
        .unwrap_or(0.0)
        .clamp(0.0, 1.0);

    match &req.point {
        DecisionPoint::ToolGate => {
            let score = kv(&line, "score")
                .or_else(|| floats(&line).first().copied())
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);
            Ok(DecisionAnswer::Gate {
                verdict: parse_verdict(&upper),
                confidence,
                score,
            })
        }
        DecisionPoint::TaskVerify => {
            let passed = has_word(&upper, "YES") && !has_word(&upper, "NO");
            Ok(DecisionAnswer::Threshold {
                passed,
                value: confidence,
            })
        }
        DecisionPoint::RouteSelect => {
            if has_word(&upper, "NOUL") {
                return Err(derr(
                    "DECISION_ABSTAIN",
                    "decision model abstained (NOUL)".to_string(),
                    false,
                    "host falls back to the default model",
                ));
            }
            match_choice(&line, &req.choices).map(|choice| DecisionAnswer::Route { choice, confidence }).ok_or_else(|| {
                derr(
                    "DECISION_UNPARSED",
                    format!("decision model answered {line:?}, no offered option matched"),
                    false,
                    "check the decision model follows the ANSWER protocol",
                )
            })
        }
        DecisionPoint::DelegateSelect | DecisionPoint::Other(_) => {
            if has_word(&upper, "NOUL") {
                return Ok(DecisionAnswer::Binary {
                    accepted: false,
                    confidence,
                });
            }
            if req.choices.is_empty() {
                return Ok(DecisionAnswer::Binary {
                    accepted: !has_word(&upper, "NO"),
                    confidence,
                });
            }
            match_choice(&line, &req.choices)
                .map(|choice| DecisionAnswer::Route { choice, confidence })
                .ok_or_else(|| {
                    derr(
                        "DECISION_UNPARSED",
                        format!("decision model answered {line:?}, no offered option matched"),
                        false,
                        "check the decision model follows the ANSWER protocol",
                    )
                })
        }
    }
}

/// A `DecisionRouter` backed by any provider/model the host configured as
/// the `DecisionRouter` auxiliary. Single-shot, non-streaming, short
/// timeout; on failure the engine falls back to host defaults.
pub struct DecisionClient {
    /// Provider + model chosen by the host (config `[decision]` / env).
    pub target: DefaultModel,
    pub transport: Box<dyn ChatTransport>,
    /// Configured key fallback; `catalog::key_for` still prefers the
    /// provider's own key env (e.g. `PANTHEON_KEY_OPENAI`) when set.
    pub api_key: Option<SecretValue>,
    pub max_tokens: u32,
}

impl DecisionClient {
    pub fn new(target: DefaultModel, api_key: Option<SecretValue>) -> Self {
        Self {
            target,
            transport: Box::new(HttpTransport {
                timeout: Duration::from_secs(DECISION_TIMEOUT_SECS),
            }),
            api_key,
            max_tokens: DECISION_MAX_TOKENS,
        }
    }

    /// Test seam: replay a canned response through any transport.
    pub fn with_transport(mut self, transport: Box<dyn ChatTransport>) -> Self {
        self.transport = transport;
        self
    }
}

impl DecisionRouter for DecisionClient {
    fn model_name(&self) -> &str {
        &self.target.model
    }

    fn decide(&self, req: &DecisionRequest) -> Result<DecisionAnswer, PantheonError> {
        let prompt = prompt_for(req);
        let base = catalog::base_url_for(&self.target.provider);
        let api_mode = catalog::provider(&self.target.provider)
            .map(|p| p.api_mode)
            .unwrap_or(ApiMode::OpenAi);
        let configured = self.api_key.as_ref().map(|k| k.expose()).unwrap_or("");
        let key = catalog::key_for(&self.target.provider, configured);
        let messages = vec![Message::user(prompt)];
        let wire = match api_mode {
            ApiMode::OpenAi => {
                openai::request(&base, &key, &self.target.model, &messages, &[], false)
            }
            ApiMode::Anthropic => anthropic::request(
                &base,
                &key,
                &self.target.model,
                &messages,
                &[],
                false,
                self.max_tokens,
            ),
        };
        let turn = match api_mode {
            ApiMode::OpenAi => openai::complete(self.transport.as_ref(), wire, &NoopModelSink),
            ApiMode::Anthropic => anthropic::complete(self.transport.as_ref(), wire, &NoopModelSink),
        }
        .map_err(|e| {
            derr(
                "DECISION_HTTP",
                format!("decision model call failed: {}", e.cause),
                e.retryable,
                "check the [decision] endpoint is reachable within the timeout",
            )
        })?;
        match turn.outcome {
            TurnOutcome::Text { text, .. } => parse_answer(req, &text),
            _ => Err(derr(
                "DECISION_NOT_TEXT",
                "decision model returned a non-text turn".to_string(),
                false,
                "decision models must answer with plain text",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pantheon_core::model::DecisionPoint;

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
            DecisionAnswer::Gate { verdict, score, confidence } => {
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
            DecisionAnswer::Gate { verdict: GateVerdict::NeedsApproval { .. }, .. }
        ));
    }

    #[test]
    fn gate_fails_closed_on_gibberish() {
        let r = req(DecisionPoint::ToolGate, &["shell.execute"]);
        let a = parse_answer(&r, "ANSWER MAYBE score=0.5 confidence=0.4").unwrap();
        assert!(matches!(
            a,
            DecisionAnswer::Gate { verdict: GateVerdict::NeedsApproval { .. }, .. }
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
            DecisionAnswer::Route { choice: "provider=default".into(), confidence: 0.9 }
        );
        // And with the full option verbatim.
        let b = parse_answer(&r, "ANSWER provider=default").unwrap();
        assert_eq!(
            b,
            DecisionAnswer::Route { choice: "provider=default".into(), confidence: 0.0 }
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
        assert_eq!(a, DecisionAnswer::Threshold { passed: true, value: 0.95 });
        let b = parse_answer(&r, "ANSWER NO").unwrap();
        assert_eq!(b, DecisionAnswer::Threshold { passed: false, value: 0.0 });
    }

    #[test]
    fn delegate_noul_is_rejection_not_error() {
        let r = req(DecisionPoint::DelegateSelect, &["researcher"]);
        let a = parse_answer(&r, "ANSWER NOUL confidence=0.3").unwrap();
        assert_eq!(
            a,
            DecisionAnswer::Binary { accepted: false, confidence: 0.3 }
        );
        let b = parse_answer(&r, "ANSWER researcher").unwrap();
        assert_eq!(b, DecisionAnswer::Route { choice: "researcher".into(), confidence: 0.0 });
    }
}
