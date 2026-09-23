//! Wired agent session: supervisor + provider chain + tool registry + loop.
//!
//! This is the "fully functioning harness" path: a run goes
//! start -> loop (model turns, gated tool calls, compacted output)
//! -> terminal outcome, every step event-sourced in the ledger.

use crate::Supervisor;
use pantheon_agent::{AgentLoop, Budget, LoopOutcome};
use pantheon_core::capability::Policy;
use pantheon_core::error::{Layer, PantheonError};
use pantheon_core::events::Event;
use pantheon_core::message::{Message, ToolCallRef};
use pantheon_core::model::ModelPolicy;
use pantheon_core::model_event::{ModelEvent, ModelEventSink};
use pantheon_exec::builtins::{register_builtins_with, BuiltinOptions};
use pantheon_exec::memory_tools::{
    register_memory_tools, MemoryToolEvent, MemoryToolOptions, MemoryToolSink,
};
use pantheon_exec::safewrite::register_safewrite;
use pantheon_exec::supervisor::PluginSupervisor;
use pantheon_exec::tools::ToolRegistry;
use pantheon_memory::{recall as mem_recall, LayerKind, MemoryStore};
use pantheon_providers::http::HttpTransport;
use pantheon_providers::ProviderChain;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Adapter: supervisor as the loop's event sink.
struct SupSink<'a>(&'a Supervisor);
impl<'a> pantheon_agent::EventSink for SupSink<'a> {
    fn emit(&self, event: Event) {
        if let Err(e) = self.0.emit(event) {
            eprintln!("ledger write failed: {e}");
        }
    }
}

/// Adapter: projects memory tool events into the run's ledger as
/// `MemoryProposed` rows plus `RunProgress` annotations. The runtime
/// keeps the ledger event enum thin; this is the only place tool-layer
/// facts cross into the durable stream.
///
/// Owned (Arc<Supervisor>, run_id) so it can live in an `Arc<dyn
/// MemoryToolSink>` with a `'static` bound. The supervisor is shared
/// cheaply; the `run_id` is cloned once at construction.
struct LedgerMemorySink {
    sup: Arc<Supervisor>,
    run_id: String,
}
impl MemoryToolSink for LedgerMemorySink {
    fn record(&self, event: MemoryToolEvent) {
        let detail = match &event {
            MemoryToolEvent::Recalled { query, hits } => {
                format!("memory_recall query={query:?} hits={hits}")
            }
            MemoryToolEvent::Listed { namespace, rows } => {
                format!("memory_list ns={namespace} rows={rows}")
            }
            MemoryToolEvent::Proposed {
                layer,
                namespace,
                key,
                value_len,
                origin,
            } => format!(
                "memory_propose layer={layer:?} ns={namespace} key={key} bytes={value_len} origin={origin}"
            ),
            MemoryToolEvent::Written {
                layer,
                namespace,
                key,
                backend,
            } => format!("memory_write layer={layer:?} ns={namespace} key={key} backend={backend}"),
            MemoryToolEvent::Forgotten {
                layer,
                namespace,
                key,
            } => format!("memory_forget layer={layer:?} ns={namespace} key={key}"),
            MemoryToolEvent::Denied { code, cause } => {
                format!("memory_denied code={code} cause={cause}")
            }
        };
        if let Err(e) = self.sup.emit(Event::MemoryProposed {
            run_id: self.run_id.clone(),
        }) {
            eprintln!("ledger write failed: {e}");
            return;
        }
        if let Err(e) = self.sup.emit(Event::RunProgress {
            run_id: self.run_id.clone(),
            detail,
        }) {
            eprintln!("ledger write failed: {e}");
        }
    }
}

