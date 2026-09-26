//! The loop itself. Pure orchestration: no model SDK, no process spawning.
//!
//! TEST HARNESS — not the canonical runtime path. `Session::drive` in
//! `pantheon-runtime` is canonical (typed messages, provenance, real
//! provider chain). This loop takes a `Vec<String>` transcript so unit
//! tests can drive it with scripted models and no network.
use crate::tool::{gate, EventSink, GateOutcome, ToolRunner};
use pantheon_core::capability::{Capability, Policy};
use pantheon_core::error::{Layer, PantheonError};
use pantheon_core::events::Event;
use pantheon_core::provenance::Provenance;

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
    /// Model identity for ledger `ModelRequested` rows. The test harness
    /// defaults to "default"; the canonical session path overrides this
    /// with the real chain's resolved model (default or fallback).
    fn model_name(&self) -> String {
        "default".into()
    }
}

/// Hard limits enforced by the runtime, never by the model.
#[derive(Debug, Clone)]
pub struct Budget {
    pub max_turns: u32,
    pub max_tool_calls: u32,
    /// Maximum tokens (input + output) across the entire run.
    /// `None` means no token cap (not "unlimited by design").
    pub max_tokens: Option<u32>,
    /// Maximum cost in US cents (e.g. 500 = $5.00).
    /// `None` means no cost cap.
    pub max_cost_cents: Option<u32>,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_turns: 16,
            max_tool_calls: 32,
            max_tokens: None,
            max_cost_cents: None,
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
    /// Policy wants approval before the tool can run.
    AwaitingApproval { capability: Capability },
    /// A budget cap stopped the run.
    BudgetExhausted { cap: &'static str },
    /// The user interrupted the run. Distinct from Denied and from a
    /// provider error: the work was abandoned on purpose, and the run
    /// stays resumable. `reason` is the short human cause.
    Canceled { reason: String },
    /// A sub-agent was spawned and completed.
    Delegated { agent: String },
}

/// Spawns a specialist sub-agent. Implemented by the runtime supervisor
/// so the loop can hand off work and get back a result transcript.
pub trait AgentSpawner {
    /// Return a transcript fragment from the spawned agent, or a structured
    /// error if swarm caps refuse the spawn.
    fn spawn(
        &self,
        agent: &str,
        model: &str,
        task: &str,
        depth: u32,
    ) -> Result<String, PantheonError>;
}

fn sberr(msg: String) -> PantheonError {
    PantheonError::new(
        "SWARM_SPAWN_DENIED",
        Layer::Agent,
        false,
        msg,
        "reduce delegation depth or raise the swarm cap",
        "",
    )
}

fn berr(cap: &'static str) -> PantheonError {
    PantheonError::new(
        "BUDGET_EXHAUSTED",
        Layer::Agent,
        false,
        format!("{cap} cap reached"),
        "raise the budget explicitly or simplify the task",
        "",
    )
}

pub struct AgentLoop<'a> {
    pub run_id: String,
    pub policy: Policy,
    pub budget: Budget,
    pub sink: &'a dyn EventSink,
    pub tools: &'a dyn ToolRunner,
    /// Optional spawner for sub-agents. If None, Delegate turns are denied.
    pub spawner: Option<&'a dyn AgentSpawner>,
    /// Optional judge model. Consulted at route selection and tool gate.
    pub judge: Option<&'a dyn pantheon_core::model::Judge>,
    /// Cooperative cancellation token. Set by the user (Ctrl-C / double-Esc);
    /// the loop checks it at every turn and tool boundary and stops cleanly.
    /// It cannot abort an in-flight provider request — that returns on its own
    /// and the flag is observed on the next boundary.
    pub cancel: Option<&'a std::sync::atomic::AtomicBool>,
    /// Current depth in the swarm (0 = primary agent).
    pub depth: u32,
}

