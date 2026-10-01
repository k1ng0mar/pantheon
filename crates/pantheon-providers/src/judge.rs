//! Provider-agnostic judge-model adapter.
//!
//! The judge layer is an auxiliary model the *host* chooses — nothing in
//! here is provider-specific. Config `[judge]` (or the
//! `PANTHEON_JUDGE_PROVIDER` / `PANTHEON_JUDGE_MODEL` env pair) becomes
//! an `AuxiliaryKind::Judge` entry in `ModelPolicy`; the client
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

use crate::http::{aux_complete, aux_request, aux_transport, resolve_aux_wire, ChatTransport};
use pantheon_agent::TurnOutcome;
use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::model::{
    DecisionAnswer, DecisionPoint, DecisionRequest, DefaultModel, GateVerdict, Judge,
};
use pantheon_secrets::SecretValue;

/// Judge calls sit inline in the agent loop: short, bounded deadline.
pub const JUDGE_TIMEOUT_SECS: u64 = 10;
/// Answers are a single label line; generous cap for chatty models.
pub const JUDGE_MAX_TOKENS: u32 = 256;

fn derr(code: &str, cause: String, retryable: bool, remediation: &'static str) -> PantheonError {
    PantheonError::new(code, Layer::Provider, retryable, cause, remediation, "")
}

/// Prompt for one judge point: role, options, query, context, plus the
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
    format!(
        "{protocol}\n{choices}Query: {query}\nContext: {ctx}",
        query = req.query
    )
}

/// Reduce a model reply to the answer line: strip code fences, prefer a
/// line starting with `ANSWER`, fall back to the last non-empty line.
///
/// Matcher (frozen, do not "simplify"): this is the STRICT matcher — it
/// only fires on a bare `ANSWER` line (optionally wrapped in bullets,
/// quotes, or punctuation) or an `answer:`-prefixed line. A plain
/// `ANSWER <payload>` line is NOT specially preferred; it wins only via
/// the last-line fallback, per the protocol's one-line-answer contract.
/// The verifier's `ANSWER`-prefix matcher in `verify.rs` differs
/// deliberately; see `crate::answer_line` for why both are kept.
fn answer_line(raw: &str) -> String {
    let lines = crate::answer_line::non_empty_unfenced_lines(raw);
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
            reason: "judge model flagged for approval".to_string(),
        };
    }
    if has_word(text, "DENY")
        || has_word(text, "BLOCK")
        || has_word(text, "REFUSE")
        || has_word(text, "REJECT")
    {
        return GateVerdict::Deny {
            reason: "judge model flagged high risk".to_string(),
        };
    }
    if has_word(text, "ALLOW") || has_word(text, "SAFE") || has_word(text, "PERMIT") {
        return GateVerdict::Allow;
    }
    GateVerdict::NeedsApproval {
        reason: "unrecognized verdict from judge model (fail-closed)".to_string(),
    }
}

/// Parse a model reply into a typed answer for this judge point.
/// Never free text out; unrecognized answers fail conservatively.
pub fn parse_answer(req: &DecisionRequest, raw: &str) -> Result<DecisionAnswer, PantheonError> {
    let line = answer_line(raw);
    if line.is_empty() {
        return Err(derr(
            "JUDGE_EMPTY",
            "judge model returned an empty answer".to_string(),
            true,
            "check the [judge] endpoint is healthy",
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
                    "JUDGE_ABSTAIN",
                    "judge model abstained (NOUL)".to_string(),
                    false,
                    "host falls back to the default model",
                ));
            }
            match_choice(&line, &req.choices)
                .map(|choice| DecisionAnswer::Route { choice, confidence })
                .ok_or_else(|| {
                    derr(
                        "JUDGE_UNPARSED",
                        format!("judge model answered {line:?}, no offered option matched"),
                        false,
                        "check the judge model follows the ANSWER protocol",
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
                        "JUDGE_UNPARSED",
                        format!("judge model answered {line:?}, no offered option matched"),
                        false,
                        "check the judge model follows the ANSWER protocol",
                    )
                })
        }
    }
}

/// A `Judge` backed by any provider/model the host configured as
/// the `Judge` auxiliary. Single-shot, non-streaming, short
/// timeout; on failure the engine falls back to host defaults.
pub struct JudgeClient {
    /// Provider + model chosen by the host (config `[judge]` / env).
    pub target: DefaultModel,
    pub transport: Box<dyn ChatTransport>,
    /// Configured key fallback; `catalog::key_for` still prefers the
    /// provider's own key env (e.g. `PANTHEON_KEY_OPENAI`) when set.
    pub api_key: Option<SecretValue>,
    pub max_tokens: u32,
}

impl JudgeClient {
    pub fn new(target: DefaultModel, api_key: Option<SecretValue>) -> Self {
        Self {
            target,
            transport: aux_transport(JUDGE_TIMEOUT_SECS),
            api_key,
            max_tokens: JUDGE_MAX_TOKENS,
        }
    }

    /// Test seam: replay a canned response through any transport.
    pub fn with_transport(mut self, transport: Box<dyn ChatTransport>) -> Self {
        self.transport = transport;
        self
    }

    /// Override the aux request timeout (seconds), e.g. from the
    /// aux section's `timeout_secs`. Rebuilds the transport; call
    /// before `with_transport` if you also inject a test transport.
    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        self.transport = aux_transport(secs.max(1));
        self
    }
}

impl Judge for JudgeClient {
    fn model_name(&self) -> &str {
        &self.target.model
    }

    fn decide(&self, req: &DecisionRequest) -> Result<DecisionAnswer, PantheonError> {
        let prompt = prompt_for(req);
        let configured = self.api_key.as_ref().map(|k| k.expose()).unwrap_or("");
        let wire = resolve_aux_wire(&self.target.provider, configured, self.max_tokens)?;
        let request = aux_request(&wire, &self.target.model, prompt);
        let turn = aux_complete(self.transport.as_ref(), &wire, request).map_err(|e| {
            derr(
                "JUDGE_HTTP",
                format!("judge model call failed: {}", e.cause),
                e.retryable,
                "check the [judge] endpoint is reachable within the timeout",
            )
        })?;
        match turn.outcome {
            TurnOutcome::Text { text, .. } => parse_answer(req, &text),
            _ => Err(derr(
                "JUDGE_NOT_TEXT",
                "judge model returned a non-text turn".to_string(),
                false,
                "judge models must answer with plain text",
            )),
        }
    }
}
