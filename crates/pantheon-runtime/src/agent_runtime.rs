//! Agent runtime: the seam between a resolved agent profile and a run.
//!
//! # Why this exists
//!
//! Before this module, a Pantheon run had no owner. `Config.profile` was
//! parsed and then read by nothing; `run_id` was a bare uuid; the memory
//! namespace was a caller-supplied string. Two consequences followed, and
//! both were silent:
//!
//! 1. A resumed session could be handed to a different agent without
//!    anything recording the change.
//! 2. Memory isolation was a convention (`namespace: "nyx"`), not a
//!    boundary. The model picked the namespace in the tool arguments, so a
//!    model that named another agent's namespace would have been served.
//!
//! This module makes the profile the unit of identity. One
//! [`AgentRuntime`] owns a resolved profile, binds runs to it, refuses
//! cross-profile memory, and hands out the delegation tool that runs
//! sub-agents *in process* against the same [`Supervisor`].
//!
//! # Relationship to the profile layer
//!
//! `pantheon_agent::agent_profile` owns declaration, inheritance, and
//! provenance. This module owns the consequences of a resolved profile at
//! runtime. It deliberately does not re-read config: a caller resolves once
//! and hands the `EffectiveProfile` in, so there is exactly one code path
//! that decides what a profile means.
//!
//! # Relationship to the collaboration store
//!
//! [`pantheon_storage::CollaborationStore`] is the durable record of who
//! delegated what. This module is the only writer for delegation, so every
//! task, message, and tool call made by another agent passes through it and
//! is attributable. Nothing here keeps collaboration state in memory.

