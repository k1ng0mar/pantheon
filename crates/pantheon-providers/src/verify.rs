//! Adversarial post-delegation verifier.
//!
//! After a delegated sub-agent completes, the `Verify` auxiliary takes the
//! task's goal plus the child's claimed result, assumes the goal was
//! missed, and tries to falsify the claim from the evidence. This is the
//! gsd-core idea Pantheon's aux set was missing: every other auxiliary
//! compresses, summarizes, or classifies — none of them *falsifies*.
//!
//! Output protocol (temperature 0, one answer line, prose tolerated):
//!
//! ```text
//! ANSWER <HOLDS|FALSIFIED|INCONCLUSIVE> [confidence=<0..1>] [reason=<short>]
//! ```
//!
//! Parsing fails closed: an unrecognizable verdict becomes `Inconclusive`,
//! never `Holds`. The host treats `Falsified` as a failed delegation and
//! `Inconclusive` as unverified — neither counts as done.
//!
//! The verifier is read-only by construction: single-shot, non-streaming,
//! no tools, no ledger writes. It sees the goal, the claim, and the
//! evidence the child reported — never the parent's live session.

use crate::http::{aux_complete, aux_request, aux_transport, resolve_aux_wire, ChatTransport};
use pantheon_agent::TurnOutcome;
use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::model::{AuxiliaryKind, DefaultModel, ModelPolicy};
use pantheon_secrets::SecretValue;

/// Verify calls sit inline in delegation completion: short, bounded deadline.
pub const VERIFY_TIMEOUT_SECS: u64 = 30;
/// Verdicts are a single label line; generous cap for chatty models.
pub const VERIFY_MAX_TOKENS: u32 = 256;

fn verr(code: &str, cause: String, retryable: bool, remediation: &'static str) -> PantheonError {
    PantheonError::new(code, Layer::Provider, retryable, cause, remediation, "")
}

/// What the host asks the verifier to adjudicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyRequest {
    /// The delegated task: what the child was asked to achieve.
    pub goal: String,
    /// The child's claimed result (usually the envelope summary).
    pub claim: String,
    /// Supporting evidence the child reported (files changed, decisions).
    /// May be empty; the verifier must say INCONCLUSIVE rather than
    /// invent evidence when there is nothing to check against.
    pub evidence: String,
}

/// The verifier's verdict. Fail-closed by construction: only an explicit
/// `HOLDS` answer counts as verified.
#[derive(Debug, Clone, PartialEq)]
pub enum VerifyVerdict {
    /// The claim survived the falsification attempt.
    Holds { confidence: f32 },
    /// Concrete evidence the claim is false.
    Falsified { reason: String },
    /// Could not decide — treated as unverified, never as done.
    Inconclusive { reason: String },
}

impl VerifyVerdict {
    /// True only for an explicit HOLDS. Everything else — falsified,
    /// inconclusive, or a child that never returned an envelope — must
    /// not count as a completed delegation.
    pub fn verified(&self) -> bool {
        matches!(self, VerifyVerdict::Holds { .. })
    }
}

/// The adversarial prompt: start from "the goal was missed" and make the
/// model earn its way back to HOLDS. Evidence-free claims cannot hold.
pub fn prompt_for(req: &VerifyRequest) -> String {
    format!(
        "You are an adversarial verifier. A sub-agent was given this goal:\n\
         \n\
         GOAL:\n{goal}\n\
         \n\
         The sub-agent claims this result:\n\
         \n\
         CLAIM:\n{claim}\n\
         \n\
         Evidence it reported:\n\
         \n\
         EVIDENCE:\n{evidence}\n\
         \n\
         Assume the goal was NOT achieved. Your job is to falsify the claim: \
         look for concrete gaps between the goal and the claim, missing \
         evidence, contradictions, or work the claim asserts but the evidence \
         does not support. Do not give the benefit of the doubt.\n\
         \n\
         Reply with exactly one line:\n\
         `ANSWER <HOLDS|FALSIFIED|INCONCLUSIVE> confidence=<0..1> reason=<short reason>`\n\
         \n\
         - HOLDS: the evidence concretely supports the claim against the goal.\n\
         - FALSIFIED: you found a concrete gap or contradiction (name it in reason).\n\
         - INCONCLUSIVE: there is not enough evidence to decide either way. \
         When in doubt, choose INCONCLUSIVE — an unverified claim is safer than a wrong HOLDS.",
        goal = req.goal.trim(),
        claim = req.claim.trim(),
        evidence = if req.evidence.trim().is_empty() {
            "(none reported)".to_string()
        } else {
            req.evidence.trim().to_string()
        },
    )
}