/// Adapter: project normalized provider `ModelEvent`s into the run's
/// ledger `Event`s. The agent loop never sees providers; this is the
/// only place the provider plane meets the ledger. Exhaustion is a
/// provider-plane fact: the mapper records it as `RunProgress`; the
/// caller decides whether the run is failed.
struct LedgerModelSink<'a> {
    sup: &'a Supervisor,
    run_id: &'a str,
}
impl<'a> ModelEventSink for LedgerModelSink<'a> {
    fn emit(&self, event: ModelEvent) {
        match event.to_event(self.run_id) {
            Some(ev) => {
                if let Err(e) = self.sup.emit(ev) {
                    eprintln!("ledger write failed: {e}");
                }
            }
            None => {
                // Delta / tool-call / usage rows are provider-plane only:
                // the ledger projects visible text deltas so a crash mid-stream
                // still leaves a transcript to rebuild from, but does not
                // store high-frequency internal events.
                if let ModelEvent::TextDelta { text } = &event {
                    if let Err(e) = self.sup.emit(Event::ModelDelta {
                        run_id: self.run_id.to_string(),
                        delta: text.clone(),
                    }) {
                        eprintln!("ledger write failed: {e}");
                    }
                }
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
    /// Native SQLite + FTS5 memory store. Optional so a session can run
    /// without it; when present, recall runs before each model turn and
    /// writes go through propose -> policy -> provenance -> validation.
    pub memory: Option<Arc<MemoryStore>>,
    /// Default namespace for memory writes (agent name or project id).
    pub memory_namespace: String,
}

impl Session {
    pub fn new(
        data_dir: std::path::PathBuf,
        policy: Policy,
        model_policy: ModelPolicy,
        api_key: String,
    ) -> Result<Self, PantheonError> {
        let memory = MemoryStore::open(&data_dir.join("memory.db"))
            .ok()
            .map(Arc::new);
        Ok(Self {
            supervisor: Supervisor::open(data_dir)?,
            policy,
            model_policy,
            api_key,
            budget: Budget::default(),
            system_prompt: String::new(),
            memory,
            memory_namespace: "nyx".into(),
        })
    }

    /// Set the namespace used for memory writes (agent name, project, etc).
    pub fn with_memory_namespace(mut self, ns: impl Into<String>) -> Self {
        self.memory_namespace = ns.into();
        self
    }

    /// Per-call timeout for plugin tool calls. Overridable via env.
    fn plugin_timeout(&self) -> Duration {
        std::env::var("PANTHEON_PLUGIN_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(Duration::from_secs(30))
    }

    /// Run one task end to end. Returns the terminal outcome.
    pub fn chat(&self, run_id: &str, user_message: &str) -> Result<LoopOutcome, PantheonError> {
        match self.supervisor.ledger_status(run_id)?.as_deref() {
            Some("awaiting_approval") => {
                return Err(aerr(
                    "RUN_PARKED",
                    format!("run {run_id} is parked on approval; grant then resume"),
                ));
            }
            Some("completed") | Some("failed") => {
                return Err(aerr(
                    "RUN_TERMINAL",
                    format!("run {run_id} already finished; start a new id"),
                ));
            }
            _ => {}
        }
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
            // Memory recall: project + agent layers, narrowest first.
            // Skip for recovered runs — they have full transcript context.
            let mut recall_block = String::new();
            if let Some(mem) = &self.memory {
                if matches!(
                    self.policy
                        .check(&pantheon_core::capability::Capability::MemoryRead),
                    pantheon_core::capability::Decision::Allow
                ) {
                    let layers = [LayerKind::Project, LayerKind::Agent, LayerKind::Global];
                    if let Ok(hits) = mem_recall(mem, &self.policy, &layers, user_message, 8) {
                        for h in hits {
                            recall_block.push_str(&format!(
                                "- {}: {}
",
                                h.record.key, h.record.value
                            ));
                        }
                    }
                }
            }
            if !self.system_prompt.is_empty() {
                messages.push(Message::system(&self.system_prompt));
            }
            if !recall_block.is_empty() {
                messages.push(Message::system(format!(
                    "<memory_recall>
{recall_block}</memory_recall>"
                )));
            }
            messages.push(Message::user(user_message));
            self.supervisor.emit(Event::AssistantMessage {
                run_id: run_id.into(),
                message: Message::user(user_message),
            })?;
        }

        let mut reg = ToolRegistry::new();
        let safewrite_dir = self.supervisor.data_dir().join("safewrite");
        register_builtins_with(
            &mut reg,
            BuiltinOptions {
                safewrite_state_dir: Some(safewrite_dir.clone()),
            },
        );
        register_safewrite(&mut reg, safewrite_dir);
        if let Some(mem) = self.memory.clone() {
            let mem_sink = LedgerMemorySink {
                sup: Arc::new(self.supervisor.clone()),
                run_id: run_id.to_string(),
            };
            register_memory_tools(
                &mut reg,
                MemoryToolOptions {
                    store: mem,
                    policy: Arc::new(self.policy.clone()),
                    namespace: self.memory_namespace.clone(),
                    max_bytes: 4096,
                    sink: Arc::new(mem_sink),
                    backend_label: "native".into(),
                },
            );
        }
        // Plugin supervisors spawned for this run. They get group-killed at
        // the end of the call to avoid orphaned processes.
        let mut plugin_supers: Vec<(String, Arc<std::sync::Mutex<PluginSupervisor>>)> = Vec::new();
        // Plugin tools: discover installed plugins, verify + spawn the enabled
        // ones, register their tools behind the capability gate. A plugin
        // spawn failure is non-fatal: log it and continue without that plugin.
        let dd = self.supervisor.data_dir();
        // Project-scoped plugins live in <cwd>/.pantheon/plugins/. If the
        // session's data dir IS the cwd (single-dir use), discovery would
        // scan the same tree twice; harmless, dedup by root below.
        let project_root = std::env::current_dir().unwrap_or_else(|_| dd.clone());
        let mut discovered = pantheon_exec::plugins::discover_plugins(&dd, &project_root);
        discovered.dedup_by(|a, b| a.root == b.root);
        for plugin in &discovered {
            if !plugin.manifest.enabled {
                continue;
            }
            let runner_path = plugin.root.join(&plugin.manifest.runner);
            // Verify the manifest + runner before spawning.
            if let Err(e) = pantheon_exec::plugins::verify_plugin(plugin) {
                eprintln!(
                    "plugin '{}': verification failed, skipping: {e}",
                    plugin.manifest.name
                );
                continue;
            }
            let timeout = self.plugin_timeout();
            match pantheon_exec::supervisor::PluginSupervisor::spawn(
                &runner_path,
                &plugin.manifest,
                &dd,
                timeout,
            ) {
                Ok(sup) => {
                    let label = format!("plugin:{}", plugin.manifest.name);
                    let sup_arc = Arc::new(Mutex::new(sup));
                    pantheon_exec::supervisor::register_plugin_tools(
                        &mut reg,
                        &plugin.manifest,
                        sup_arc.clone(),
                    );
                    // Stash the supervisor so it gets stopped (group-kill) on
                    // session end instead of leaking children.
                    plugin_supers.push((label, sup_arc));
                }
                Err(e) => {
                    eprintln!(
                        "plugin '{}': spawn failed, skipping: {e}",
                        plugin.manifest.name
                    );
                }
            }
        }
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

        // Crashed-mid-tool: rebuild pending calls from the ledger. The
        // session replays their results from persisted ToolMessage rows when
        // present, and re-runs the rest before the next model call.
        let entries = self.supervisor.replay(run_id)?;
        let pending = if recovered {
            unfinished_calls(&entries)
        } else {
            Vec::new()
        };
        let grants: Vec<String> = entries
            .iter()
            .filter_map(|e| match &e.event {
                Event::ApprovalGranted { scope, .. } => Some(scope.clone()),
                _ => None,
            })
            .collect();

        // Drive the loop through the canonical-message path: each turn feeds
        // messages to the provider, appends assistant/tool rows, repeats.
        let outcome = match self.drive(
            &loop_,
            &chain,
            &mut messages,
            run_id,
            0,
            &reg,
            pending,
            &grants,
        ) {
            Ok(o) => o,
            Err(e) => {
                // Provider failures bubble up before any terminal outcome is
                // emitted. Mark the run failed so the ledger is honest about
                // why the run didn't complete; the structured error is
                // surfaced to the caller verbatim.
                let _ = self.supervisor.fail(run_id, &e.code);
                return Err(e);
            }
        };

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

        // Clean up plugin supervisors: group-kill children before return.
        for (_, sup) in &plugin_supers {
            sup.lock().unwrap().stop();
        }
        Ok(outcome)
    }

    /// Canonical-message driver: replaces the legacy string-transcript loop.
    /// `pending` holds tool calls that crashed mid-execution; `grants`
    /// records scopes the user has already approved.
    fn drive(
        &self,
        loop_: &AgentLoop,
        chain: &ProviderChain<HttpTransport>,
        messages: &mut Vec<Message>,
        run_id: &str,
        turn: u32,
        reg: &ToolRegistry,
        pending: Vec<String>,
        grants: &[String],
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
        // Recovered pending tool calls settle first, before the next model
        // call. Tools already completed (matching ToolMessage rows in the
        // ledger) are skipped; tools that crashed before completing run.
        if !pending.is_empty() {
            let pending_set: std::collections::BTreeSet<String> = pending.into_iter().collect();
            let mut not_done: Vec<String> = Vec::new();
            for cid in &pending_set {
                // If a ToolMessage row exists for this call id, it's done.
                let done = messages.iter().any(|m| {
                    m.role == pantheon_core::message::Role::Tool
                        && m.tool_call_id.as_deref() == Some(cid.as_str())
                });
                if !done {
                    not_done.push(cid.clone());
                }
            }
            for cid in not_done {
                if grants.iter().any(|s| s == &cid) {
                    // Already approved via ApprovalGranted.
                } else {
                    // Pending re-execution without a grant: surface as
                    // approval-needed. We do not know the capability from
                    // the ledger alone (calls are anonymous until re-run),
                    // so we ask with the conservative "Other" capability.
                    let cap =
                        pantheon_core::capability::Capability::Other(format!("recover:{cid}"));
                    self.supervisor.emit(Event::ApprovalRequested {
                        run_id: run_id.into(),
                        scope: cid.clone(),
                    })?;
                    return Ok(LoopOutcome::AwaitingApproval { capability: cap });
                }
            }
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
                            // Approval scope MUST be the call id, not the
                            // capability: granting one shell call must not
                            // automatically grant the next one.
                            self.supervisor.emit(Event::ApprovalRequested {
                                run_id: run_id.into(),
                                scope: r.id.clone(),
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
                self.drive(
                    loop_,
                    chain,
                    messages,
                    run_id,
                    turn + 1,
                    reg,
                    Vec::new(),
                    grants,
                )
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

fn aerr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(code, Layer::Agent, false, cause, "see ledger status", "")
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

#[cfg(test)]
mod tests {
    use super::*;
    use pantheon_core::model_event::ModelEvent;

    #[test]
    fn streaming_deltas_persist_as_model_delta_rows() {
        // The sink must persist TextDelta events as ModelDelta rows even
        // though to_event() returns None for them (high-frequency provider-plane
        // events are not full Event variants).
        let dir = std::env::temp_dir().join(format!("pantheon-rt-sess-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sup = Supervisor::open(dir).unwrap();
        sup.start_run("run_stream").unwrap();
        let sink = LedgerModelSink {
            sup: &sup,
            run_id: "run_stream",
        };
        sink.emit(ModelEvent::Attempt {
            provider: "router".into(),
            model: "chat".into(),
            chain_index: 0,
            streaming: true,
        });
        sink.emit(ModelEvent::TextDelta {
            text: "part1".into(),
        });
        sink.emit(ModelEvent::TextDelta {
            text: "part2".into(),
        });
        sink.emit(ModelEvent::Usage {
            usage: pantheon_core::model_event::ModelUsage {
                input_tokens: 3,
                output_tokens: 6,
                total_tokens: 9,
                cost_usd: None,
            },
        });
        sink.emit(ModelEvent::Completed {
            finish_reason: Some("stop".into()),
        });

        let entries = sup.replay("run_stream").unwrap();
        let deltas: Vec<String> = entries
            .iter()
            .filter_map(|e| match &e.event {
                Event::ModelDelta { delta, .. } => Some(delta.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["part1".to_string(), "part2".to_string()]);
        // Other events also projected.
        // Event order: RunStarted (from start_run) -> Attempt/ModelRequested,
        // two TextDelta->ModelDelta, then Completed->ModelCompleted.
        // Usage events are provider-plane only (to_event returns None) and
        // are NOT persisted to the ledger -- by design.
        let kinds: Vec<&str> = entries
            .iter()
            .map(|e| match &e.event {
                Event::RunStarted { .. } => "start",
                Event::ModelRequested { .. } => "req",
                Event::ModelDelta { .. } => "delta",
                Event::ModelCompleted { .. } => "done",
                _ => "skip",
            })
            .collect();
        assert_eq!(kinds, vec!["start", "req", "delta", "delta", "done"]);
        // rebuild_messages skips deltas (they are not full messages), but
        // the transcript still has the persisted content for inspection.
        let msgs = rebuild_messages(entries);
        assert!(msgs.is_empty(), "no full assistant/user rows emitted");
    }
}
