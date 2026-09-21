//! The loop itself. Pure orchestration: no model SDK, no process spawning.
use crate::tool::{gate, EventSink, GateOutcome, ToolRunner};
use pantheon_core::capability::{Capability, Policy};
use pantheon_core::error::{Layer, PantheonError};
use pantheon_core::events::Event;

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    /// Plain text response.
    Text(String),
    /// One or more tool calls to execute.
    Tools(Vec<ToolCall>),
    /// Delegate to a specialist sub-agent.
    /// The loop checks swarm caps and emits AgentSpawned/AgentCompleted.
    Delegate {
        agent: String,
        model: String,
        task: String,
    },
}

/// The model, behind one method. Providers implement this; the loop does not
/// know which one it is talking to.
pub trait ModelTurn {
    fn turn(&self, transcript: &[String]) -> Result<TurnOutcome, PantheonError>;
}

/// Hard limits enforced by the runtime, never by the model.
#[derive(Debug, Clone)]
pub struct Budget {
    pub max_turns: u32,
    pub max_tool_calls: u32,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_turns: 16,
            max_tool_calls: 32,
        }
    }
}

/// How the loop ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopOutcome {
    /// Model returned a final text answer.
    Answered(String),
    /// A capability was denied: the run stopped hard.
    Denied { capability: Capability },
    /// Policy wants approval before the tool can run.
    AwaitingApproval { capability: Capability },
    /// A budget cap stopped the run.
    BudgetExhausted { cap: &'static str },
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

        loop {
            if turns >= self.budget.max_turns {
                return Err(berr("max_turns"));
            }
            turns += 1;
            self.sink.emit(Event::ModelRequested {
                run_id: self.run_id.clone(),
                model: "default".into(),
            });

            let outcome = model.turn(transcript)?;
            self.sink.emit(Event::ModelCompleted {
                run_id: self.run_id.clone(),
            });

            match outcome {
                TurnOutcome::Text(text) => {
                    transcript.push(format!("assistant: {text}"));
                    if turns > 1 {
                        self.sink.emit(Event::RunProgress {
                            run_id: self.run_id.clone(),
                            detail: format!("{turns} turns, {calls} tool calls"),
                        });
                    }
                    return Ok(LoopOutcome::Answered(text));
                }
                TurnOutcome::Tools(tool_calls) => {
                    for call in tool_calls {
                        if calls >= self.budget.max_tool_calls {
                            return Ok(LoopOutcome::BudgetExhausted {
                                cap: "max_tool_calls",
                            });
                        }
                        match gate(&self.policy, &call.capability)? {
                            GateOutcome::Allow => {}
                            GateOutcome::NeedsApproval { capability } => {
                                self.sink.emit(Event::ApprovalRequested {
                                    run_id: self.run_id.clone(),
                                    scope: format!("{capability:?}"),
                                });
                                return Ok(LoopOutcome::AwaitingApproval { capability });
                            }
                        }
                        calls += 1;
                        self.sink.emit(Event::ToolRequested {
                            run_id: self.run_id.clone(),
                            tool: call.name.clone(),
                        });
                        self.sink.emit(Event::ToolStarted {
                            run_id: self.run_id.clone(),
                            tool: call.name.clone(),
                        });
                        let out = self.tools.run(&call.name, &call.args)?;
                        self.sink.emit(Event::ToolOutput {
                            run_id: self.run_id.clone(),
                            tool: call.name.clone(),
                            truncated: false,
                        });
                        transcript.push(format!("tool[{}]: {out}", call.name));
                        self.sink.emit(Event::ToolCompleted {
                            run_id: self.run_id.clone(),
                            tool: call.name.clone(),
                        });
                    }
                }
                TurnOutcome::Delegate { agent, model, task } => {
                    self.sink.emit(Event::AgentMessage {
                        run_id: self.run_id.clone(),
                        agent: agent.clone(),
                    });
                    let spawner = self
                        .spawner
                        .ok_or_else(|| sberr("no spawner configured for delegation".into()))?;
                    match spawner.spawn(&agent, &model, &task, self.depth) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolRunner;
    use std::cell::RefCell;

    struct Collector(RefCell<Vec<String>>);
    impl EventSink for Collector {
        fn emit(&self, ev: Event) {
            let s = format!("{ev:?}");
            self.0
                .borrow_mut()
                .push(s.chars().take(24).collect::<String>());
        }
    }

    struct NoTools;
    impl ToolRunner for NoTools {
        fn run(&self, _n: &str, _a: &str) -> Result<String, PantheonError> {
            Ok("ok".into())
        }
    }

    struct Scripted {
        steps: RefCell<Vec<TurnOutcome>>,
    }
    impl ModelTurn for Scripted {
        fn turn(&self, _t: &[String]) -> Result<TurnOutcome, PantheonError> {
            let mut s = self.steps.borrow_mut();
            if s.is_empty() {
                return Ok(TurnOutcome::Text("done".into()));
            }
            Ok(s.remove(0))
        }
    }

    fn loop_with<'a>(policy: Policy, sink: &'a Collector, tools: &'a NoTools) -> AgentLoop<'a> {
        AgentLoop {
            run_id: "run_t".into(),
            policy,
            budget: Budget::default(),
            sink,
            tools,
            spawner: None,
            depth: 0,
        }
    }

    /// A spawner that simulates swarm cap enforcement without the swarm crate
    /// (avoids a circular dep: agent ← swarm ← provider).
    struct CappedSpawner {
        max_depth: u32,
    }
    impl AgentSpawner for CappedSpawner {
        fn spawn(
            &self,
            _agent: &str,
            _model: &str,
            _task: &str,
            depth: u32,
        ) -> Result<String, PantheonError> {
            if depth + 1 > self.max_depth {
                return Err(PantheonError::new(
                    "SWARM_SPAWN_DENIED",
                    Layer::Agent,
                    false,
                    format!("depth {depth}+1 exceeds max {}", self.max_depth),
                    "reduce delegation depth or raise the swarm cap",
                    "",
                ));
            }
            Ok("spawned-sub-agent-result".into())
        }
    }

    #[test]
    fn delegate_emits_agent_message_and_returns_delegated() {
        let sink = Collector(RefCell::new(vec![]));
        let tools = NoTools;
        let spawner = CappedSpawner { max_depth: 2 };
        let model = Scripted {
            steps: RefCell::new(vec![TurnOutcome::Delegate {
                agent: "researcher".into(),
                model: "kimi".into(),
                task: "find X".into(),
            }]),
        };
        let mut t = vec![];
        let loop_ = AgentLoop {
            run_id: "run_d".into(),
            policy: Policy::coder(),
            budget: Budget::default(),
            sink: &sink,
            tools: &tools,
            spawner: Some(&spawner),
            depth: 0,
        };
        let out = loop_.run(&model, "go", &mut t).unwrap();
        // Loop returns Delegated immediately after a successful spawn.
        assert_eq!(
            out,
            LoopOutcome::Delegated {
                agent: "researcher".into()
            }
        );
        assert!(t.iter().any(|l| l.contains("delegate[researcher]")));
        assert!(t.iter().any(|l| l.contains("spawned-sub-agent-result")));
        // AgentMessage event emitted.
        assert!(sink.0.borrow().iter().any(|s| s.contains("AgentMessage")));
    }

    #[test]
    fn depth_cap_denies_spawn_and_returns_error() {
        let sink = Collector(RefCell::new(vec![]));
        let tools = NoTools;
        let spawner = CappedSpawner { max_depth: 0 };
        let model = Scripted {
            steps: RefCell::new(vec![TurnOutcome::Delegate {
                agent: "researcher".into(),
                model: "kimi".into(),
                task: "find X".into(),
            }]),
        };
        let mut t = vec![];
        let loop_ = AgentLoop {
            run_id: "run_d2".into(),
            policy: Policy::coder(),
            budget: Budget::default(),
            sink: &sink,
            tools: &tools,
            spawner: Some(&spawner),
            depth: 0,
        };
        let err = loop_.run(&model, "go", &mut t).unwrap_err();
        assert_eq!(err.code, "SWARM_SPAWN_DENIED");
        // Transcript has the initial "user:" but no delegate entry appended.
        assert!(!t.iter().any(|l| l.starts_with("delegate[")));
        // AgentMessage was emitted before the spawn attempt.
        assert!(sink.0.borrow().iter().any(|s| s.contains("AgentMessage")));
    }

    #[test]
    fn delegate_without_spawner_fails_loud() {
        let sink = Collector(RefCell::new(vec![]));
        let tools = NoTools;
        let model = Scripted {
            steps: RefCell::new(vec![TurnOutcome::Delegate {
                agent: "x".into(),
                model: "y".into(),
                task: "z".into(),
            }]),
        };
        let mut t = vec![];
        // loop_with sets spawner: None.
        let err = loop_with(Policy::coder(), &sink, &tools)
            .run(&model, "go", &mut t)
            .unwrap_err();
        assert_eq!(err.code, "SWARM_SPAWN_DENIED");
    }

    #[test]
    fn scripted_two_turn_run_completes() {
        let sink = Collector(RefCell::new(vec![]));
        let tools = NoTools;
        let model = Scripted {
            steps: RefCell::new(vec![TurnOutcome::Tools(vec![ToolCall {
                name: "shell".into(),
                capability: Capability::ShellExecute,
                args: "ls".into(),
            }])]),
        };
        let mut t = vec![];
        let out = loop_with(Policy::coder(), &sink, &tools)
            .run(&model, "go", &mut t)
            .unwrap();
        assert_eq!(out, LoopOutcome::Answered("done".into()));
        assert!(t.iter().any(|l| l.starts_with("tool[shell]")));
        assert!(sink.0.borrow().len() >= 6, "expected model/tool events");
    }

    #[test]
    fn denied_capability_stops_the_run() {
        let sink = Collector(RefCell::new(vec![]));
        let tools = NoTools;
        let model = Scripted {
            steps: RefCell::new(vec![TurnOutcome::Tools(vec![ToolCall {
                name: "browse".into(),
                capability: Capability::Browser,
                args: "".into(),
            }])]),
        };
        let mut t = vec![];
        let err = loop_with(Policy::coder(), &sink, &tools)
            .run(&model, "go", &mut t)
            .unwrap_err();
        assert_eq!(err.code, "CAP_DENIED");
    }

    #[test]
    fn approval_parks_instead_of_running() {
        let sink = Collector(RefCell::new(vec![]));
        let tools = NoTools;
        let model = Scripted {
            steps: RefCell::new(vec![TurnOutcome::Tools(vec![ToolCall {
                name: "push".into(),
                capability: Capability::GitPush,
                args: "origin main".into(),
            }])]),
        };
        let mut t = vec![];
        let out = loop_with(Policy::coder(), &sink, &tools)
            .run(&model, "go", &mut t)
            .unwrap();
        assert_eq!(
            out,
            LoopOutcome::AwaitingApproval {
                capability: Capability::GitPush
            }
        );
        assert!(!t.iter().any(|l| l.starts_with("tool[push]")));
    }

    #[test]
    fn budget_cap_stops_before_the_turn() {
        let sink = Collector(RefCell::new(vec![]));
        let tools = NoTools;
        struct NeverAnswers;
        impl ModelTurn for NeverAnswers {
            fn turn(&self, _t: &[String]) -> Result<TurnOutcome, PantheonError> {
                Ok(TurnOutcome::Tools(vec![ToolCall {
                    name: "shell".into(),
                    capability: Capability::ShellExecute,
                    args: "".into(),
                }]))
            }
        }
        let mut l = loop_with(Policy::coder(), &sink, &tools);
        l.budget = Budget {
            max_turns: 3,
            max_tool_calls: 32,
        };
        let mut t = vec![];
        let err = l.run(&NeverAnswers, "go", &mut t).unwrap_err();
        assert_eq!(err.code, "BUDGET_EXHAUSTED");
        assert!(err.cause.contains("max_turns"));
    }
}
