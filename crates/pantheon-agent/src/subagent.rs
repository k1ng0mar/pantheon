//! In-process sub-agent handles: non-blocking delegation.
//!
//! `delegate` no longer stops the parent loop. The spawner returns a
//! handle immediately (`delegate[researcher]: spawned, handle h_1`) and
//! the parent keeps working while children run on their own threads.
//! The model follows up through three tools:
//!
//! - `subagent_read(handle)` — the child's transcript so far; never blocks.
//! - `subagent_wait(handle)` — blocks until that child finishes, returns
//!   its final result.
//! - `subagent_list()` — every live handle with its status.
//!
//! [`SubagentRegistry`] is the threaded in-process implementation:
//! children run `FnOnce` work closures on spawned threads, appending to
//! a shared transcript the parent can read live. Spawn enforces
//! [`SubagentCaps`]; every violation is a structured refusal error,
//! never silent.

use pantheon_api::error::{Layer, PantheonError};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

fn derr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Agent,
        false,
        cause,
        "raise the cap explicitly or reduce delegation depth",
        "",
    )
}

/// Handle to a live (or finished) sub-agent. `id` looks like `h_1`;
/// `name` is the agent/profile name the parent delegated to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentHandle {
    pub id: String,
    pub name: String,
}

/// Liveness of one sub-agent, as reported by `subagent_list` and the
/// spawner's `subagent_status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentStatus {
    Working,
    Done,
    Failed,
}

impl SubagentStatus {
    /// Wire spelling used by `subagent_list` JSON and the HTTP swarm API.
    pub fn as_str(&self) -> &'static str {
        match self {
            SubagentStatus::Working => "working",
            SubagentStatus::Done => "done",
            SubagentStatus::Failed => "failed",
        }
    }
}

/// Caps enforced at spawn by the in-process registry. Mirrors the
/// corresponding `pantheon_runtime::swarm::Caps` fields; pantheon-agent
/// deliberately does not depend on pantheon-runtime (see the
/// circular-dep note in engine_tests.rs), so keep the field meanings
/// and defaults in sync with `swarm::Caps`.
#[derive(Debug, Clone)]
pub struct SubagentCaps {
    /// Maximum delegation depth (primary = 0). Deeper spawns refused.
    pub max_depth: u32,
    /// Maximum concurrently working sub-agents in this registry.
    pub max_concurrent: u32,
    /// Per-agent spawn cap: children one agent may spawn.
    pub max_subagents: u32,
    /// When false, a child (depth >= 1) attempting to delegate is refused.
    pub allow_child_spawn: bool,
}

impl Default for SubagentCaps {
    fn default() -> Self {
        Self {
            max_depth: 2,
            max_concurrent: 4,
            max_subagents: 4,
            allow_child_spawn: true,
        }
    }
}

/// Handle-based spawner: `spawn_handle` returns immediately; the child
/// works in parallel with the parent. `subagent_wait` is the only
/// blocking call; `subagent_read` never blocks.
pub trait SubagentSpawner {
    /// Spawn a child now; returns its handle immediately.
    /// Cap violations are structured refusal errors (`SWARM_*`).
    fn spawn_handle(
        &self,
        agent: &str,
        model: &str,
        task: &str,
        depth: u32,
    ) -> Result<SubagentHandle, PantheonError>;
    /// Current status of one handle; unknown handle is an error.
    fn subagent_status(&self, handle: &str) -> Result<SubagentStatus, PantheonError>;
    /// Block until the child finishes; returns its final result text
    /// (or the child's error).
    fn subagent_wait(&self, handle: &str) -> Result<String, PantheonError>;
    /// The child's transcript so far. Never blocks.
    fn subagent_read(&self, handle: &str) -> Result<String, PantheonError>;
    /// Every known handle with its status, in spawn order.
    fn subagent_list(&self) -> Vec<(SubagentHandle, SubagentStatus)>;
}

struct ChildEntry {
    handle: SubagentHandle,
    transcript: Arc<Mutex<Vec<String>>>,
    /// (status, result): result is Some once the child settled.
    state: Arc<(
        Mutex<(SubagentStatus, Option<Result<String, String>>)>,
        Condvar,
    )>,
}

struct RegistryState {
    children: HashMap<String, ChildEntry>,
    order: Vec<String>,
    spawned_by_parent: HashMap<String, u32>,
}

/// Threaded in-process [`SubagentSpawner`]: each child runs its work
/// closure on a spawned thread, appending progress to a shared
/// transcript the parent reads live via `subagent_read`.
///
/// The registry itself does not know how to *build* child work — the
/// `work` closure is supplied per spawn, so hosts (the engine test
/// harness, the runtime's session spawner) decide what a child runs.
pub struct SubagentRegistry {
    state: Mutex<RegistryState>,
    next_id: AtomicU64,
    caps: SubagentCaps,
}

impl SubagentRegistry {
    pub fn new(caps: SubagentCaps) -> Self {
        Self {
            state: Mutex::new(RegistryState {
                children: HashMap::new(),
                order: Vec::new(),
                spawned_by_parent: HashMap::new(),
            }),
            next_id: AtomicU64::new(1),
            caps,
        }
    }

