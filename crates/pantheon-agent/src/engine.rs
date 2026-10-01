//! The loop itself. Pure orchestration: no model SDK, no process spawning.
//!
//! The canonical runtime path is `Session::drive` in `pantheon-runtime`
//! (typed messages, provenance, real provider chain).
use crate::tool::{EventSink, ToolRunner};
use pantheon_api::capability::{Capability, Policy};
use pantheon_api::error::PantheonError;
use pantheon_api::events::Event;

/// One tool the model asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub name: String,
    /// Capability the call requires, decided by the tool registry.
    pub capability: Capability,
    /// Raw arguments (already serialized by the caller).
    pub args: String,
}

/// What one model turn produced.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnOutcome {
    /// Plain text response.
    Text {
        text: String,
        /// Tokens consumed in this turn (input + output).
        tokens: u32,
        /// Cost in US cents for this turn (0 if provider doesn't report).
        cost_cents: u32,
    },
    /// One or more tool calls to execute.
    Tools {
        calls: Vec<ToolCall>,
        /// Tokens consumed for the turn that produced these calls.
        tokens: u32,
        cost_cents: u32,
    },
    /// Delegate to a specialist sub-agent.
    Delegate {
        agent: String,
        model: String,
        task: String,
    },
}

impl TurnOutcome {
    /// Total tokens consumed this turn.
    pub fn tokens(&self) -> u32 {
        match self {
            TurnOutcome::Text { tokens, .. } => *tokens,
            TurnOutcome::Tools { tokens, .. } => *tokens,
            TurnOutcome::Delegate { .. } => 0,
        }
    }

    /// Cost in cents for this turn.
    pub fn cost_cents(&self) -> u32 {
        match self {
            TurnOutcome::Text { cost_cents, .. } => *cost_cents,
            TurnOutcome::Tools { cost_cents, .. } => *cost_cents,
            TurnOutcome::Delegate { .. } => 0,
        }
    }
}

/// The model, behind one method. Providers implement this; the loop does not
/// know which one it is talking to.
pub trait ModelTurn {
    fn turn(&self, transcript: &[String]) -> Result<TurnOutcome, PantheonError>;
    /// Model identity for ledger `ModelRequested` rows. Defaults to
    /// "default"; the canonical session path overrides this with the
    /// real chain's resolved model (default or fallback).
    fn model_name(&self) -> String {
        "default".into()
    }
}

/// Hard limits enforced by the runtime, never by the model.
#[derive(Debug, Clone)]
pub struct Budget {
    pub max_turns: u32,
    pub max_tool_calls: u32,
    /// Per-request OUTPUT cap for chat requests (not a run budget — input
    /// tokens are never counted). Precedence: `/tokens N` > the
    /// `[budget].max_tokens` config value > the session model's known
    /// maximum output (16k fallback when unknown), clamped to the model's
    /// known maximum. `None` means no cap from the session (not
    /// "unlimited by design"). Strictly optional: Pantheon never requires
    /// it, `/tokens` manages it per session, and cost tracking in stats
    /// is unaffected by its absence.
    pub max_tokens: Option<u32>,
    /// Maximum delegation depth. A loop running at `depth` may spawn a
    /// child (which runs at `depth + 1`) only while
    /// `depth + 1 <= max_delegate_depth`; deeper requests are refused
    /// with `SWARM_SPAWN_DENIED`, never recorded as fake completions.
    /// Default 2 mirrors `pantheon_runtime::swarm::Caps::default().max_depth`, the
    /// canonical swarm cap. pantheon-agent deliberately does not depend
    /// on pantheon-runtime (see the circular-dep note in engine_tests.rs),
    /// so keep this default in sync with the swarm module; the runtime
    /// may override it per session from real swarm caps.
    pub max_delegate_depth: u32,
    /// When false, a loop running at depth >= 1 (a child) may not
    /// delegate at all: the Delegate branch refuses with
    /// `SWARM_CHILD_SPAWN_DENIED` before any spawner is consulted.
    /// Default true (children may spawn, subject to depth caps).
    pub allow_child_spawn: bool,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_turns: 16,
            max_tool_calls: 32,
            max_tokens: None,
            max_delegate_depth: 2,
            allow_child_spawn: true,
        }
    }
}

/// How the loop ended.
#[derive(Debug, Clone, PartialEq)]
pub enum LoopOutcome {
    /// Model returned a final text answer.
    Answered {
        text: String,
        total_tokens: u32,
        total_cost_cents: u32,
    },
    /// A capability was denied: the run stopped hard.
    Denied { capability: Capability },
    /// Policy wants approval before the tool can run. `scope` is the call
    /// id that `pantheon run --taskID <id> --grant` expects, so the message can name
    /// the exact command instead of leaving the user to dig the id out of
    /// the ledger.
    AwaitingApproval {
        capability: Capability,
        scope: String,
    },
    /// A budget cap stopped the run.
    BudgetExhausted { cap: &'static str },
    /// The agent asked the operator a question via the `ask_user` tool.
    /// The turn parks; the host answers through the supervisor and the
    /// resumed turn sees the answer as the tool result. `call_id` ties
    /// the answer back to the exact call.
    AwaitingInput {
        call_id: String,
        question: String,
        options: Vec<String>,
    },
    /// The user interrupted the run. Distinct from Denied and from a
    /// provider error: the work was abandoned on purpose, and the run
    /// stays resumable. `reason` is the short human cause.
    Canceled { reason: String },
    /// A sub-agent was spawned and completed.
    Delegated { agent: String },
}

/// Handle-based sub-agent spawner: `spawn_handle` returns immediately and
/// the parent loop continues while the child works in parallel; the
/// model follows up through `subagent_read` / `subagent_wait` /
/// `subagent_list`. Replaces the old blocking `AgentSpawner::spawn`.
pub use crate::subagent::SubagentSpawner;

/// Swarm identity for one agent inside a swarm: which swarm and which
/// agent name this loop is running as.
pub struct SwarmCtx {
    pub swarm_id: String,
    pub agent_name: String,
}

/// Parse `ask_user` tool arguments: `{question, options?}`.
/// Malformed args degrade to the raw text as the question rather than
/// failing the turn — a confused question is still answerable, a crashed
/// turn is not.
/// Used by the production `Session::drive` pre-gate.
pub fn parse_ask_user_args(args: &str) -> (String, Vec<String>) {
    let v: serde_json::Value = serde_json::from_str(args).unwrap_or(serde_json::Value::Null);
    let question = v
        .get("question")
        .and_then(|q| q.as_str())
        .filter(|q| !q.trim().is_empty())
        .unwrap_or("")
        .to_string();
    let question = if question.is_empty() {
        args.trim().to_string()
    } else {
        question
    };
    let options = v
        .get("options")
        .and_then(|o| o.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|o| o.as_str())
                .map(str::to_string)
                .take(9)
                .collect()
        })
        .unwrap_or_default();
    (question, options)
}