impl<'a> AgentLoop<'a> {
    /// Drive the loop to a terminal outcome. Every transition is emitted.
    pub fn run(
        &self,
        model: &dyn ModelTurn,
        task: &str,
        transcript: &mut Vec<String>,
    ) -> Result<LoopOutcome, PantheonError> {
        transcript.push(format!("user: {task}"));
        let mut turns = 0u32;
        let mut calls = 0u32;
        let mut total_tokens: u32 = 0;
        let mut total_cost_cents: u32 = 0;

        loop {
            // Cooperative cancel boundary: checked at the top of every turn,
            // before the model is asked to do more work.
            if self
                .cancel
                .is_some_and(|f| f.load(std::sync::atomic::Ordering::SeqCst))
            {
                return Ok(LoopOutcome::Canceled {
                    reason: "interrupted by user".to_string(),
                });
            }
            if turns >= self.budget.max_turns {
                return Err(berr("max_turns"));
            }
            turns += 1;
            self.sink.emit(Event::ModelRequested {
                run_id: self.run_id.clone(),
                model: model.model_name(),
            });

            // Route selection (advisory-only): ask the judge model
            // which provider/model to use for this turn. The answer is
            // validated against the allowed set (default + fallbacks);
            // anything else is recorded as Overridden and ignored.
            // Never changes which model actually runs — that stays
            // runtime-controlled via the fallback chain.
            if let Some(judge) = self.judge {
                self.consult_route_advisory(
                    judge,
                    &[format!("provider={}", "default")],
                    Some(task),
                );
            }

            let outcome = model.turn(transcript)?;
            self.sink.emit(Event::ModelCompleted {
                run_id: self.run_id.clone(),
            });

            // Track token and cost budgets across the run
            total_tokens += outcome.tokens();
            total_cost_cents += outcome.cost_cents();
            if let Some(cap) = self.budget.max_tokens {
                if total_tokens >= cap {
                    return Ok(LoopOutcome::BudgetExhausted { cap: "max_tokens" });
                }
            }
            if let Some(cap) = self.budget.max_cost_cents {
                if total_cost_cents >= cap {
                    return Ok(LoopOutcome::BudgetExhausted {
                        cap: "max_cost_cents",
                    });
                }
            }

            match outcome {
                TurnOutcome::Text { text, .. } => {
                    transcript.push(format!("assistant: {text}"));
                    if turns > 1 {
                        self.sink.emit(Event::RunProgress {
                            run_id: self.run_id.clone(),
                            detail: format!(
                                "{turns} turns, {calls} tool calls, {total_tokens} tokens, ${total_cost_cents} cents"
                            ),
                        });
                    }
                    return Ok(LoopOutcome::Answered {
                        text,
                        total_tokens,
                        total_cost_cents,
                    });
                }
                TurnOutcome::Tools {
                    calls: tool_calls, ..
                } => {
                    // Budget counts EXECUTED calls only: denials never
                    // consume budget (group-A audit). Check-then-count:
                    // gate first, count only on Allow.
                    for (i, call) in tool_calls.into_iter().enumerate() {
                        let call_id = format!("call_{}_{}", turns, i);
                        // Tool gating (escalate-only): the classifier may raise
                        // Allow -> Approval/Deny but never lower Deny -> Allow.
                        // The deterministic host policy is the floor.
                        // Returns the classifier verdict if it escalates,
                        // else None (host policy stands).
                        let classifier_verdict = if let Some(judge) = self.judge {
                            self.consult_gate_advisory(
                                judge,
                                &[call.name.clone(), format!("{:?}", call.capability)],
                                Some(&call.args),
                            )
                        } else {
                            None
                        };
                        // Host floor from deterministic policy.
                        // gate() returns Ok(Allow|NeedsApproval) or Err(Deny).
                        // Classifier may only ESCALATE (Allow->Approval/Deny).
                        let host_outcome = gate(&self.policy, &call.capability);
                        let effective = match (host_outcome, &classifier_verdict) {
                            // Host denies: classifier cannot lower it.
                            (Err(e), _) => {
                                self.sink.emit(Event::DecisionRecorded {
                                    run_id: self.run_id.clone(),
                                    point: pantheon_core::model::DecisionPoint::ToolGate,
                                    model: self
                                        .judge
                                        .map(|d| d.model_name().to_string())
                                        .unwrap_or_else(|| "host-policy".to_string()),
                                    action: pantheon_core::events::DecisionActionSummary::Denied {
                                        reason: format!(
                                            "host policy blocked {:?}",
                                            call.capability
                                        ),
                                    },
                                });
                                return Err(e);
                            }
                            // Host parks for approval: classifier cannot lower.
                            (Ok(GateOutcome::NeedsApproval { capability }), _) => {
                                GateOutcome::NeedsApproval { capability }
                            }
                            // Host allows: honor classifier only if it escalates.
                            (Ok(_), Some(v))
                                if v.escalation_level()
                                    > pantheon_core::model::GateVerdict::Allow
                                        .escalation_level() =>
                            {
                                match v {
                                    pantheon_core::model::GateVerdict::Deny { reason } => {
                                        self.sink.emit(Event::DecisionRecorded {
                                            run_id: self.run_id.clone(),
                                            point: pantheon_core::model::DecisionPoint::ToolGate,
                                            model: self
                                                .judge
                                                .map(|d| d.model_name().to_string())
                                                .unwrap_or_else(|| {
                                                    "host-policy".to_string()
                                                }),
                                            action: pantheon_core::events::DecisionActionSummary::Denied {
                                                reason: reason.clone(),
                                            },
                                        });
                                        return Err(PantheonError::new(
                                            "GATE_ESCALATED_DENY",
                                            Layer::Agent,
                                            false,
                                            format!(
                                                "judge model escalated {:?} to deny: {reason}",
                                                call.capability
                                            ),
                                            "adjust the policy or narrow the tool call",
                                            "",
                                        ));
                                    }
                                    pantheon_core::model::GateVerdict::NeedsApproval { .. } => {
                                        GateOutcome::NeedsApproval {
                                            capability: call.capability.clone(),
                                        }
                                    }
                                    pantheon_core::model::GateVerdict::Allow => GateOutcome::Allow,
                                }
                            }
                            (Ok(_), _) => GateOutcome::Allow,
                        };
                        // Record the final host action for this gate.
                        if self.judge.is_some() {
                            let action = match &effective {
                                GateOutcome::Allow => {
                                    pantheon_core::events::DecisionActionSummary::Accepted
                                }
                                GateOutcome::NeedsApproval { .. } => {
                                    pantheon_core::events::DecisionActionSummary::Accepted
                                }
                            };
                            self.sink.emit(Event::DecisionRecorded {
                                run_id: self.run_id.clone(),
                                point: pantheon_core::model::DecisionPoint::ToolGate,
                                model: self
                                    .judge
                                    .map(|d| d.model_name().to_string())
                                    .unwrap_or_else(|| "host-policy".to_string()),
                                action,
                            });
                        }
                        match effective {
                            GateOutcome::Allow => {}
                            GateOutcome::NeedsApproval { capability } => {
                                self.sink.emit(Event::ApprovalRequested {
                                    run_id: self.run_id.clone(),
                                    scope: call_id.clone(),
                                });
                                return Ok(LoopOutcome::AwaitingApproval { capability });
                            }
                        }
                        // Only executed calls consume budget. Denials and
                        // approvals park/fail before this point and cost
                        // nothing against max_tool_calls.
                        if calls >= self.budget.max_tool_calls {
                            return Ok(LoopOutcome::BudgetExhausted {
                                cap: "max_tool_calls",
                            });
                        }
                        calls += 1;
                        self.sink.emit(Event::ToolRequested {
                            run_id: self.run_id.clone(),
                            tool: call.name.clone(),
                        });
                        self.sink.emit(Event::ToolStarted {
                            run_id: self.run_id.clone(),
                            call_id: call_id.clone(),
                            tool: call.name.clone(),
                            args: call.args.clone(),
                            provenance: Provenance::untrusted(&call.name),
                        });
                        let out = self.tools.run(&call.name, &call.args)?;
                        self.sink.emit(Event::ToolOutput {
                            run_id: self.run_id.clone(),
                            call_id: call_id.clone(),
                            tool: call.name.clone(),
                            truncated: false,
                            provenance: Provenance::untrusted(&call.name),
                        });
                        transcript.push(format!("tool[{}]: {out}", call.name));
                        self.sink.emit(Event::ToolCompleted {
                            run_id: self.run_id.clone(),
                            call_id,
                            tool: call.name.clone(),
                            provenance: Provenance::untrusted(&call.name),
                        });
                    }
                }
                TurnOutcome::Delegate { agent, model, task } => {
                    self.sink.emit(Event::AgentMessage {
                        run_id: self.run_id.clone(),
                        agent: agent.clone(),
                    });
                    // No spawner wired: delegation is denied by the host
                    // (session.rs also denies today pending swarm Caps).
                    // Fail loud — never silently continue as if delegated.
                    let spawner = self
                        .spawner
                        .ok_or_else(|| sberr("no spawner configured for delegation".into()))?;
                    match spawner.spawn(&agent, &model, &task, self.depth) {
                        // Spawn succeeded: sub-agent output lands in the
                        // transcript and the loop STOPS here. The parent
                        // does not continue from delegated output in v1
                        // (swarm Caps wiring will decide resume vs stop).
                        Ok(result) => {
                            transcript.push(format!("delegate[{agent}]: {result}"));
                            return Ok(LoopOutcome::Delegated { agent });
                        }
                        Err(e) if e.code == "SWARM_SPAWN_DENIED" => {
                            return Err(e);
                        }
                        Err(e) => {
                            self.sink.emit(Event::RunFailed {
                                run_id: self.run_id.clone(),
                                code: e.code.clone(),
                            });
                            return Err(e);
                        }
                    }
                }
            }
        }
    }