use pantheon_agent::agent_profile::{EffectiveProfile, ProfileError, ProfileRegistry};
use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::events::Event;
use pantheon_storage::{
    AgentMessage, AgentTask, CollaborationStore, MessageKind, TaskConflict, TaskMutationError,
    TaskStatus,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::Supervisor;

/// The profile every run falls back to when nothing selects one.
pub const DEFAULT_PROFILE: &str = "default";

fn aerr(code: &str, cause: String, remediation: &str) -> PantheonError {
    PantheonError::new(code, Layer::Runtime, false, cause, remediation, "")
}

/// One agent, resolved and ready to run.
///
/// Cheap to clone: an `Arc` over the shared parts. A session, a delegated
/// sub-agent, and the TUI all hold one, and they all see the same identity.
#[derive(Clone)]
pub struct AgentRuntime {
    inner: Arc<AgentRuntimeInner>,
}

struct AgentRuntimeInner {
    supervisor: Supervisor,
    /// Swarm caps and live-agent accounting, shared by *every* agent
    /// runtime in the process.
    ///
    /// Held behind an `Arc` so `for_profile` hands a peer the same
    /// accounting, not a private copy. A fresh `Swarm` per call or per
    /// agent would reset `live` and `spawned_total` to zero, so
    /// `max_concurrent` and `max_total_agents` could never fire and a
    /// peer could never release the coordinator's slot — the cap check
    /// would report enforcement that did not exist.
    swarm: Arc<Mutex<pantheon_swarm::Swarm>>,
    profiles: ProfileRegistry,
    /// The profile this runtime *is*. Not a preference: it is the identity
    /// bound to every run and the only memory namespace this agent may use.
    effective: EffectiveProfile,
    collaboration: CollaborationStore,
    data_dir: PathBuf,
    /// Policy preset the profile resolved to. Kept as a string so the
    /// capability layer (which lives in the terminal crate) stays the single
    /// place that maps a preset name to a `Policy`.
    policy_preset: String,
}

impl AgentRuntime {
    /// Build a runtime for an already-resolved profile.
    ///
    /// The caller resolves the profile (config loading, `--agent`, a TUI
    /// switch) and passes the result. Taking `EffectiveProfile` rather than
    /// a name is deliberate: an unresolved profile cannot reach a run.
    pub fn new(
        supervisor: Supervisor,
        profiles: ProfileRegistry,
        effective: EffectiveProfile,
        data_dir: impl Into<PathBuf>,
    ) -> Result<Self, PantheonError> {
        let policy_preset = effective.policy.value.clone();
        let data_dir = data_dir.into();
        let collaboration = CollaborationStore::open(&data_dir.join("collaboration.db"))?;
        Ok(Self {
            inner: Arc::new(AgentRuntimeInner {
                supervisor,
                // One swarm per process, shared by every profile in it, so
                // caps bound the whole run rather than each call.
                swarm: Arc::new(Mutex::new(pantheon_swarm::Swarm::new(
                    pantheon_swarm::Caps::default(),
                ))),
                profiles,
                effective,
                collaboration,
                data_dir,
                policy_preset,
            }),
        })
    }

    /// Every declared profile, for the TUI's profile list and for resolving
    /// a delegation target.
    pub fn profiles(&self) -> &ProfileRegistry {
        &self.inner.profiles
    }

    /// A second runtime for a peer profile, sharing this one's supervisor
    /// and data dir. This is how a sub-agent runs *in process*: same
    /// ledger, same event stream, same collaboration database, different
    /// identity. Not a subprocess, and not a second runtime subsystem.
    pub fn for_profile(&self, name: &str) -> Result<Self, PantheonError> {
        let effective = self
            .inner
            .profiles
            .resolve(name, &self.inner.policy_preset)
            .map_err(profile_err)?;
        // Read the preset before the profile moves into the struct: a peer
        // resolves its own policy, so a read-only agent must never inherit
        // the coordinator's write capabilities.
        let policy_preset = effective.policy.value.clone();
        Ok(Self {
            inner: Arc::new(AgentRuntimeInner {
                supervisor: self.inner.supervisor.clone(),
                // Shared, not copied: a peer settling its own task has to be
                // able to release the slot the coordinator took for it.
                swarm: Arc::clone(&self.inner.swarm),
                profiles: self.inner.profiles.clone(),
                effective,
                collaboration: CollaborationStore::open(
                    &self.inner.data_dir.join("collaboration.db"),
                )?,
                data_dir: self.inner.data_dir.clone(),
                policy_preset,
            }),
        })
    }

    /// The profile this runtime is.
    pub fn profile(&self) -> &EffectiveProfile {
        &self.inner.effective
    }

    /// The stable identity, for logs and TUI headers.
    pub fn identity(&self) -> String {
        self.inner.effective.identity()
    }

    /// The memory namespace this agent owns. The only one it may use.
    pub fn memory_namespace(&self) -> &str {
        &self.inner.effective.memory_namespace.value
    }

    /// The policy preset name this profile resolved to. The CLI maps this
    /// to a `Policy`; the runtime carries it so the mapping has one home.
    pub fn policy_preset(&self) -> &str {
        &self.inner.policy_preset
    }

    pub fn supervisor(&self) -> &Supervisor {
        &self.inner.supervisor
    }

    pub fn collaboration(&self) -> &CollaborationStore {
        &self.inner.collaboration
    }

    /// Bind a run to this agent, and refuse if it already belongs to
    /// someone else.
    ///
    /// This is the "resume must not leak session state" guarantee, enforced
    /// at the one place a run acquires identity. A TUI `/agent zeus` that
    /// then resumes a Nyx run fails here with `AGENT_RUN_CONFLICT` instead
    /// of quietly replaying another agent's transcript into a new profile.
    pub fn bind_run(&self, run_id: &str) -> Result<(), PantheonError> {
        let existing = self.inner.supervisor.ledger_run_agent(run_id)?;
        if let Some(bound) = existing {
            if bound != self.inner.effective.agent_id {
                return Err(aerr(
                    "AGENT_RUN_CONFLICT",
                    format!(
                        "run {run_id} belongs to agent {bound}, not {}; \
                         switch back to that agent or start a new conversation",
                        self.inner.effective.agent_id
                    ),
                    "resume the run as its own agent, or begin a new run",
                ));
            }
            return Ok(());
        }
        self.inner.supervisor.emit(Event::AgentBound {
            run_id: run_id.to_string(),
            agent_id: self.inner.effective.agent_id.clone(),
            profile: self.inner.effective.name.clone(),
        })
    }

    /// The agent a run belongs to, if it is bound.
    pub fn run_agent(&self, run_id: &str) -> Result<Option<String>, PantheonError> {
        self.inner.supervisor.ledger_run_agent(run_id)
    }

    // ------------------------------------------------------------ delegation

    /// Delegate a task to another profile, creating the collaboration and
    /// the first task if this is the first delegation in it.
    ///
    /// Returns the created task. The caller (the model, through the
    /// delegation tool) gets a task id it can poll, so a long-running
    /// sub-agent does not block the parent turn.
    pub fn delegate(
        &self,
        collaboration_id: &str,
        objective: &str,
        target: &str,
        task_id: &str,
    ) -> Result<AgentTask, PantheonError> {
        let coordinator = self.inner.effective.name.as_str();
        if target == coordinator {
            return Err(aerr(
                "DELEGATE_SELF",
                format!("{coordinator} cannot delegate to itself; run the work inline"),
                "name a different agent profile",
            ));
        }
        // The target must be a declared profile. Refusing here is what keeps
        // a typo'd name from creating a task that can never be executed.
        let target_effective = self.resolve_target(target).map_err(|e| {
            aerr(
                "DELEGATE_UNKNOWN_AGENT",
                e,
                "declare the target in [agents.<name>]",
            )
        })?;

        // Swarm caps first: a refusal must not leave a half-created task.
        let model = target_effective
            .model
            .value
            .clone()
            .unwrap_or_else(|| "default".to_string());
        self.spawn_in_swarm(target, &model)?;

        // Create the collaboration on first use so a coordinator's objective
        // is recorded once rather than per-task.
        if self
            .inner
            .collaboration
            .collaboration(collaboration_id)?
            .is_none()
        {
            self.inner.collaboration.create_collaboration(
                collaboration_id,
                coordinator,
                objective,
            )?;
        }
        let task = self.inner.collaboration.create_task(
            task_id,
            Some(collaboration_id),
            coordinator,
            Some(target),
            None,
            objective,
        )?;
        // The delegation itself is a message, so "what was sent to whom"
        // is answerable from the conversation trail alone.
        let message_id = format!("{task_id}-assign");
        self.inner.collaboration.record_message(
            &message_id,
            Some(collaboration_id),
            Some(task_id),
            coordinator,
            target,
            MessageKind::Delegation,
            objective,
        )?;
        Ok(task)
    }

    /// The tool surface a profile can use to reach its peers. Gated on
    /// `AgentSpawn` in the *calling* agent's policy, so an agent whose
    /// policy forbids spawning simply has no such tool.
    pub fn register_delegate_tool(&self, reg: &mut pantheon_tools::tools::ToolRegistry) {
        let me = self.clone();
        reg.register(
            pantheon_api::message::ToolSchema {
                name: "agent_delegate".into(),
                description: "Delegate a unit of work to another agent profile. \
                              Returns a task id to poll with agent_task. \
                              The other agent runs under its own permissions, \
                              never yours."
                    .into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string", "description": "Id you choose for this task."},
                        "target": {"type": "string", "description": "Profile name of the agent to delegate to."},
                        "objective": {"type": "string", "description": "What the other agent should do."},
                        "collaboration_id": {"type": "string", "description": "Shared objective id (default: 'collab')."}
                    },
                    "required": ["task_id", "target", "objective"]
                }),
            },
            pantheon_api::capability::Capability::AgentSpawn,
            move |args| {
                let v: serde_json::Value =
                    serde_json::from_str(args).map_err(|e| aerr("DELEGATE_ARGS", e.to_string(), ""))?;
                let get = |k: &str| -> Result<String, PantheonError> {
                    v.get(k)
                        .and_then(|x| x.as_str())
                        .map(str::to_string)
                        .ok_or_else(|| aerr("DELEGATE_ARGS", format!("missing {k}"), ""))
                };
                let task_id = get("task_id")?;
                let target = get("target")?;
                let objective = get("objective")?;
                let collab = v
                    .get("collaboration_id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("collab")
                    .to_string();
                match me.delegate(&collab, &objective, &target, &task_id) {
                    Ok(t) => Ok(format!("delegated {} to {} (task {})", t.task_id, target, t.task_id)),
                    Err(e) => Ok(format!("delegation refused: {}", e.cause)),
                }
            },
        );

        // Polling tool. Separate from delegation so a coordinator can check
        // on work it handed off without holding its turn open.
        let me2 = self.clone();
        reg.register(
            pantheon_api::message::ToolSchema {
                name: "agent_task".into(),
                description: "Read the status and result of a delegated task by id. \
                              Use this to collect results, review work, or decide on a retry."
                    .into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task_id": {"type": "string", "description": "The delegated task id."}
                    },
                    "required": ["task_id"]
                }),
            },
            pantheon_api::capability::Capability::AgentSpawn,
            move |args| {
                let v: serde_json::Value = serde_json::from_str(args)
                    .map_err(|e| aerr("DELEGATE_ARGS", e.to_string(), ""))?;
                let id = v
                    .get("task_id")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| aerr("DELEGATE_ARGS", "missing task_id".into(), ""))?;
                match me2.task_status(id) {
                    Ok(Some(t)) => Ok(format!(
                        "{id}: {} ({}){}",
                        t.status,
                        t.assigned_agent.as_deref().unwrap_or("unassigned"),
                        t.result
                            .as_deref()
                            .map(|r| format!(" result: {r}"))
                            .unwrap_or_default()
                    )),
                    Ok(None) => Ok(format!("{id}: no such task")),
                    Err(e) => Ok(format!("{id}: {}", e.cause)),
                }
            },
        );
    }

    /// Read one task's current state.
    pub fn task_status(&self, task_id: &str) -> Result<Option<AgentTask>, PantheonError> {
        self.inner.collaboration.task(task_id)
    }

    /// Report this agent's work on a task it was assigned.
    ///
    /// `result` and `error` are recorded verbatim as *data*. The settle
    /// path is the same for every agent: there is no variant of this call
    /// that grants the reporter anything the reporter's own policy lacks.
    pub fn complete_task(&self, task_id: &str, result: &str) -> Result<AgentTask, PantheonError> {
        let current = self
            .inner
            .collaboration
            .task(task_id)?
            .ok_or_else(|| aerr("TASK_NOT_FOUND", format!("no task {task_id}"), ""))?;
        // Only the assignee settles its own task. A coordinator that
        // completes someone else's work would erase the audit trail of who
        // actually did it.
        if current.assigned_agent.as_deref() != Some(self.inner.effective.name.as_str()) {
            return Err(aerr(
                "TASK_NOT_OWNED",
                format!(
                    "task {task_id} is assigned to {}, not {}",
                    current.assigned_agent.as_deref().unwrap_or("nobody"),
                    self.inner.effective.name
                ),
                "only the assigned agent may report a result",
            ));
        }
        let settled = self
            .inner
            .collaboration
            .transition(
                task_id,
                Some(current.version),
                TaskStatus::Completed,
                Some(result),
                None,
            )
            .map_err(task_err)?;
        if let Some(who) = &settled.assigned_agent {
            // Free the concurrency slot. Without this, a long session
            // delegating many small tasks would hit `max_concurrent` with
            // nothing actually running.
            self.retire_from_swarm(who);
        }
        Ok(settled)
    }

    /// Report failure on a task this agent owns.
    pub fn fail_task(&self, task_id: &str, error: &str) -> Result<AgentTask, PantheonError> {
        let current = self
            .inner
            .collaboration
            .task(task_id)?
            .ok_or_else(|| aerr("TASK_NOT_FOUND", format!("no task {task_id}"), ""))?;
        if current.assigned_agent.as_deref() != Some(self.inner.effective.name.as_str()) {
            return Err(aerr(
                "TASK_NOT_OWNED",
                format!("task {task_id} is assigned to someone else"),
                "only the assigned agent may report failure",
            ));
        }
        let settled = self
            .inner
            .collaboration
            .transition(
                task_id,
                Some(current.version),
                TaskStatus::Failed,
                None,
                Some(error),
            )
            .map_err(task_err)?;
        if let Some(who) = &settled.assigned_agent {
            self.retire_from_swarm(who);
        }
        Ok(settled)
    }

    /// Send a structured message to another agent.
    pub fn send(
        &self,
        message_id: &str,
        recipient: &str,
        kind: MessageKind,
        content: &str,
        task_id: Option<&str>,
    ) -> Result<AgentMessage, PantheonError> {
        self.inner.collaboration.record_message(
            message_id,
            Some("collab"),
            task_id,
            &self.inner.effective.name,
            recipient,
            kind,
            content,
        )
    }

    /// This agent's unread mail, oldest first.
    pub fn inbox(&self) -> Result<Vec<AgentMessage>, PantheonError> {
        self.inner.collaboration.inbox(&self.inner.effective.name)
    }

    /// Resolve a peer profile for delegation. Fails closed: an undeclared
    /// name never gets a synthesized profile.
    fn resolve_target(&self, target: &str) -> Result<EffectiveProfile, String> {
        if self.inner.profiles.get(target).is_none() {
            return Err(format!(
                "no agent profile named {target:?} is declared; \
                 add [agents.{target}] to the config"
            ));
        }
        self.inner
            .profiles
            .resolve(target, &self.inner.policy_preset)
            .map_err(|e| format!("{e}"))
    }

    /// Check the caps and take a slot, against the shared swarm.
    ///
    /// Caps are runtime-owned, not agent-chosen: a delegating agent cannot
    /// widen its own limits by asking for them. The lock is held only for
    /// the check-and-insert, never across task creation or a model call.
    fn spawn_in_swarm(&self, target: &str, model: &str) -> Result<(), PantheonError> {
        let mut swarm = self.inner.swarm.lock().map_err(|_| {
            aerr(
                "SWARM_STATE_POISONED",
                "swarm accounting lock is poisoned".into(),
                "restart pantheon",
            )
        })?;
        swarm.spawn(target, 0, model).map(|_| ()).map_err(|e| {
            PantheonError::new(
                "SWARM_SPAWN_DENIED",
                Layer::Agent,
                false,
                e.cause,
                e.remediation,
                "",
            )
        })
    }

    /// Retire a sub-agent and fold its usage into the shared counters, so a
    /// finished delegation frees its concurrency slot.
    fn retire_from_swarm(&self, target: &str) {
        if let Ok(mut swarm) = self.inner.swarm.lock() {
            swarm.complete(target, 0, 0, 0);
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.inner.data_dir
    }
}