    /// Spawn a child running `work` on a new thread. The transcript is
    /// seeded with the task; `work` appends progress lines itself.
    /// Caps are enforced before the thread starts; violations are
    /// structured `SWARM_*` refusals.
    pub fn spawn_child(
        &self,
        parent: &str,
        agent: &str,
        parent_depth: u32,
        task: &str,
        work: impl FnOnce() -> Result<String, PantheonError> + Send + 'static,
    ) -> Result<SubagentHandle, PantheonError> {
        let child_depth = parent_depth + 1;
        if child_depth > self.caps.max_depth {
            return Err(derr(
                "SWARM_MAX_DEPTH",
                format!("depth {child_depth} exceeds max {}", self.caps.max_depth),
            ));
        }
        if !self.caps.allow_child_spawn && parent_depth >= 1 {
            return Err(derr(
                "SWARM_CHILD_SPAWN_DENIED",
                format!(
                    "child at depth {parent_depth} may not delegate (allow_child_spawn = false)"
                ),
            ));
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let live = state
            .children
            .values()
            .filter(|c| {
                c.state
                    .0
                    .lock()
                    .map(|s| s.0 == SubagentStatus::Working)
                    .unwrap_or(false)
            })
            .count() as u32;
        if live >= self.caps.max_concurrent {
            return Err(derr(
                "SWARM_MAX_CONCURRENT",
                format!("{live} live agents, max {}", self.caps.max_concurrent),
            ));
        }
        let by_parent = state.spawned_by_parent.get(parent).copied().unwrap_or(0);
        if by_parent >= self.caps.max_subagents {
            return Err(derr(
                "SWARM_PER_AGENT_CAP",
                format!(
                    "agent {parent} spawned {by_parent}, per-agent cap {}",
                    self.caps.max_subagents
                ),
            ));
        }
        let id = format!("h_{}", self.next_id.fetch_add(1, Ordering::SeqCst));
        let handle = SubagentHandle {
            id: id.clone(),
            name: agent.to_string(),
        };
        let transcript = Arc::new(Mutex::new(vec![format!("task: {task}")]));
        let child_state = Arc::new((
            Mutex::new((SubagentStatus::Working, None::<Result<String, String>>)),
            Condvar::new(),
        ));
        let entry = ChildEntry {
            handle: handle.clone(),
            transcript: Arc::clone(&transcript),
            state: Arc::clone(&child_state),
        };
        state.children.insert(id.clone(), entry);
        state.order.push(id);
        *state
            .spawned_by_parent
            .entry(parent.to_string())
            .or_insert(0) += 1;
        drop(state);

        let t_log = Arc::clone(&transcript);
        let st = Arc::clone(&child_state);
        std::thread::spawn(move || {
            let outcome = work();
            let line = match &outcome {
                Ok(text) => format!("result: {text}"),
                Err(e) => format!("error[{}]: {}", e.code, e.cause),
            };
            t_log
                .lock()
                .map(|mut t| t.push(line))
                .unwrap_or_else(|e| e.into_inner().push("result: <transcript poisoned>".into()));
            let (lock, cvar) = &*st;
            let status = if outcome.is_ok() {
                SubagentStatus::Done
            } else {
                SubagentStatus::Failed
            };
            let stored: Result<String, String> =
                outcome.map_err(|e| format!("[{}] {}", e.code, e.cause));
            if let Ok(mut s) = lock.lock() {
                s.0 = status;
                s.1 = Some(stored);
            }
            cvar.notify_all();
        });
        Ok(handle)
    }

    fn entry_state(
        &self,
        handle: &str,
    ) -> Result<
        Arc<(
            Mutex<(SubagentStatus, Option<Result<String, String>>)>,
            Condvar,
        )>,
        PantheonError,
    > {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .children
            .get(handle)
            .map(|e| Arc::clone(&e.state))
            .ok_or_else(|| {
                derr(
                    "SWARM_UNKNOWN_HANDLE",
                    format!("unknown sub-agent handle {handle:?}"),
                )
            })
    }
}

impl SubagentSpawner for SubagentRegistry {
    fn spawn_handle(
        &self,
        agent: &str,
        _model: &str,
        task: &str,
        depth: u32,
    ) -> Result<SubagentHandle, PantheonError> {
        // A bare registry has no model runner: the threaded child echoes
        // the task as its result. Hosts that run real children (the
        // runtime's session spawner) use `spawn_child` with real work.
        let task = task.to_string();
        self.spawn_child("", agent, depth, &task.clone(), move || {
            Ok(format!("(echo) {task}"))
        })
    }

    fn subagent_status(&self, handle: &str) -> Result<SubagentStatus, PantheonError> {
        let st = self.entry_state(handle)?;
        st.0.lock()
            .map(|s| s.0)
            .map_err(|_| derr("SWARM_REGISTRY", "registry lock poisoned".into()))
    }

    fn subagent_wait(&self, handle: &str) -> Result<String, PantheonError> {
        let st = self.entry_state(handle)?;
        let (lock, cvar) = &*st;
        let mut guard = lock
            .lock()
            .map_err(|_| derr("SWARM_REGISTRY", "registry lock poisoned".into()))?;
        while guard.1.is_none() {
            guard = cvar
                .wait(guard)
                .map_err(|_| derr("SWARM_REGISTRY", "wait interrupted".into()))?;
        }
        match guard.1.clone().expect("checked above") {
            Ok(text) => Ok(text),
            Err(e) => Err(derr("SWARM_CHILD_FAILED", e)),
        }
    }

    fn subagent_read(&self, handle: &str) -> Result<String, PantheonError> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let entry = state.children.get(handle).ok_or_else(|| {
            derr(
                "SWARM_UNKNOWN_HANDLE",
                format!("unknown sub-agent handle {handle:?}"),
            )
        })?;
        let t = entry
            .transcript
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .join("\n");
        Ok(t)
    }

    fn subagent_list(&self) -> Vec<(SubagentHandle, SubagentStatus)> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .order
            .iter()
            .filter_map(|id| state.children.get(id))
            .map(|e| {
                let status = e
                    .state
                    .0
                    .lock()
                    .map(|s| s.0)
                    .unwrap_or(SubagentStatus::Working);
                (e.handle.clone(), status)
            })
            .collect()
    }
}