    /// Route advisory: validate choice against `allowed`, record outcome.
    /// Returns validated choice or None (host keeps default). Never panics.
    ///
    /// `pub` because the canonical-message driver (`pantheon-runtime`'s
    /// `Session::drive`) consults the judge inline too — one advisory
    /// implementation for both loop paths.
    pub fn consult_route_advisory(
        &self,
        judge: &dyn pantheon_core::model::Judge,
        allowed: &[String],
        context: Option<&str>,
    ) -> Option<String> {
        use pantheon_core::model::DecisionPoint;
        let point = DecisionPoint::RouteSelect;
        let model_name = judge.model_name();
        self.sink.emit(Event::DecisionRequested {
            run_id: self.run_id.clone(),
            point: point.clone(),
            model: model_name.to_string(),
        });

        let req = pantheon_core::model::DecisionRequest {
            run_id: self.run_id.clone(),
            point: point.clone(),
            query: "select route for this turn".to_string(),
            choices: allowed.to_vec(),
            context: context.map(String::from),
        };

        match judge.decide(&req) {
            Ok(answer) => {
                let summary = match &answer {
                    pantheon_core::model::DecisionAnswer::Route { choice, confidence } => {
                        pantheon_core::events::DecisionAnswerSummary::Route {
                            choice: choice.clone(),
                            confidence: *confidence,
                        }
                    }
                    pantheon_core::model::DecisionAnswer::Gate {
                        verdict,
                        confidence,
                        score,
                    } => pantheon_core::events::DecisionAnswerSummary::Gate {
                        verdict: format!("{:?}", verdict),
                        score: *score,
                        confidence: *confidence,
                    },
                    pantheon_core::model::DecisionAnswer::Binary {
                        accepted,
                        confidence,
                    } => pantheon_core::events::DecisionAnswerSummary::Binary {
                        accepted: *accepted,
                        confidence: *confidence,
                    },
                    pantheon_core::model::DecisionAnswer::Threshold { passed, value } => {
                        pantheon_core::events::DecisionAnswerSummary::Threshold {
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
                // Advisory-only: route choice validated against allowed set.
                // Accepted only if it matches; else Overridden -> default.
                let accepted = answer.validated_route(&req.choices).is_some();
                self.sink.emit(Event::DecisionRecorded {
                    run_id: self.run_id.clone(),
                    point: req.point.clone(),
                    model: model_name.to_string(),
                    action: if accepted {
                        pantheon_core::events::DecisionActionSummary::Accepted
                    } else {
                        pantheon_core::events::DecisionActionSummary::Overridden {
                            fallback_used: "default".to_string(),
                        }
                    },
                });
                if accepted {
                    return answer.validated_route(&req.choices);
                }
                None
            }
            Err(e) => {
                // Model failed or returned an error. Host falls back to defaults.
                let _ = e;
                self.sink.emit(Event::DecisionRecorded {
                    run_id: self.run_id.clone(),
                    point,
                    model: model_name.to_string(),
                    action: pantheon_core::events::DecisionActionSummary::Overridden {
                        fallback_used: "default".to_string(),
                    },
                });
                None
            }
        }
    }

    /// Gate advisory (escalate-only). Returns classifier verdict for the
    /// caller to enforce against the host policy floor. None on failure
    /// or wrong answer kind. See `consult_route_advisory` on visibility.
    pub fn consult_gate_advisory(
        &self,
        judge: &dyn pantheon_core::model::Judge,
        choices: &[String],
        context: Option<&str>,
    ) -> Option<pantheon_core::model::GateVerdict> {
        use pantheon_core::model::DecisionPoint;
        let point = DecisionPoint::ToolGate;
        let model_name = judge.model_name();
        self.sink.emit(Event::DecisionRequested {
            run_id: self.run_id.clone(),
            point: point.clone(),
            model: model_name.to_string(),
        });
        let req = pantheon_core::model::DecisionRequest {
            run_id: self.run_id.clone(),
            point: point.clone(),
            query: "score risk of tool call".to_string(),
            choices: choices.to_vec(),
            context: context.map(String::from),
        };
        match judge.decide(&req) {
            Ok(answer) => {
                let summary = match &answer {
                    pantheon_core::model::DecisionAnswer::Route { choice, confidence } => {
                        pantheon_core::events::DecisionAnswerSummary::Route {
                            choice: choice.clone(),
                            confidence: *confidence,
                        }
                    }
                    pantheon_core::model::DecisionAnswer::Gate {
                        verdict,
                        confidence,
                        score,
                    } => pantheon_core::events::DecisionAnswerSummary::Gate {
                        verdict: format!("{verdict:?}"),
                        score: *score,
                        confidence: *confidence,
                    },
                    pantheon_core::model::DecisionAnswer::Binary {
                        accepted,
                        confidence,
                    } => pantheon_core::events::DecisionAnswerSummary::Binary {
                        accepted: *accepted,
                        confidence: *confidence,
                    },
                    pantheon_core::model::DecisionAnswer::Threshold { passed, value } => {
                        pantheon_core::events::DecisionAnswerSummary::Threshold {
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
                    pantheon_core::model::DecisionAnswer::Gate { verdict, .. } => Some(verdict),
                    _ => {
                        self.sink.emit(Event::DecisionRecorded {
                            run_id: self.run_id.clone(),
                            point,
                            model: model_name.to_string(),
                            action: pantheon_core::events::DecisionActionSummary::Overridden {
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
                    action: pantheon_core::events::DecisionActionSummary::Overridden {
                        fallback_used: "host-policy".to_string(),
                    },
                });
                None
            }
        }
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
