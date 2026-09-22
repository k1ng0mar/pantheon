//! Wired agent session: supervisor + provider chain + tool registry + loop.
//!
//! This is the "fully functioning harness" path: a run goes
//! start -> loop (model turns, gated tool calls, compacted output)
//! -> terminal outcome, every step event-sourced in the ledger.

use crate::Supervisor;
use pantheon_agent::{AgentLoop, Budget, LoopOutcome};
use pantheon_core::capability::Policy;
use pantheon_core::error::PantheonError;
use pantheon_core::events::Event;
use pantheon_core::message::{Message, ToolCallRef};
use pantheon_core::model::ModelPolicy;
use pantheon_exec::builtins::register_builtins;
use pantheon_exec::safewrite::register_safewrite;
use pantheon_exec::tools::ToolRegistry;
use pantheon_providers::http::HttpTransport;
use pantheon_providers::ProviderChain;

/// Adapter: supervisor as the loop's event sink.
struct SupSink<'a>(&'a Supervisor);
impl<'a> pantheon_agent::EventSink for SupSink<'a> {
    fn emit(&self, event: Event) {
        if let Err(e) = self.0.emit(event) {
            eprintln!("ledger write failed: {e}");
        }
    }
}

/// Adapter: project normalized provider `ModelEvent`s into the run's
/// ledger `Event`s (Attempt→ModelRequested, TextDelta→ModelDelta,
/// Completed→ModelCompleted, Fallback/AttemptFailed→RunProgress,
/// Exhausted→RunFailed). The agent loop never sees providers; this is the
/// only place the provider plane meets the ledger.
struct LedgerModelSink<'a> {
    sup: &'a Supervisor,
    run_id: &'a str,
}
impl<'a> pantheon_core::model_event::ModelEventSink for LedgerModelSink<'a> {
    fn emit(&self, event: pantheon_core::model_event::ModelEvent) {
        if let Some(ev) = event.to_event(self.run_id) {
            if let Err(e) = self.sup.emit(ev) {
                eprintln!("ledger write failed: {e}");
            }
        }
    }
}

/// Adapter: tool registry as the loop's runner. Capability comes from the
/// registry, not the model's claim.
struct RegRunner<'a>(&'a ToolRegistry);
impl<'a> pantheon_agent::ToolRunner for RegRunner<'a> {
    fn run(&self, name: &str, args: &str) -> Result<String, PantheonError> {
        self.0.execute(name, args)
    }
}

/// Everything one agent run needs.
pub struct Session {
    pub supervisor: Supervisor,
    pub policy: Policy,
    pub model_policy: ModelPolicy,
    pub api_key: String,
    pub budget: Budget,
    pub system_prompt: String,
}

impl Session {
    pub fn new(
        data_dir: std::path::PathBuf,
        policy: Policy,
        model_policy: ModelPolicy,
        api_key: String,
    ) -> Result<Self, PantheonError> {
        Ok(Self {
            supervisor: Supervisor::open(data_dir)?,
            policy,
            model_policy,
            api_key,
            budget: Budget::default(),
            system_prompt: String::new(),
        })
    }

    /// Run one task end to end. Returns the terminal outcome.
    pub fn chat(&self, run_id: &str, user_message: &str) -> Result<LoopOutcome, PantheonError> {
        let recovered = self.supervisor.start_run(run_id)?;
        let mut messages: Vec<Message> = if recovered {
            self.supervisor.emit(Event::RunProgress {
                run_id: run_id.into(),
                detail: "recovered unfinished run".into(),
            })?;
            rebuild_messages(self.supervisor.replay(run_id)?)
        } else {
            Vec::new()
        };
        if messages.is_empty() {
            if !self.system_prompt.is_empty() {
                messages.push(Message::system(&self.system_prompt));
            }
            messages.push(Message::user(user_message));
            self.supervisor.emit(Event::AssistantMessage {
                run_id: run_id.into(),
                message: Message::user(user_message),
            })?;
        }
        let mut reg = ToolRegistry::new();
        register_builtins(&mut reg);
        register_safewrite(&mut reg, self.supervisor.data_dir().join("safewrite"));
        let chain = ProviderChain::new(
            self.model_policy.clone(),
            HttpTransport::default(),
            reg.schemas(),
            self.api_key.clone(),
        );

        let sink = SupSink(&self.supervisor);
        let runner = RegRunner(&reg);
        let _ = (&sink, &runner); // adapters used by the legacy loop path below
        let loop_ = AgentLoop {
            run_id: run_id.into(),
            policy: self.policy.clone(),
            budget: self.budget.clone(),
            sink: &sink,
            tools: &runner,
            spawner: None,
            depth: 0,
        };

        // Drive the loop through the canonical-message path: each turn feeds
        // messages to the provider, appends assistant/tool rows, repeats.
        let outcome = self.drive(&loop_, &chain, &mut messages, run_id, 0, &reg)?;

        match &outcome {
            LoopOutcome::Answered(text) => {
                println!("{text}");
                self.supervisor.emit(Event::RunProgress {
                    run_id: run_id.into(),
                    detail: text.chars().take(200).collect(),
                })?;
                self.supervisor.complete(run_id)?;
            }
            LoopOutcome::Denied { .. } | LoopOutcome::BudgetExhausted { .. } => {
                self.supervisor.fail(run_id, "LOOP_STOPPED")?;
            }
            LoopOutcome::AwaitingApproval { capability } => {
                self.supervisor.emit(Event::RunProgress {
                    run_id: run_id.into(),
                    detail: format!("awaiting approval for {capability:?}"),
                })?;
                // Parked, not failed.
            }
            LoopOutcome::Delegated { agent } => {
                self.supervisor.emit(Event::AgentCompleted {
                    run_id: run_id.into(),
                    agent: agent.clone(),
                })?;
                self.supervisor.complete(run_id)?;
            }
        }
        Ok(outcome)
    }

