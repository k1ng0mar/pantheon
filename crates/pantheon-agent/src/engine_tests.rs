//! Tests for `pantheon_agent::engine::tests` — sibling file so sources stay test-free.
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
            return Ok(TurnOutcome::Text {
                text: "done".into(),
                tokens: 0,
                cost_cents: 0,
            });
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
        judge: None,
        cancel: None,
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
        judge: None,
        cancel: None,
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
        judge: None,
        cancel: None,
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
        steps: RefCell::new(vec![TurnOutcome::Tools {
            calls: vec![ToolCall {
                name: "shell".into(),
                capability: Capability::ShellExecute,
                args: "ls".into(),
            }],
            tokens: 0,
            cost_cents: 0,
        }]),
    };
    let mut t = vec![];
    let out = loop_with(Policy::coder(), &sink, &tools)
        .run(&model, "go", &mut t)
        .unwrap();
    assert!(matches!(out, LoopOutcome::Answered { .. }));
    assert!(t.iter().any(|l| l.starts_with("tool[shell]")));
    assert!(sink.0.borrow().len() >= 6, "expected model/tool events");
}

#[test]
fn denied_capability_stops_the_run() {
    let sink = Collector(RefCell::new(vec![]));
    let tools = NoTools;
    let model = Scripted {
        steps: RefCell::new(vec![TurnOutcome::Tools {
            calls: vec![ToolCall {
                name: "browse".into(),
                capability: Capability::Browser,
                args: "".into(),
            }],
            tokens: 0,
            cost_cents: 0,
        }]),
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
        steps: RefCell::new(vec![TurnOutcome::Tools {
            calls: vec![ToolCall {
                name: "push".into(),
                capability: Capability::GitPush,
                args: "origin main".into(),
            }],
            tokens: 0,
            cost_cents: 0,
        }]),
    };
    let mut t = vec![];
    let out = loop_with(Policy::coder(), &sink, &tools)
        .run(&model, "go", &mut t)
        .unwrap();
    // The scope must be the real call id, because that is what the user
    // types into `pantheon grant`.
    match out {
        LoopOutcome::AwaitingApproval { capability, scope } => {
            assert_eq!(capability, Capability::GitPush);
            assert!(!scope.is_empty(), "the parked outcome names its call id");
        }
        other => panic!("expected AwaitingApproval, got {other:?}"),
    }
    assert!(!t.iter().any(|l| l.starts_with("tool[push]")));
}

#[test]
fn budget_cap_stops_before_the_turn() {
    let sink = Collector(RefCell::new(vec![]));
    let tools = NoTools;
    struct NeverAnswers;
    impl ModelTurn for NeverAnswers {
        fn turn(&self, _t: &[String]) -> Result<TurnOutcome, PantheonError> {
            Ok(TurnOutcome::Tools {
                calls: vec![ToolCall {
                    name: "shell".into(),
                    capability: Capability::ShellExecute,
                    args: "".into(),
                }],
                tokens: 0,
                cost_cents: 0,
            })
        }
    }
    let mut l = loop_with(Policy::coder(), &sink, &tools);
    l.budget = Budget {
        max_turns: 3,
        max_tool_calls: 32,
        max_tokens: None,
        max_cost_cents: None,
    };
    let mut t = vec![];
    let err = l.run(&NeverAnswers, "go", &mut t).unwrap_err();
    assert_eq!(err.code, "BUDGET_EXHAUSTED");
    assert!(err.cause.contains("max_turns"));
}

struct FixedRouter {
    answer: pantheon_api::model::DecisionAnswer,
}
impl pantheon_api::model::Judge for FixedRouter {
    fn decide(
        &self,
        _req: &pantheon_api::model::DecisionRequest,
    ) -> Result<pantheon_api::model::DecisionAnswer, PantheonError> {
        Ok(self.answer.clone())
    }
}

#[test]
fn route_outside_allowed_set_is_overridden() {
    use pantheon_api::events::{DecisionActionSummary, Event};
    use pantheon_api::model::{DecisionAnswer, DecisionPoint};
    struct CapSink(RefCell<Vec<Event>>);
    impl EventSink for CapSink {
        fn emit(&self, event: Event) {
            self.0.borrow_mut().push(event);
        }
    }
    let sink = CapSink(RefCell::new(vec![]));
    let tools = NoTools;
    struct Answer;
    impl ModelTurn for Answer {
        fn turn(&self, _t: &[String]) -> Result<TurnOutcome, PantheonError> {
            Ok(TurnOutcome::Text {
                text: "done".into(),
                tokens: 0,
                cost_cents: 0,
            })
        }
    }
    let router = FixedRouter {
        answer: DecisionAnswer::Route {
            choice: "evil-provider".into(),
            confidence: 0.99,
        },
    };
    let loop_ = AgentLoop {
        run_id: "run_route".into(),
        policy: Policy::coder(),
        budget: Budget::default(),
        sink: &sink,
        tools: &tools,
        spawner: None,
        judge: Some(&router),
        cancel: None,
        depth: 0,
    };
    let mut t = vec![];
    let out = loop_.run(&Answer, "go", &mut t).unwrap();
    assert!(matches!(out, LoopOutcome::Answered { .. }));
    let evs = sink.0.borrow();
    assert!(evs.iter().any(|e| matches!(
        e,
        Event::DecisionRequested {
            point: DecisionPoint::RouteSelect,
            ..
        }
    )));
    assert!(evs.iter().any(|e| matches!(
        e,
        Event::DecisionRecorded {
            point: DecisionPoint::RouteSelect,
            action: DecisionActionSummary::Overridden { .. },
            ..
        }
    )));
    assert!(!evs.iter().any(|e| matches!(
        e,
        Event::DecisionRecorded {
            point: DecisionPoint::RouteSelect,
            action: DecisionActionSummary::Accepted,
            ..
        }
    )));
}

