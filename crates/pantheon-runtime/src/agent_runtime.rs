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
    swarm: Arc<Mutex<crate::swarm::Swarm>>,
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
                swarm: Arc::new(Mutex::new(crate::swarm::Swarm::new(
                    crate::swarm::Caps::default(),
                ))),
                profiles,
                effective,
                collaboration,
                data_dir,
                policy_preset,
            }),
        })
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

    /// Persona file path declared by this profile, if any. Read verbatim
    /// into a delegated child's system prompt — the child cannot inherit
    /// the parent's context, so the persona must travel with the spawn.
    pub fn soul_file(&self) -> Option<&str> {
        self.inner.effective.soul_file.value.as_deref()
    }

    /// User-context file path declared by this profile, if any. Read
    /// verbatim into the main session's system prompt alongside the
    /// persona — it is part of the identity, not a layer.
    pub fn user_file(&self) -> Option<&str> {
        self.inner.effective.user_file.value.as_deref()
    }

    /// Layered instruction files, parent first then child, each tagged
    /// with the profile that contributed it: `(profile, path)`. Inlined
    /// verbatim into a delegated child's system prompt, same reason.
    pub fn instruction_files(&self) -> &[(String, String)] {
        &self.inner.effective.agents_files
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

    /// Which agent owns a run, if any. `None` is a real answer for runs
    /// created before agent profiles existed — never default it.
    pub fn run_agent(&self, run_id: &str) -> Result<Option<String>, PantheonError> {
        self.inner.supervisor.ledger_run_agent(run_id)
    }

    /// Report this agent's work on a task it was assigned.
    ///
    /// `result` is recorded verbatim as *data*. Only the assignee settles
    /// its own task: a coordinator that completed someone else's work would
    /// erase the audit trail of who actually did it.
    pub fn complete_task(&self, task_id: &str, result: &str) -> Result<AgentTask, PantheonError> {
        let current = self
            .inner
            .collaboration
            .task(task_id)?
            .ok_or_else(|| aerr("TASK_NOT_FOUND", format!("no task {task_id}"), ""))?;
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

    /// This agent's unread mail, oldest first.
    pub fn inbox(&self) -> Result<Vec<AgentMessage>, PantheonError> {
        self.inner.collaboration.inbox(&self.inner.effective.name)
    }

    /// Send a message to another agent profile. The message is data, never
    /// instruction: kinds are a closed set with no authority variant, and
    /// provenance always names the sender.
    pub fn send(
        &self,
        message_id: &str,
        to: &str,
        kind: MessageKind,
        body: &str,
        task_id: Option<&str>,
    ) -> Result<AgentMessage, PantheonError> {
        self.inner.collaboration.record_message(
            message_id,
            None,
            task_id,
            &self.inner.effective.name,
            to,
            kind,
            body,
        )
    }
    fn retire_from_swarm(&self, target: &str) {
        if let Ok(mut swarm) = self.inner.swarm.lock() {
            swarm.complete(target, 0, 0, 0);
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.inner.data_dir
    }
}

/// Turn a task-store mutation error into a runtime error, preserving the
/// distinction between "no such task", "someone else moved it", and "the
/// store itself failed".
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

/// Turn a profile resolution error into a runtime error, preserving the
/// distinction between "no such profile" and "that profile is broken".
pub fn profile_err(e: ProfileError) -> PantheonError {
    aerr(
        "PROFILE_INVALID",
        e.to_string(),
        "fix the [agents.<name>] table",
    )
}