    /// Canonical-message driver: replaces the legacy string-transcript loop.
    fn drive(
        &self,
        loop_: &AgentLoop,
        chain: &ProviderChain<HttpTransport>,
        messages: &mut Vec<Message>,
        run_id: &str,
        turn: u32,
        reg: &ToolRegistry,
    ) -> Result<LoopOutcome, PantheonError> {
        if turn >= loop_.budget.max_turns {
            return Err(PantheonError::new(
                "BUDGET_EXHAUSTED",
                pantheon_core::error::Layer::Agent,
                false,
                "max_turns cap reached".to_string(),
                "raise the budget or simplify the task",
                "",
            ));
        }
        // Chain events (Attempt/Usage/Completed/Fallback/…) project into the
        // ledger through one sink — no manual model lifecycle emissions here.
        let msink = LedgerModelSink {
            sup: &self.supervisor,
            run_id,
        };
        let outcome = chain.turn_with_sink(messages, &msink)?;

        match outcome {
            pantheon_agent::TurnOutcome::Text(text) => {
                let msg = Message::assistant(&text);
                messages.push(msg.clone());
                self.supervisor.emit(Event::AssistantMessage {
                    run_id: run_id.into(),
                    message: msg,
                })?;
                Ok(LoopOutcome::Answered(text))
            }
            pantheon_agent::TurnOutcome::Tools(calls) => {
                let refs: Vec<ToolCallRef> = calls
                    .iter()
                    .enumerate()
                    .map(|(i, c)| ToolCallRef {
                        id: format!("call_{turn}_{i}"),
                        name: c.name.clone(),
                        arguments: c.args.clone(),
                    })
                    .collect();
                messages.push(Message::assistant_tool_calls(refs.clone()));
                self.supervisor.emit(Event::AssistantMessage {
                    run_id: run_id.into(),
                    message: Message::assistant_tool_calls(refs.clone()),
                })?;
                for (call, r) in calls.iter().zip(refs.iter()) {
                    // Gate on the registry's capability, not the model's claim.
                    let cap = reg
                        .capability_of(&call.name)
                        .unwrap_or(pantheon_core::capability::Capability::Other("tool".into()));
                    let gated = pantheon_agent::ToolCall {
                        name: call.name.clone(),
                        capability: cap,
                        args: call.args.clone(),
                    };
                    match pantheon_agent::gate(&loop_.policy, &gated.capability)? {
                        pantheon_agent::GateOutcome::Allow => {}
                        pantheon_agent::GateOutcome::NeedsApproval { capability } => {
                            self.supervisor.emit(Event::ApprovalRequested {
                                run_id: run_id.into(),
                                scope: format!("{capability:?}"),
                            })?;
                            return Ok(LoopOutcome::AwaitingApproval { capability });
                        }
                    }
                    self.supervisor.emit(Event::ToolStarted {
                        run_id: run_id.into(),
                        call_id: r.id.clone(),
                        tool: gated.name.clone(),
                        args: gated.args.clone(),
                    })?;
                    let out = reg.execute(&gated.name, &gated.args)?;
                    self.supervisor.emit(Event::ToolOutput {
                        run_id: run_id.into(),
                        call_id: r.id.clone(),
                        tool: gated.name.clone(),
                        truncated: false,
                    })?;
                    let tool_msg = Message::tool(r.id.clone(), out);
                    messages.push(tool_msg.clone());
                    self.supervisor.emit(Event::ToolMessage {
                        run_id: run_id.into(),
                        message: tool_msg,
                    })?;
                    self.supervisor.emit(Event::ToolCompleted {
                        run_id: run_id.into(),
                        call_id: r.id.clone(),
                        tool: gated.name.clone(),
                    })?;
                }
                self.drive(loop_, chain, messages, run_id, turn + 1, reg)
            }
            pantheon_agent::TurnOutcome::Delegate { .. } => {
                // Spawner not wired in v1; treat as structured denial.
                Err(PantheonError::new(
                    "SWARM_SPAWN_DENIED",
                    pantheon_core::error::Layer::Agent,
                    false,
                    "delegation not configured in this session".to_string(),
                    "configure a spawner or disable delegation",
                    "",
                ))
            }
        }
    }
}

/// Rebuild the canonical transcript from persisted message events.
pub fn rebuild_messages(entries: Vec<pantheon_storage::LedgerEntry>) -> Vec<Message> {
    let mut out = Vec::new();
    for e in entries {
        match e.event {
            Event::AssistantMessage { message, .. } | Event::ToolMessage { message, .. } => {
                out.push(message);
            }
            _ => {}
        }
    }
    out
}

/// Call ids that started and never completed.
pub fn unfinished_calls(entries: &[pantheon_storage::LedgerEntry]) -> Vec<String> {
    let mut started = std::collections::BTreeSet::new();
    let mut done = std::collections::BTreeSet::new();
    for e in entries {
        match &e.event {
            Event::ToolStarted { call_id, .. } => {
                started.insert(call_id.clone());
            }
            Event::ToolCompleted { call_id, .. } => {
                done.insert(call_id.clone());
            }
            _ => {}
        }
    }
    started.difference(&done).cloned().collect()
}