/// A pre-armed cancel token must stop the loop at the first turn
/// boundary, returning Canceled instead of calling the model.
#[test]
fn cancel_token_stops_loop_before_first_turn() {
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    struct Boom;
    impl ModelTurn for Boom {
        fn turn(&self, _t: &[String]) -> Result<TurnOutcome, PantheonError> {
            panic!("model must not be called after cancel");
        }
    }

    struct NullSink;
    impl EventSink for NullSink {
        fn emit(&self, _event: Event) {}
    }
    let sink = NullSink;
    let tools = NoTools;
    let flag = Arc::new(AtomicBool::new(true));
    let loop_ = AgentLoop {
        run_id: "run_cancel".into(),
        policy: Policy::coder(),
        budget: Budget::default(),
        sink: &sink,
        tools: &tools,
        spawner: None,
        judge: None,
        cancel: Some(&flag),
        depth: 0,
    };
    let mut t = vec![];
    let out = loop_.run(&Boom, "go", &mut t).unwrap();
    assert!(
        matches!(out, LoopOutcome::Canceled { .. }),
        "expected Canceled, got {out:?}"
    );
}

#[test]
fn gate_escalates_allow_to_approval_but_never_lowers_deny() {
    use pantheon_api::events::Event;
    use pantheon_api::model::{DecisionAnswer, DecisionPoint, GateVerdict};
    struct CapSink(RefCell<Vec<Event>>);
    impl EventSink for CapSink {
        fn emit(&self, event: Event) {
            self.0.borrow_mut().push(event);
        }
    }
    let sink = CapSink(RefCell::new(vec![]));
    let tools = NoTools;
    let model = Scripted {
        steps: RefCell::new(vec![TurnOutcome::Tools {
            calls: vec![ToolCall {
                name: "shell".into(),
                capability: Capability::ShellExecute,
                args: "ls".into(),
            }],
            tokens: 0,
            cost_cents: 0,
        }]),
    };
    let router = FixedRouter {
        answer: DecisionAnswer::Gate {
            verdict: GateVerdict::NeedsApproval {
                reason: "risky".into(),
            },
            confidence: 0.9,
            score: 0.8,
        },
    };
    let loop_ = AgentLoop {
        run_id: "run_gate_up".into(),
        policy: Policy::coder(),
        budget: Budget::default(),
        sink: &sink,
        tools: &tools,
        spawner: None,
        judge: Some(&router),
        cancel: None,
        depth: 0,
    };
    let mut t = vec![];
    let out = loop_.run(&model, "go", &mut t).unwrap();
    match out {
        LoopOutcome::AwaitingApproval { capability, scope } => {
            assert_eq!(capability, Capability::ShellExecute);
            assert!(!scope.is_empty(), "the parked outcome names its call id");
        }
        other => panic!("expected AwaitingApproval, got {other:?}"),
    }
    let sink2 = CapSink(RefCell::new(vec![]));
    let model2 = Scripted {
        steps: RefCell::new(vec![TurnOutcome::Tools {
            calls: vec![ToolCall {
                name: "browse".into(),
                capability: Capability::Browser,
                args: "".into(),
            }],
            tokens: 0,
            cost_cents: 0,
        }]),
    };
    let router2 = FixedRouter {
        answer: DecisionAnswer::Gate {
            verdict: GateVerdict::Allow,
            confidence: 0.99,
            score: 0.0,
        },
    };
    let loop2 = AgentLoop {
        run_id: "run_gate_down".into(),
        policy: Policy::coder(),
        budget: Budget::default(),
        sink: &sink2,
        tools: &tools,
        spawner: None,
        judge: Some(&router2),
        cancel: None,
        depth: 0,
    };
    let mut t2 = vec![];
    let err = loop2.run(&model2, "go", &mut t2).unwrap_err();
    assert_eq!(err.code, "CAP_DENIED");
    let evs2 = sink2.0.borrow();
    assert!(evs2.iter().any(|e| matches!(
        e,
        Event::DecisionRecorded {
            point: DecisionPoint::ToolGate,
            ..
        }
    )));
}