fn task_err(e: TaskMutationError) -> PantheonError {
    let (code, cause) = match &e {
        TaskMutationError::Conflict(TaskConflict::Missing { task_id }) => {
            ("TASK_NOT_FOUND", format!("no task {task_id}"))
        }
        TaskMutationError::Conflict(TaskConflict::Version {
            task_id,
            expected,
            actual,
        }) => (
            "TASK_VERSION_CONFLICT",
            format!(
                "task {task_id} moved on (expected version {expected}, now {})",
                actual
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "gone".into())
            ),
        ),
        TaskMutationError::Conflict(TaskConflict::Transition { task_id, from, to }) => (
            "TASK_TRANSITION",
            format!("task {task_id} cannot move from {from} to {to}"),
        ),
        TaskMutationError::Storage(e) => ("TASK_STORAGE", e.clone()),
    };
    aerr(code, cause, "reload the task state and retry")
}

#[cfg(test)]
#[path = "agent_runtime_tests.rs"]
mod tests;

/// Turn a profile resolution error into a runtime error, preserving the
/// distinction between "no such profile" and "that profile is broken".
pub fn profile_err(e: ProfileError) -> PantheonError {
    aerr(
        "PROFILE_INVALID",
        e.to_string(),
        "fix the [agents.<name>] table",
    )
}