/// Reduce a model reply to the answer line: strip code fences, prefer a
/// line starting with `ANSWER`, fall back to the last non-empty line.
///
/// Matcher (frozen, do not "simplify"): this is the `ANSWER`-PREFIX
/// matcher — it prefers the last line that starts with `ANSWER`
/// (case-insensitive, so `ANSWERED ...` also matches). The judge's strict
/// punctuation-tolerant matcher in `judge.rs` differs deliberately; see
/// `crate::answer_line` for why both are kept.
fn answer_line(raw: &str) -> String {
    let lines = crate::answer_line::non_empty_unfenced_lines(raw);
    if lines.is_empty() {
        return String::new();
    }
    lines
        .iter()
        .rev()
        .find(|l| l.to_ascii_uppercase().starts_with("ANSWER"))
        .or_else(|| lines.last())
        .map(|l| l.as_str())
        .unwrap_or("")
        .to_string()
}

/// Parse `confidence=<0..1>` from the answer line; absent or malformed = 0.5.
fn parse_confidence(line: &str) -> f32 {
    line.split_whitespace()
        .find_map(|tok| {
            tok.strip_prefix("confidence=").and_then(|v| {
                v.trim_matches(|c| c == '"' || c == '\'')
                    .parse::<f32>()
                    .ok()
            })
        })
        .filter(|c| (0.0..=1.0).contains(c))
        .unwrap_or(0.5)
}

/// Parse `reason=<...>` (rest of line) from the answer line.
fn parse_reason(line: &str) -> String {
    line.split_once("reason=")
        .map(|(_, r)| r.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
        .unwrap_or_default()
}

/// Parse the verdict line. Fails closed: anything unrecognizable is
/// `Inconclusive`, never `Holds`.
pub fn parse_verdict(raw: &str) -> VerifyVerdict {
    let line = answer_line(raw);
    let upper = line.to_ascii_uppercase();
    let confidence = parse_confidence(&line);
    let reason = parse_reason(&line);
    if upper.contains("FALSIFIED") {
        let reason = if reason.is_empty() {
            "verifier reported FALSIFIED without a reason".to_string()
        } else {
            reason
        };
        VerifyVerdict::Falsified { reason }
    } else if upper.contains("HOLDS") {
        VerifyVerdict::Holds { confidence }
    } else if upper.contains("INCONCLUSIVE") {
        VerifyVerdict::Inconclusive {
            reason: if reason.is_empty() {
                "verifier reported INCONCLUSIVE".to_string()
            } else {
                reason
            },
        }
    } else {
        VerifyVerdict::Inconclusive {
            reason: format!("unparseable verifier reply: {line:?}"),
        }
    }
}

/// A `VerifyClient` backed by whatever provider/model the host configured
/// as the `Verify` auxiliary. Single-shot, non-streaming, short timeout;
/// read-only by construction (no tools, no writes).
pub struct VerifyClient {
    /// Provider + model chosen by the host (config `[verify]` / env).
    pub target: DefaultModel,
    pub transport: Box<dyn ChatTransport>,
    /// Configured key fallback; `catalog::key_for` still prefers the
    /// provider's own key env when set.
    pub api_key: Option<SecretValue>,
    pub max_tokens: u32,
}

impl VerifyClient {
    /// Build from the policy: `AuxiliaryKind::Verify` entry present ->
    /// verifier on; absent -> `None` (the slot is OFF by default).
    pub fn from_policy(policy: &ModelPolicy, api_key: Option<SecretValue>) -> Option<Self> {
        let aux = policy.auxiliary(&AuxiliaryKind::Verify)?;
        Some(Self {
            target: DefaultModel {
                provider: aux.provider.clone(),
                model: aux.model.clone(),
            },
            transport: aux_transport(aux.timeout_secs.max(1)),
            api_key,
            max_tokens: VERIFY_MAX_TOKENS,
        })
    }

    /// Test seam: replay a canned response through any transport.
    pub fn with_transport(mut self, transport: Box<dyn ChatTransport>) -> Self {
        self.transport = transport;
        self
    }

    /// Run one falsification attempt. Fails closed: transport errors
    /// surface as `Err` (the host treats them as unverified), and an
    /// unparseable answer becomes `Inconclusive`, never `Holds`.
    pub fn verify(&self, req: &VerifyRequest) -> Result<VerifyVerdict, PantheonError> {
        let prompt = prompt_for(req);
        let configured = self.api_key.as_ref().map(|k| k.expose()).unwrap_or("");
        let wire = resolve_aux_wire(&self.target.provider, configured, self.max_tokens)?;
        let request = aux_request(&wire, &self.target.model, prompt);
        let turn = aux_complete(self.transport.as_ref(), &wire, request).map_err(|e| {
            verr(
                "VERIFY_HTTP",
                format!("verifier model call failed: {}", e.cause),
                e.retryable,
                "check the [verify] endpoint is reachable within the timeout",
            )
        })?;
        match turn.outcome {
            TurnOutcome::Text { text, .. } => Ok(parse_verdict(&text)),
            _ => Err(verr(
                "VERIFY_NOT_TEXT",
                "verifier model returned a non-text turn".to_string(),
                false,
                "verifier models must answer with plain text",
            )),
        }
    }
}