pub struct AgentLoop<'a> {
    pub run_id: String,
    pub policy: Policy,
    pub budget: Budget,
    pub sink: &'a dyn EventSink,
    pub tools: &'a dyn ToolRunner,
    /// Optional spawner for sub-agents. If None, Delegate turns are denied.
    /// Spawn is non-blocking: the spawner returns a handle and the loop
    /// continues while the child works in parallel.
    pub spawner: Option<&'a dyn SubagentSpawner>,
    /// Optional swarm context. When set, the loop runs as the named
    /// agent inside the given swarm.
    pub swarm_ctx: Option<SwarmCtx>,
    /// Optional judge model. Consulted at route selection and tool gate.
    pub judge: Option<&'a dyn pantheon_api::model::Judge>,
    /// Cooperative cancellation token. Set by the user (Ctrl-C / double-Esc);
    /// the loop checks it at every turn and tool boundary and stops cleanly.
    /// It cannot abort an in-flight provider request — that returns on its own
    /// and the flag is observed on the next boundary.
    pub cancel: Option<&'a std::sync::atomic::AtomicBool>,
    /// Current depth in the swarm (0 = primary agent).
    pub depth: u32,
}

impl<'a> AgentLoop<'a> {
    /// Gate advisory (escalate-only). Returns classifier verdict for the
    /// caller to enforce against the host policy floor. None on failure
    /// or wrong answer kind.
    pub fn consult_gate_advisory(
        &self,
        judge: &dyn pantheon_api::model::Judge,
        choices: &[String],
        context: Option<&str>,
    ) -> Option<pantheon_api::model::GateVerdict> {
        use pantheon_api::model::DecisionPoint;
        let point = DecisionPoint::ToolGate;
        let model_name = judge.model_name();
        self.sink.emit(Event::DecisionRequested {
            run_id: self.run_id.clone(),
            point: point.clone(),
            model: model_name.to_string(),
        });
        let req = pantheon_api::model::DecisionRequest {
            run_id: self.run_id.clone(),
            point: point.clone(),
            query: "score risk of tool call".to_string(),
            choices: choices.to_vec(),
            context: context.map(String::from),
        };
        match judge.decide(&req) {
            Ok(answer) => {
                let summary = match &answer {
                    pantheon_api::model::DecisionAnswer::Route { choice, confidence } => {
                        pantheon_api::events::DecisionAnswerSummary::Route {
                            choice: choice.clone(),
                            confidence: *confidence,
                        }
                    }
                    pantheon_api::model::DecisionAnswer::Gate {
                        verdict,
                        confidence,
                        score,
                    } => pantheon_api::events::DecisionAnswerSummary::Gate {
                        verdict: format!("{verdict:?}"),
                        score: *score,
                        confidence: *confidence,
                    },
                    pantheon_api::model::DecisionAnswer::Binary {
                        accepted,
                        confidence,
                    } => pantheon_api::events::DecisionAnswerSummary::Binary {
                        accepted: *accepted,
                        confidence: *confidence,
                    },
                    pantheon_api::model::DecisionAnswer::Threshold { passed, value } => {
                        pantheon_api::events::DecisionAnswerSummary::Threshold {
                            passed: *passed,
                            value: *value,
                        }
                    }
                };
                self.sink.emit(Event::DecisionMade {
                    run_id: self.run_id.clone(),
                    point: point.clone(),
                    model: model_name.to_string(),
                    answer: summary,
                });
                match answer {
                    pantheon_api::model::DecisionAnswer::Gate { verdict, .. } => Some(verdict),
                    _ => {
                        self.sink.emit(Event::DecisionRecorded {
                            run_id: self.run_id.clone(),
                            point,
                            model: model_name.to_string(),
                            action: pantheon_api::events::DecisionActionSummary::Overridden {
                                fallback_used: "host-policy".to_string(),
                            },
                        });
                        None
                    }
                }
            }
            Err(e) => {
                let _ = e;
                self.sink.emit(Event::DecisionRecorded {
                    run_id: self.run_id.clone(),
                    point,
                    model: model_name.to_string(),
                    action: pantheon_api::events::DecisionActionSummary::Overridden {
                        fallback_used: "host-policy".to_string(),
                    },
                });
                None
            }
        }
    }
}
