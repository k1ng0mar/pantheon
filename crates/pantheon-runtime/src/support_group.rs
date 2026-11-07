//! support_group: agent_runtime, loop_guard, nightly_tools, rpc, skill_exec_tool, tool_config, watchdog in one file.

pub mod agent_runtime {
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
        /// peer could never release the coordinator's slot - the cap check
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
            Self::with_caps(
                supervisor,
                profiles,
                effective,
                data_dir,
                crate::swarm::Caps::default(),
            )
        }

        /// Build a runtime with an explicit swarm cap set (from a `[swarm]`
        /// config section). See [`AgentRuntime::new`] for the rest.
        pub fn with_caps(
            supervisor: Supervisor,
            profiles: ProfileRegistry,
            effective: EffectiveProfile,
            data_dir: impl Into<PathBuf>,
            caps: crate::swarm::Caps,
        ) -> Result<Self, PantheonError> {
            let policy_preset = effective.policy.value.clone();
            let data_dir = data_dir.into();
            let collaboration = CollaborationStore::open(&data_dir.join("collaboration.db"))?;
            Ok(Self {
                inner: Arc::new(AgentRuntimeInner {
                    supervisor,
                    // One swarm per process, shared by every profile in it, so
                    // caps bound the whole run rather than each call.
                    swarm: Arc::new(Mutex::new(crate::swarm::Swarm::new(caps))),
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
        /// into a delegated child's system prompt - the child cannot inherit
        /// the parent's context, so the persona must travel with the spawn.
        pub fn soul_file(&self) -> Option<&str> {
            self.inner.effective.soul_file.value.as_deref()
        }

        /// User-context file path declared by this profile, if any. Read
        /// verbatim into the main session's system prompt alongside the
        /// persona - it is part of the identity, not a layer.
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
        /// created before agent profiles existed - never default it.
        pub fn run_agent(&self, run_id: &str) -> Result<Option<String>, PantheonError> {
            self.inner.supervisor.ledger_run_agent(run_id)
        }

        /// Report this agent's work on a task it was assigned.
        ///
        /// `result` is recorded verbatim as *data*. Only the assignee settles
        /// its own task: a coordinator that completed someone else's work would
        /// erase the audit trail of who actually did it.
        pub fn complete_task(
            &self,
            task_id: &str,
            result: &str,
        ) -> Result<AgentTask, PantheonError> {
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
            correlation_id: Option<&str>,
        ) -> Result<AgentMessage, PantheonError> {
            self.inner.collaboration.record_message(
                message_id,
                None,
                task_id,
                correlation_id,
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
}

pub mod loop_guard {
    //! Loop-hygiene gate: the dsh `repeat-tool-reminder` guard, expressed with
    //! Pantheon's own deny-with-reason semantics (plan doc §10).
    //!
    //! A model that issues the same tool call with the same arguments three
    //! times in a row is stuck: each identical call burns time and tokens and
    //! changes nothing. On the threshold call this gate refuses with a
    //! model-visible reason - an ordinary tool-shaped refusal, exactly like the
    //! extension gate's - so the model can change approach, adjust arguments,
    //! or finish. Nothing is blocked permanently: the counter resets once the
    //! call differs (or the turn ends, since the guard lives one turn).
    //!
    //! Comparison is exact on (tool, args) - no hashing, so a hash collision can
    //! never deny a call that differs. State is per-turn by construction: each
    //! user message is a fresh budget, which keeps a legitimate retry pattern
    //! ("read file, then read it again after editing") from tripping across
    //! turns while still stopping a same-turn spin.

    const DEFAULT_THRESHOLD: u32 = 3;
    /// Args compared in full; this cap only bounds one denial's error text.
    const ARGS_SNIPPET: usize = 160;

    #[derive(Default)]
    struct TurnState {
        last: Option<(String, String)>,
        count: u32,
    }

    pub struct LoopGuard {
        state: std::sync::Mutex<TurnState>,
        threshold: u32,
    }

    impl Default for LoopGuard {
        fn default() -> Self {
            Self::new()
        }
    }

    impl LoopGuard {
        pub fn new() -> Self {
            Self {
                state: std::sync::Mutex::new(TurnState::default()),
                threshold: DEFAULT_THRESHOLD,
            }
        }

        /// `Ok(())` proceeds; `Err(reason)` is the model-visible refusal text.
        pub fn admit(&self, tool: &str, args: &str) -> Result<(), String> {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let repeat = state
                .last
                .as_ref()
                .is_some_and(|(last_tool, last_args)| last_tool == tool && last_args == args);
            if repeat {
                state.count += 1;
            } else {
                state.last = Some((tool.to_string(), args.to_string()));
                state.count = 1;
            }
            if state.count < self.threshold {
                return Ok(());
            }
            let count = state.count;
            // Fresh budget after the refusal: the next differing call starts over.
            state.last = None;
            state.count = 0;
            let snippet: String = args.chars().take(ARGS_SNIPPET).collect();
            Err(format!(
                "loop guard: identical call to `{tool}` repeated {count} times in a row (args: {snippet}). \
                 Change approach: gather different information, adjust the arguments, or finish the task."
            ))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn allows_distinct_calls_and_first_repeats() {
            let guard = LoopGuard::new();
            assert!(guard.admit("read_file", r#"{"path":"a"}"#).is_ok());
            assert!(guard.admit("read_file", r#"{"path":"b"}"#).is_ok());
            assert!(guard.admit("read_file", r#"{"path":"a"}"#).is_ok());
            assert!(guard.admit("read_file", r#"{"path":"a"}"#).is_ok());
            // The third consecutive identical `a` denies (the contract
            // `denies_on_third_consecutive_identical_call` pins); the test
            // formerly asserted a fifth Ok, which that contract forbids.
            let err = guard.admit("read_file", r#"{"path":"a"}"#).unwrap_err();
            assert!(err.contains("3 times"));
        }

        #[test]
        fn denies_on_third_consecutive_identical_call() {
            let guard = LoopGuard::new();
            let args = r#"{"path":"a"}"#;
            assert!(guard.admit("read_file", args).is_ok());
            assert!(guard.admit("read_file", args).is_ok());
            let err = guard.admit("read_file", args).unwrap_err();
            assert!(err.contains("3 times"));
            assert!(err.contains("read_file"));
        }

        #[test]
        fn budget_resets_after_a_denial_and_after_a_change() {
            let guard = LoopGuard::new();
            let args = r#"{"path":"a"}"#;
            for _ in 0..3 {
                let _ = guard.admit("read_file", args);
            }
            // A different call resets the streak.
            assert!(guard.admit("list_dir", "{}").is_ok());
            assert!(guard.admit("read_file", args).is_ok());
            assert!(guard.admit("read_file", args).is_ok());
            assert!(guard.admit("read_file", args).is_err());
        }
    }
}

pub mod nightly_tools {
    //! The nightly repair loop's durable tool-disable list.
    //!
    //! When the nightly pass disables a broken tool it removes it from the
    //! live registry *and* records the name here
    //! (`<data_dir>/nightly/disabled-tools.json`), so the next registry build
    //! skips it. [`crate::session::Session::build_tool_registry`] consults this
    //! list - the registry build is the one constructor, so the skip lives
    //! there, not in every caller.
    //!
    //! The file is a JSON array of `{name, reason, at_ms}` records. Writes are
    //! atomic (temp file + rename), mirroring the other data-dir stores.
    //!
    //! Missing vs corrupt are different states, and the old code conflated
    //! them: a missing file is normal (no tool has ever been disabled) and
    //! reads as empty, but a corrupt file used to read as empty too -
    //! silently re-enabling every tool the nightly pass had contained,
    //! including a dangerous one it disabled for a reason. A corrupt list
    //! now warns loudly at the registry build (log + `/status`
    //! diagnostic) and refuses to be overwritten, so the corruption is
    //! visible instead of paved over. The build still proceeds - it cannot
    //! know which tools the corrupt list named - but it never proceeds
    //! quietly.

    use pantheon_api::logging::now_ms;
    use std::path::{Path, PathBuf};

    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    struct DisabledTool {
        name: String,
        reason: String,
        at_ms: i64,
    }

    /// Path of the disable list: `<data_dir>/nightly/disabled-tools.json`.
    pub fn disabled_tools_path(data_dir: &Path) -> PathBuf {
        data_dir.join("nightly").join("disabled-tools.json")
    }

    /// Names the nightly disabled, in record order. Missing file = empty
    /// (no tool has ever been disabled). A corrupt file also yields empty
    /// here - use [`load_disabled_tools_status`] when the distinction
    /// matters, which is everywhere the list gates a dangerous tool.
    pub fn load_disabled_tools(data_dir: &Path) -> Vec<String> {
        load_disabled_tools_status(data_dir).0
    }

    /// Names the nightly disabled, plus whether the list file exists but
    /// failed to parse. `true` means containment may be lost: tools the
    /// nightly pass disabled could be live again, and callers must warn
    /// loudly rather than proceed as if the list were simply empty.
    pub fn load_disabled_tools_status(data_dir: &Path) -> (Vec<String>, bool) {
        let (records, corrupt) = read_records_status(&disabled_tools_path(data_dir));
        (records.into_iter().map(|e| e.name).collect(), corrupt)
    }

    /// Record one nightly tool disable. Idempotent on the name: re-disabling
    /// updates the reason and timestamp instead of duplicating the row.
    /// Refuses to overwrite a corrupt list: writing a fresh list over it
    /// would silently discard the containment record it once held.
    pub fn record_disabled_tool(data_dir: &Path, name: &str, reason: &str) -> Result<(), String> {
        let path = disabled_tools_path(data_dir);
        let (mut records, corrupt) = read_records_status(&path);
        if corrupt {
            return Err(format!(
                "refusing to overwrite corrupt disabled-tools list {}; repair or remove it by hand first",
                path.display()
            ));
        }
        let at_ms = now_ms();
        match records.iter_mut().find(|e| e.name == name) {
            Some(e) => {
                e.reason = reason.to_string();
                e.at_ms = at_ms;
            }
            None => records.push(DisabledTool {
                name: name.to_string(),
                reason: reason.to_string(),
                at_ms,
            }),
        }
        write_records(&path, &records)
    }

    /// Read the list, distinguishing "no file" (clean empty) from "file
    /// present but unreadable or unparseable" (corrupt).
    fn read_records_status(path: &Path) -> (Vec<DisabledTool>, bool) {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(records) => (records, false),
                Err(_) => (Vec::new(), true),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vec::new(), false),
            Err(_) => (Vec::new(), true),
        }
    }

    fn write_records(path: &Path, records: &[DisabledTool]) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create nightly dir: {e}"))?;
        }
        let text = serde_json::to_string_pretty(records)
            .map_err(|e| format!("encode disable list: {e}"))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).map_err(|e| format!("write disable list: {e}"))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("publish disable list: {e}"))?;
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn dir_with(content: Option<&str>) -> (tempfile::TempDir, PathBuf) {
            let dir = tempfile::tempdir().expect("tempdir");
            let data_dir = dir.path().to_path_buf();
            if let Some(text) = content {
                let path = disabled_tools_path(&data_dir);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, text).unwrap();
            }
            (dir, data_dir)
        }

        #[test]
        fn missing_list_is_clean_empty_not_corrupt() {
            let (_dir, data_dir) = dir_with(None);
            let (names, corrupt) = load_disabled_tools_status(&data_dir);
            assert!(names.is_empty());
            assert!(!corrupt, "a missing list is normal, not corruption");
        }

        #[test]
        fn corrupt_list_is_flagged_and_refuses_overwrite() {
            let (_dir, data_dir) = dir_with(Some("{not json"));
            let (names, corrupt) = load_disabled_tools_status(&data_dir);
            assert!(names.is_empty());
            assert!(corrupt, "an unparseable list must be flagged");
            let err = record_disabled_tool(&data_dir, "tool-x", "broken")
                .expect_err("a corrupt list must not be overwritten");
            assert!(err.contains("corrupt"), "got: {err}");
            assert_eq!(
                std::fs::read_to_string(disabled_tools_path(&data_dir)).unwrap(),
                "{not json",
                "the corrupt file must be untouched by the refused write"
            );
        }

        #[test]
        fn valid_list_round_trips_and_dedupes_on_name() {
            let (_dir, data_dir) = dir_with(None);
            record_disabled_tool(&data_dir, "tool-x", "first").unwrap();
            record_disabled_tool(&data_dir, "tool-x", "second").unwrap();
            let (names, corrupt) = load_disabled_tools_status(&data_dir);
            assert_eq!(names, vec!["tool-x".to_string()]);
            assert!(!corrupt);
        }
    }
}

pub mod rpc {
    //! JSON-RPC 2.0 protocol types and method dispatch (§18).
    //!
    //! The runtime answers commands over a transport; the transport does nothing
    //! but move JSON lines - this module owns the protocol. One request id maps
    //! to exactly one response id, notifications (id `null`) are executed but
    //! never answered, and every failure - bad JSON, unknown method, bad params,
    //! handler error - becomes a structured error response, never a crash.

    use serde::{Deserialize, Serialize};
    use serde_json::{json, Value};
    use std::collections::HashMap;
    use std::sync::{Arc, RwLock};

    /// JSON-RPC 2.0 error codes.
    pub const PARSE_ERROR_CODE: i32 = -32700;
    pub const INVALID_REQUEST_CODE: i32 = -32600;
    pub const METHOD_NOT_FOUND_CODE: i32 = -32601;
    pub const INVALID_PARAMS_CODE: i32 = -32602;
    pub const INTERNAL_ERROR_CODE: i32 = -32603;

    /// Request id. `Null` marks a notification (no response is sent).
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(untagged)]
    pub enum Id {
        Number(i64),
        Str(String),
        Null,
    }

    /// A structured JSON-RPC error.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct RpcError {
        pub code: i32,
        pub message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub data: Option<Value>,
    }

    impl RpcError {
        pub fn new(code: i32, message: impl Into<String>) -> Self {
            Self {
                code,
                message: message.into(),
                data: None,
            }
        }

        pub fn parse_error() -> Self {
            Self::new(
                PARSE_ERROR_CODE,
                "parse error: the message was not valid JSON-RPC 2.0",
            )
        }

        pub fn invalid_request() -> Self {
            Self::new(
                INVALID_REQUEST_CODE,
                "invalid request: expected a JSON-RPC 2.0 request object",
            )
        }

        pub fn method_not_found(method: &str) -> Self {
            Self::new(METHOD_NOT_FOUND_CODE, format!("method not found: {method}"))
        }

        pub fn invalid_params(detail: impl Into<String>) -> Self {
            Self::new(INVALID_PARAMS_CODE, detail.into())
        }

        pub fn internal(detail: impl Into<String>) -> Self {
            Self::new(INTERNAL_ERROR_CODE, detail.into())
        }
    }

    /// One inbound command.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct Request {
        pub jsonrpc: String,
        pub id: Id,
        pub method: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub params: Option<Value>,
    }

    /// One outbound answer: a result xor an error, echoing the request id.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct Response {
        pub jsonrpc: String,
        pub id: Id,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub result: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub error: Option<RpcError>,
    }

    impl Response {
        pub fn ok(id: Id, result: Value) -> Self {
            Self {
                jsonrpc: "2.0".into(),
                id,
                result: Some(result),
                error: None,
            }
        }

        pub fn err(id: Id, error: RpcError) -> Self {
            Self {
                jsonrpc: "2.0".into(),
                id,
                result: None,
                error: Some(error),
            }
        }

        pub fn is_success(&self) -> bool {
            self.error.is_none()
        }
    }

    /// A single command handler. Handlers are pure over their params: all
    /// runtime access goes through the runtime that constructs them.
    pub trait MethodHandler: Send + Sync {
        fn call(&self, params: Option<Value>) -> Result<Value, RpcError>;
    }

    /// Method registry + dispatch. Thread-safe: a server may answer many
    /// connections concurrently. Registering an existing name replaces it.
    pub struct Dispatcher {
        methods: Arc<RwLock<HashMap<String, Arc<dyn MethodHandler>>>>,
    }

    impl Default for Dispatcher {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Dispatcher {
        pub fn new() -> Self {
            Self {
                methods: Arc::new(RwLock::new(HashMap::new())),
            }
        }

        /// A dispatcher with the transport-agnostic built-ins:
        /// `system.ping` and `system.methods`.
        pub fn with_builtins() -> Self {
            let d = Self::new();
            d.register("system.ping", Ping);
            d.register(
                "system.methods",
                MethodList {
                    methods: Arc::clone(&d.methods),
                },
            );
            d
        }

        pub fn register<H: MethodHandler + 'static>(&self, name: impl Into<String>, handler: H) {
            self.methods
                .write()
                .expect("dispatcher lock poisoned")
                .insert(name.into(), Arc::new(handler));
        }

        /// Registered method names, sorted.
        pub fn methods(&self) -> Vec<String> {
            let mut names: Vec<String> = self
                .methods
                .read()
                .expect("dispatcher lock poisoned")
                .keys()
                .cloned()
                .collect();
            names.sort();
            names
        }

        /// Dispatch one request. Notifications (`id: null`) return `None`.
        pub fn dispatch(&self, req: &Request) -> Option<Response> {
            let response = self.invoke(req);
            if req.id == Id::Null {
                None
            } else {
                Some(response)
            }
        }

        fn invoke(&self, req: &Request) -> Response {
            let Some(handler) = self
                .methods
                .read()
                .expect("dispatcher lock poisoned")
                .get(&req.method)
                .cloned()
            else {
                return Response::err(req.id.clone(), RpcError::method_not_found(&req.method));
            };
            match handler.call(req.params.clone()) {
                Ok(value) => Response::ok(req.id.clone(), value),
                Err(e) => Response::err(req.id.clone(), e),
            }
        }

        /// Handle one newline-delimited JSON line and produce its responses.
        ///
        /// Covers the protocol-level failures the spec requires to be answers,
        /// not crashes: a line that is not JSON gets a parse-error response with
        /// id `null`; a non-request or wrong-version object gets an
        /// invalid-request response.
        ///
        /// Batch arrays follow JSON-RPC 2.0 (§6): each element is dispatched in
        /// order and its responses concatenated. An empty array is an invalid
        /// request; a batch in which every element is a notification produces no
        /// response at all, so the caller must not assume one reply per line.
        pub fn handle_line(&self, line: &str) -> Vec<Response> {
            let value: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => return vec![Response::err(Id::Null, RpcError::parse_error())],
            };
            let Some(items) = value.as_array() else {
                return self.handle_value(value);
            };
            // §6: an empty batch is an invalid request, not a no-op.
            if items.is_empty() {
                return vec![Response::err(Id::Null, RpcError::invalid_request())];
            }
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.extend(self.handle_value(item.clone()));
            }
            out
        }

        /// Dispatch one already-parsed JSON value into zero or one response.
        fn handle_value(&self, value: Value) -> Vec<Response> {
            let req: Request = match serde_json::from_value(value) {
                Ok(r) => r,
                Err(_) => return vec![Response::err(Id::Null, RpcError::invalid_request())],
            };
            if req.jsonrpc != "2.0" {
                return vec![Response::err(Id::Null, RpcError::invalid_request())];
            }
            match self.dispatch(&req) {
                Some(resp) => vec![resp],
                None => vec![],
            }
        }
    }

    /// `system.ping` - liveness probe.
    struct Ping;
    impl MethodHandler for Ping {
        fn call(&self, _params: Option<Value>) -> Result<Value, RpcError> {
            Ok(json!({ "pong": true }))
        }
    }

    /// `system.methods` - the currently registered command names.
    struct MethodList {
        methods: Arc<RwLock<HashMap<String, Arc<dyn MethodHandler>>>>,
    }
    impl MethodHandler for MethodList {
        fn call(&self, _params: Option<Value>) -> Result<Value, RpcError> {
            let mut names: Vec<String> = self
                .methods
                .read()
                .expect("dispatcher lock poisoned")
                .keys()
                .cloned()
                .collect();
            names.sort();
            Ok(Value::Array(names.into_iter().map(Value::String).collect()))
        }
    }

    #[cfg(test)]
    mod batch_tests {
        use super::*;

        fn d() -> Dispatcher {
            Dispatcher::with_builtins()
        }

        /// Batch arrays used to be rejected wholesale. They are part of JSON-RPC
        /// 2.0 (§6) and clients send them, so they must dispatch element-wise.
        #[test]
        fn batch_dispatches_every_element_in_order() {
            let line = r#"[
                {"jsonrpc":"2.0","id":1,"method":"system.ping"},
                {"jsonrpc":"2.0","id":2,"method":"system.ping"}
            ]"#;
            let out = d().handle_line(line);
            assert_eq!(out.len(), 2, "one response per request");
            assert_eq!(out[0].id, Id::Number(1));
            assert_eq!(out[1].id, Id::Number(2));
            assert!(out.iter().all(|r| r.is_success()));
        }

        /// §6: an empty array is an invalid request, not a silent success.
        #[test]
        fn empty_batch_is_invalid_request() {
            let out = d().handle_line("[]");
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].error.as_ref().unwrap().code, INVALID_REQUEST_CODE);
        }

        /// A batch of notifications must produce no responses at all. A server
        /// that wrote a `null` reply here would corrupt the client's stream.
        #[test]
        fn notification_only_batch_answers_nothing() {
            let line = r#"[
                {"jsonrpc":"2.0","id":null,"method":"system.ping"},
                {"jsonrpc":"2.0","id":null,"method":"system.ping"}
            ]"#;
            assert!(d().handle_line(line).is_empty());
        }

        /// Mixed batches answer only the calls that carry an id.
        #[test]
        fn mixed_batch_answers_only_requests() {
            let line = r#"[
                {"jsonrpc":"2.0","id":null,"method":"system.ping"},
                {"jsonrpc":"2.0","id":7,"method":"system.ping"}
            ]"#;
            let out = d().handle_line(line);
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].id, Id::Number(7));
        }

        /// One bad element must not poison its siblings: §6 has servers answer
        /// per-element, and an unknown method is a normal error response.
        #[test]
        fn unknown_method_in_batch_fails_only_its_element() {
            let line = r#"[
                {"jsonrpc":"2.0","id":1,"method":"system.ping"},
                {"jsonrpc":"2.0","id":2,"method":"no.such.method"}
            ]"#;
            let out = d().handle_line(line);
            assert_eq!(out.len(), 2);
            assert!(out[0].is_success());
            assert_eq!(
                out[1].error.as_ref().unwrap().code,
                METHOD_NOT_FOUND_CODE,
                "a bad element must answer method-not-found, not fail the batch"
            );
        }

        /// A single (non-array) request keeps behaving exactly as before.
        #[test]
        fn single_object_still_answers_one_response() {
            let out = d().handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"system.ping"}"#);
            assert_eq!(out.len(), 1);
            assert!(out[0].is_success());
        }

        /// Malformed JSON is still a parse error with a null id.
        #[test]
        fn malformed_json_is_still_a_parse_error() {
            let out = d().handle_line("{not json");
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].error.as_ref().unwrap().code, PARSE_ERROR_CODE);
            assert_eq!(out[0].id, Id::Null);
        }
    }
}

pub mod skill_exec_tool {
    //! The `skill_exec` agent tool: run a skill's declared executables.
    //!
    //! Registration lives here (pantheon-runtime) because the tool registry
    //! type belongs to pantheon-tools, which pantheon-exec cannot depend on
    //! (Tools → Exec is the layering; Exec knows nothing of the registry).
    //! All execution mechanics - parsing, confinement, sandboxing - stay in
    //! pantheon-exec (`skills`, `skill_exec`).
    //!
    //! Two worlds, one tool list:
    //! - Declared executables (`exec:` frontmatter) run via `skill_exec`:
    //!   validated, sandboxed, attributed on the timeline.
    //! - Third-party skills that invoke scripts through prose
    //!   (`"${CLAUDE_SKILL_DIR}/scripts/..."`) are run by the agent via the
    //!   regular `shell` tool. The `skill_exec` description therefore also
    //!   lists every skill's absolute directory, so the agent can expand the
    //!   placeholder itself.

    use pantheon_api::capability::Capability;
    use pantheon_api::error::PantheonError;
    use pantheon_api::message::ToolSchema;
    use pantheon_exec::skills::{
        find_skill_exec, parse_skill_exec_args, skill_dir, Skill,
        SKILL_EXEC_TOOL_NAME,
    };
    use pantheon_tools::tools::ToolRegistry;
    use std::sync::Arc;

    /// Register `skill_exec` on a registry.
    ///
    /// Registered whenever at least one skill is installed - even when none
    /// declares an executable - because the description carries the
    /// skill-directory mapping third-party prose-invoked scripts need. An
    /// empty skill list registers nothing.
    pub fn register_skill_exec_tool(reg: &mut ToolRegistry, skills: Vec<Skill>) {
        if skills.is_empty() {
            return;
        }
        let skills = Arc::new(skills);
        let for_caps = Arc::clone(&skills);
        let description = describe_skill_exec(&skills);
        reg.register_with(
            ToolSchema {
                name: SKILL_EXEC_TOOL_NAME.into(),
                description,
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "skill": {
                            "type": "string",
                            "description": "Skill name, as listed by skills_list"
                        },
                        "name": {
                            "type": "string",
                            "description": "Executable name as declared in the skill's exec: frontmatter"
                        },
                        "args": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Arguments, each passed as a separate argv element (never shell-interpolated). ${CLAUDE_SKILL_DIR} / $SKILL_DIR expand to the skill's directory."
                        }
                    },
                    "required": ["skill", "name"]
                }),
            },
            Capability::FilesystemRead,
            move |args| run_skill_exec_call(&skills, args),
            Some(Box::new(move |args: &str| {
                extra_capabilities(&for_caps, args)
            })),
        );
    }

    /// Capability gating per call, on top of the static FilesystemRead:
    /// EVERY `skill_exec` run needs ShellExecute, whatever its declared
    /// side effects say. The declaration is author-controlled, and a
    /// read-declared executable is still arbitrary skill code running
    /// under the HIGH sandbox (network on, no seccomp/landlock, whole
    /// host tree readable) - strictly more than FilesystemRead, which
    /// confines reads to the workspace. Trusting the declaration let a
    /// Reader session run untrusted code and exfiltrate files the file
    /// tools would confine-deny (e.g. ~/.ssh). With ShellExecute
    /// required, a Reader policy denies the call, Plan mode refuses it
    /// (see `skill_exec_call_is_mutating`), and under Coder it runs with
    /// exactly the privilege the shell tool already grants - no new
    /// privilege, no silent widening. Discovery-time scanning is not a
    /// substitute: project-root skills (`.pantheon/skills`,
    /// `.agents/skills`, `.claude/skills`, `PANTHEON_SKILLS_DIR`) are
    /// collected without a SkillSpector gate, so the gate must live here,
    /// at execution, not at import.
    ///
    /// A call that cannot be resolved is treated as an execution attempt,
    /// not as a read. The executor fails it closed, so nothing runs, but
    /// the gate should not be the thing that decided it was harmless:
    /// reporting `FilesystemRead` for a `skill_exec` invocation means a
    /// caller that somehow got a resolvable exec past this check would
    /// only need the read tier. Fail toward the capability that matches
    /// the tool's nature.
    fn extra_capabilities(skills: &[Skill], args: &str) -> Vec<Capability> {
        let Ok((skill, name, _)) = parse_skill_exec_args(args) else {
            // Not even parseable: still a `skill_exec` call.
            return vec![Capability::ShellExecute];
        };
        match find_skill_exec(skills, &skill, &name) {
            Ok(_) => vec![Capability::ShellExecute],
            // The model named a skill or an executable that does not exist.
            // Could be a stale prompt, could be probing for a gate that says
            // no. Either way this is an execution request.
            Err(_) => vec![Capability::ShellExecute],
        }
    }

    fn run_skill_exec_call(skills: &[Skill], args: &str) -> Result<String, PantheonError> {
        let (skill, name, exec_args) = parse_skill_exec_args(args)?;
        let (skill, exec) = find_skill_exec(skills, &skill, &name)?;
        pantheon_exec::skill_exec::run_skill_exec(skill, exec, &exec_args)
    }

    /// Tool description: declared executables (the `skill_exec` path) plus
    /// the skill-directory mapping (the third-party prose path).
    fn describe_skill_exec(skills: &[Skill]) -> String {
        let mut d = String::from(
            "Run one of a skill's declared executables (exec: frontmatter). \
             Each argument is passed as a separate argv element, never shell-interpolated; \
             ${CLAUDE_SKILL_DIR} / $SKILL_DIR in args expand to the skill's directory. \
             Every executable run needs ShellExecute, whatever its declared \\
             side effects say: a read-declared executable is still arbitrary \\
             skill code, not a read-only tool. All runs are blocked in Plan mode.",
        );
        let mut any = false;
        for s in skills {
            for e in s.meta.executables() {
                if !any {
                    d.push_str("\n\nDeclared executables:");
                    any = true;
                }
                d.push_str(&format!(
                    "\n- skill {:?}, executable {:?}: {} [{}]",
                    s.meta.name,
                    e.name,
                    if e.args.is_empty() {
                        "(no args)".to_string()
                    } else {
                        e.args.clone()
                    },
                    e.side_effects.as_str(),
                ));
                if !e.description.is_empty() {
                    d.push_str(&format!(" - {}", e.description));
                }
            }
        }
        if !any {
            d.push_str("\n\nNo skill currently declares an executable.");
        }
        d.push_str(
            "\n\nSkill directories (for third-party skills that invoke scripts through prose, \
             e.g. \"${CLAUDE_SKILL_DIR}/scripts/...\": expand the placeholder to the directory \
             below and run via the shell tool; skill_exec only runs exec:-declared entries):",
        );
        for s in skills {
            d.push_str(&format!(
                "\n- {:?}: {}",
                s.meta.name,
                skill_dir(s).display()
            ));
        }
        d
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::path::Path;

        fn read_declared_skill() -> Skill {
            let raw = "---\nname: probe\ndescription: test skill\nexec:\n  - name: probe-it\n    command: scripts/probe.sh\n    side_effects: read\n---\nBody.\n";
            pantheon_exec::skills::parse_skill(raw, Path::new("/tmp/probe/SKILL.md"))
                .expect("parse test skill")
        }

        #[test]
        fn read_declared_executable_still_needs_shell_execute() {
            // A `read` declaration is author-controlled: it must not drop
            // the call to the read tier, or a Reader session could run
            // arbitrary skill code under FilesystemRead alone.
            let skill = read_declared_skill();
            assert_eq!(
                skill.meta.exec[0].side_effects,
                SkillExecSideEffects::Read,
                "test fixture must declare read"
            );
            let caps = extra_capabilities(&[skill], r#"{"skill": "probe", "name": "probe-it"}"#);
            assert!(
                caps.contains(&Capability::ShellExecute),
                "read-declared skill_exec must require ShellExecute: {caps:?}"
            );
        }

        #[test]
        fn unresolvable_skill_exec_needs_shell_execute() {
            let skill = read_declared_skill();
            let caps = extra_capabilities(&[skill], r#"{"skill": "probe", "name": "nope"}"#);
            assert!(caps.contains(&Capability::ShellExecute));
        }
    }
}

pub mod tool_config {
    //! Runtime-side tool configuration for browser automation and web search.
    //!
    //! These are the resolved, concrete configs the agent loop reads at tool
    //! registration time. The TUI's `[browser]` / `[websearch]` config
    //! sections resolve into these (see `pantheon-tui`'s `BrowserSection` /
    //! `WebsearchSection`), with environment variables winning over the file.

    use pantheon_web::browser::registry::BackendKind;
    use std::path::PathBuf;

    /// Runtime config for the `browser_*` tools (gsd-browser subprocess).
    #[derive(Debug, Clone, PartialEq)]
    pub struct BrowserToolConfig {
        /// Master switch. When false no `browser_*` tools are registered.
        pub enabled: bool,
        /// Which browser backend drives the tools. Default
        /// [`BackendKind::Gsd`].
        pub backend: BackendKind,
        /// Path to the gsd-browser binary. `None` = resolved from PATH.
        pub binary: Option<PathBuf>,
        /// `browser_act` clicks the top semantic-intent candidate with no
        /// minimum score upstream. True = the call carries the `browser.act`
        /// capability and the run parks for human approval.
        pub act_require_approval: bool,
        /// Idle seconds before a run's browser daemon is stopped.
        pub idle_timeout_secs: u64,
        /// Secret name holding the gsd-browser auth-vault key, resolved via
        /// the secrets broker and injected as `GSD_BROWSER_VAULT_KEY`.
        /// `None` = no vault.
        pub vault_key_secret: Option<String>,
        /// Secret name holding the Steel API key, resolved via the secrets
        /// broker. Default "STEEL_API_KEY". `None` = Steel backend unusable.
        pub steel_api_key_secret: Option<String>,
        /// Steel REST base URL override (self-hosted Steel).
        /// `None` = the Steel cloud.
        pub steel_base_url: Option<String>,
        /// Secret name holding the Browserbase API key, resolved via the
        /// secrets broker. Default "BROWSERBASE_API_KEY".
        /// `None` = Browserbase backend unusable.
        pub browserbase_api_key_secret: Option<String>,
        /// Browserbase project id (required by `POST /v1/sessions`).
        pub browserbase_project_id: Option<String>,
        /// Path to the `playwright-cli` binary. `None` = resolved from PATH.
        pub playwright_binary: Option<PathBuf>,
        /// Path to the Chrome/Chromium binary for the `chromiumoxide`
        /// backend. `None` = chromiumoxide's auto-detect.
        pub chrome_binary: Option<PathBuf>,
        /// Headless mode for the `chromiumoxide` backend.
        pub headless: bool,
        /// Explicit `--no-sandbox` opt-in for the `chromiumoxide` backend.
        /// Default false: never default to unsandboxed.
        pub no_sandbox: bool,
        /// Per-command timeout in seconds for browser backends.
        pub timeout_secs: u64,
        /// Camoufox anti-detect settings (`[browser.camofox]`), resolved
        /// with defaults applied.
        pub camofox: CamofoxToolConfig,
    }

    impl Default for BrowserToolConfig {
        fn default() -> Self {
            Self {
                enabled: true,
                backend: BackendKind::Gsd,
                binary: None,
                act_require_approval: true,
                idle_timeout_secs: 900,
                vault_key_secret: Some("GSD_BROWSER_VAULT_KEY".to_string()),
                steel_api_key_secret: Some("STEEL_API_KEY".to_string()),
                steel_base_url: None,
                browserbase_api_key_secret: Some("BROWSERBASE_API_KEY".to_string()),
                browserbase_project_id: None,
                playwright_binary: None,
                chrome_binary: None,
                headless: true,
                no_sandbox: false,
                timeout_secs: 120,
                camofox: CamofoxToolConfig::default(),
            }
        }
    }

    /// Runtime config for the Camoufox anti-detect browser backend. These
    /// are the resolved `[browser.camofox]` values (defaults applied); the
    /// proxy password itself resolves at registration via
    /// `proxy_password_secret` and is never stored here.
    #[derive(Debug, Clone, PartialEq)]
    pub struct CamofoxToolConfig {
        /// Python interpreter for the shim. `None` = `python3` on PATH.
        pub python: Option<PathBuf>,
        /// Launch headless.
        pub headless: bool,
        /// Xvfb "virtual" headless mode instead of true headless (needs
        /// `xvfb`).
        pub headless_virtual: bool,
        /// Fingerprint OS: `windows` | `macos` | `linux`.
        /// `None` = the launcher's BrowserForge default.
        pub os: Option<String>,
        /// Human-like cursor movement, in seconds. `None` = off.
        pub humanize_secs: Option<f64>,
        /// Derive timezone/locale/geolocation from the proxy IP (needs the
        /// `[geoip]` install extra).
        pub geoip: bool,
        /// Explicit locale override, e.g. `"en-US"`.
        pub locale: Option<String>,
        /// Explicit timezone override, e.g. `"America/New_York"`.
        pub timezone: Option<String>,
        /// Proxy server URL, e.g. `"http://proxy:8080"`.
        pub proxy_server: Option<String>,
        /// Proxy username (optional).
        pub proxy_username: Option<String>,
        /// Secret name holding the proxy password, resolved via the secrets
        /// broker at registration. Never a raw value.
        pub proxy_password_secret: Option<String>,
        /// Block image loads (perf).
        pub block_images: bool,
        /// Block WebRTC (IP-leak prevention).
        pub block_webrtc: bool,
        /// Use BrowserForge-backed real fingerprint presets.
        pub fingerprint_preset: bool,
        /// Per-command timeout in seconds.
        pub timeout_secs: u64,
        /// Seconds to wait for the browser to launch on session start.
        /// First launch after a fetch can take minutes.
        pub startup_timeout_secs: u64,
    }

    impl Default for CamofoxToolConfig {
        /// Matches [`pantheon_web::browser::CamofoxConfig`]'s defaults.
        fn default() -> Self {
            Self {
                python: None,
                headless: true,
                headless_virtual: false,
                os: None,
                humanize_secs: None,
                geoip: false,
                locale: None,
                timezone: None,
                proxy_server: None,
                proxy_username: None,
                proxy_password_secret: None,
                block_images: false,
                block_webrtc: false,
                fingerprint_preset: true,
                timeout_secs: 120,
                startup_timeout_secs: 300,
            }
        }
    }

    /// Runtime config for the `web_search` tool.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct WebsearchToolConfig {
        /// Master switch. Keyed providers register only when a key also
        /// resolves; keyless providers (Marginalia, the `*_free` fallbacks,
        /// `ddgs`, self-hosted SearXNG) register without one - the old "a
        /// keyless `web_search` can never work" claim predates them.
        pub enabled: bool,
        /// Provider name: a `pantheon-web` registry id (`tavily`, `exa`,
        /// `marginalia`, ...). An unknown id leaves `web_search` unregistered
        /// with a diagnostic recorded on the session (`/status` reads it).
        pub provider: String,
        /// Secret name holding the provider API key, resolved via the
        /// secrets broker. Never hardcoded. `None` = the provider's own
        /// default (via `pantheon_web::websearch::default_key_env`), so a
        /// provider switch without an explicit secret name still resolves
        /// the right variable instead of a stale Tavily one.
        pub api_key_secret: Option<String>,
        /// Endpoint override for self-hosted providers. `None` = the
        /// provider's default endpoint.
        ///
        /// DEPRECATED legacy global: retained only so the session can warn
        /// the operator to move it to `base_urls`. It is NEVER passed to
        /// `build_provider` (a global URL used to follow a provider switch
        /// and send another provider's calls, and key, to the wrong host);
        /// the use site resolves `base_url_for(provider)` instead.
        pub base_url: Option<String>,
        /// Per-provider endpoint overrides, keyed by registry provider id
        /// (`[websearch.base_urls.<provider>]`). Only the entry for the
        /// provider actually being built is ever used.
        pub base_urls: std::collections::HashMap<String, String>,
        /// Default result count per query.
        pub max_results: u8,
    }

    impl WebsearchToolConfig {
        /// Endpoint override for exactly one provider id, or `None`.
        /// Mirrors `WebSearchSection::base_url_for`: case-insensitive on
        /// the provider id, blank values ignored, and the legacy global
        /// `base_url` deliberately NOT consulted, so one provider's URL
        /// can never follow a provider switch into another's requests.
        pub fn base_url_for(&self, provider: &str) -> Option<&str> {
            let want = provider.trim().to_ascii_lowercase();
            self.base_urls.iter().find_map(|(k, v)| {
                if k.trim().to_ascii_lowercase() == want {
                    let v = v.trim();
                    if v.is_empty() {
                        None
                    } else {
                        Some(v)
                    }
                } else {
                    None
                }
            })
        }
    }

    /// Runtime config for desktop computer use (the CUA driver).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ComputerToolConfig {
        /// Master switch, joined with the ComputerUse tool-group toggle.
        /// When false no driver tools are registered.
        pub enabled: bool,
        /// Driver id. Today only `cua-driver`.
        pub driver: String,
        /// Path to the driver binary. `None` = resolved from PATH.
        pub binary: Option<PathBuf>,
    }

    impl Default for ComputerToolConfig {
        fn default() -> Self {
            Self {
                enabled: true,
                driver: crate::computer::CUA_DRIVER_ID.to_string(),
                binary: None,
            }
        }
    }

    impl Default for WebsearchToolConfig {
        fn default() -> Self {
            Self {
                enabled: true,
                provider: "tinyfish".to_string(),
                api_key_secret: None,
                base_url: None,
                base_urls: std::collections::HashMap::new(),
                max_results: 8,
            }
        }
    }

    /// Runtime config for MCP servers: `[mcp]` + migration declarations,
    /// resolved into server specs by the TUI at session startup.
    #[derive(Debug, Clone, PartialEq, Eq, Default)]
    pub struct McpToolConfig {
        /// Master switch. When false the manager holds no specs and no
        /// server is launched, connected, or registered.
        pub enabled: bool,
        /// Resolved server specs (config section first, declarations fill gaps).
        pub servers: Vec<pantheon_mcp::manager::McpServerSpec>,
    }

    /// Tool-group enablement resolved from `[tools]` in config.toml: which
    /// tool groups the runtime registers. A disabled group never appears in
    /// the model's tool list - the registration call sites consult this,
    /// never the config section directly.
    ///
    /// Session search is deliberately absent: it is default-on and always
    /// registered, so there is nothing to resolve for it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ToolEnablement {
        pub web_search: bool,
        pub browser: bool,
        pub terminal: bool,
        pub files: bool,
        pub memory: bool,
        pub skills: bool,
        pub tasks: bool,
        pub delegation: bool,
        pub ask_user: bool,
        pub vault: bool,
        pub voice: bool,
        pub vision: bool,
        pub video_analysis: bool,
        pub computer_use: bool,
        pub plugins: bool,
        pub code_intel: bool,
    }

    impl Default for ToolEnablement {
        /// Absent `[tools]` section = every group on, matching the
        /// pre-section behavior exactly.
        fn default() -> Self {
            Self {
                web_search: true,
                browser: true,
                terminal: true,
                files: true,
                memory: true,
                skills: true,
                tasks: true,
                delegation: true,
                ask_user: true,
                vault: true,
                voice: true,
                vision: true,
                video_analysis: true,
                computer_use: true,
                plugins: true,
                code_intel: true,
            }
        }
    }

    impl ToolEnablement {
        /// Every group off. For tests: a disabled group must not appear in
        /// the registry.
        pub fn none() -> Self {
            Self {
                web_search: false,
                browser: false,
                terminal: false,
                files: false,
                memory: false,
                skills: false,
                tasks: false,
                delegation: false,
                ask_user: false,
                vault: false,
                voice: false,
                vision: false,
                video_analysis: false,
                computer_use: false,
                plugins: false,
                code_intel: false,
            }
        }

        /// Resolve from the `[tools]` config section. Absent flags stay on.
        pub fn from_section(s: &pantheon_api::config::ToolsSection) -> Self {
            let mut e = Self::default();
            let get = |v: Option<bool>| v.unwrap_or(true);
            e.web_search = get(s.web_search);
            e.browser = get(s.browser);
            e.terminal = get(s.terminal);
            e.files = get(s.files);
            e.memory = get(s.memory);
            e.skills = get(s.skills);
            e.tasks = get(s.tasks);
            e.delegation = get(s.delegation);
            e.ask_user = get(s.ask_user);
            e.vault = get(s.vault);
            e.voice = get(s.voice);
            e.vision = get(s.vision);
            e.video_analysis = get(s.video_analysis);
            e.computer_use = get(s.computer_use);
            e.plugins = get(s.plugins);
            e.code_intel = get(s.code_intel);
            e
        }

        /// Whether the group registers its tools.
        pub fn is_enabled(&self, group: pantheon_api::config::ToolGroup) -> bool {
            use pantheon_api::config::ToolGroup as G;
            match group {
                G::WebSearch => self.web_search,
                G::Browser => self.browser,
                G::Terminal => self.terminal,
                G::Files => self.files,
                G::Memory => self.memory,
                G::Skills => self.skills,
                G::Tasks => self.tasks,
                G::Delegation => self.delegation,
                G::AskUser => self.ask_user,
                G::Vault => self.vault,
                G::Voice => self.voice,
                G::Vision => self.vision,
                G::VideoAnalysis => self.video_analysis,
                G::ComputerUse => self.computer_use,
                G::Plugins => self.plugins,
                G::CodeIntel => self.code_intel,
            }
        }
    }

    fn on(value: Option<bool>, default: bool) -> bool {
        value.unwrap_or(default)
    }

    /// Resolve `[browser]` to the runtime [`BrowserToolConfig`]. Env vars
    /// (`PANTHEON_BROWSER_ENABLED`, `PANTHEON_BROWSER_BINARY`,
    /// `PANTHEON_BROWSER_BACKEND`) win over the file, matching the runtime
    /// defaults.
    ///
    /// Lives here (not in the TUI) so every client of the runtime - the TUI
    /// agent loop and the dashboard's browser stream endpoints - resolves the
    /// section identically.
    /// Parse an env-var boolean the way users write it. The old `v != "0"`
    /// reading meant `PANTHEON_BROWSER_ENABLED=false` *enabled* the browser
    /// (only the literal "0" disabled it). Unknown values warn and fall back
    /// to the caller's default instead of silently enabling.
    fn env_bool(var: &str) -> Option<bool> {
        let v = std::env::var(var).ok()?;
        match v.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            other => {
                log_warn!("[pantheon] unknown {var} value '{other}'; ignoring it");
                None
            }
        }
    }

    pub fn resolve_browser_section(s: &pantheon_api::config::BrowserSection) -> BrowserToolConfig {
        let mut cfg = BrowserToolConfig::default();
        cfg.enabled =
            env_bool("PANTHEON_BROWSER_ENABLED").unwrap_or_else(|| on(s.enabled, cfg.enabled));
        // Backend selection: env, then file, then the default. Unknown ids
        // warn and fall back to GSD (never fail the session over a typo).
        let backend_id = std::env::var("PANTHEON_BROWSER_BACKEND")
            .ok()
            .or_else(|| s.backend.clone());
        if let Some(id) = backend_id {
            match BackendKind::parse(&id) {
                Some(kind) => cfg.backend = kind,
                None => {
                    log_warn!("[pantheon] unknown [browser] backend '{id}'; falling back to 'gsd'")
                }
            }
        }
        // The docstring promises PANTHEON_BROWSER_BINARY wins over the file,
        // but the code never read it: the binary came only from [browser].
        // Env wins, then file.
        if cfg.binary.is_none() {
            cfg.binary = std::env::var("PANTHEON_BROWSER_BINARY")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .map(std::path::PathBuf::from)
                .or_else(|| s.binary.clone().map(std::path::PathBuf::from));
        }
        cfg.act_require_approval = on(s.act_require_approval, cfg.act_require_approval);
        if let Some(secs) = s.idle_timeout_secs.filter(|&s| s > 0) {
            cfg.idle_timeout_secs = secs;
        }
        if let Some(name) = s.vault_key_secret.clone() {
            if !name.trim().is_empty() {
                cfg.vault_key_secret = Some(name);
            }
        }
        if let Some(name) = s.steel_api_key_secret.clone() {
            if !name.trim().is_empty() {
                cfg.steel_api_key_secret = Some(name);
            }
        }
        if let Some(url) = s.steel_base_url.clone() {
            if !url.trim().is_empty() {
                cfg.steel_base_url = Some(url);
            }
        }
        if let Some(name) = s.browserbase_api_key_secret.clone() {
            if !name.trim().is_empty() {
                cfg.browserbase_api_key_secret = Some(name);
            }
        }
        if let Some(id) = s.browserbase_project_id.clone() {
            if !id.trim().is_empty() {
                cfg.browserbase_project_id = Some(id);
            }
        }
        if let Some(bin) = s.playwright_binary.clone() {
            if !bin.trim().is_empty() {
                cfg.playwright_binary = Some(std::path::PathBuf::from(bin));
            }
        }
        if let Some(bin) = s.chrome_binary.clone() {
            if !bin.trim().is_empty() {
                cfg.chrome_binary = Some(std::path::PathBuf::from(bin));
            }
        }
        if let Some(headless) = s.headless {
            cfg.headless = headless;
        }
        if let Some(no_sandbox) = s.no_sandbox {
            cfg.no_sandbox = no_sandbox;
        }
        if let Some(secs) = s.timeout_secs.filter(|&s| s > 0) {
            cfg.timeout_secs = secs;
        }
        // `[browser.camofox]`: the Camoufox backend's launcher and
        // fingerprint options. Absent = the backend defaults; a
        // camofox-specific timeout wins over the global `[browser]` one.
        if let Some(cs) = &s.camofox {
            let c = &mut cfg.camofox;
            if let Some(p) = cs.python.clone() {
                if !p.trim().is_empty() {
                    c.python = Some(std::path::PathBuf::from(p));
                }
            }
            if let Some(headless) = cs.headless {
                c.headless = headless;
            }
            if let Some(hv) = cs.headless_virtual {
                c.headless_virtual = hv;
            }
            if let Some(os) = cs.os.clone() {
                if !os.trim().is_empty() {
                    c.os = Some(os);
                }
            }
            if let Some(hs) = cs.humanize_secs {
                c.humanize_secs = Some(hs);
            }
            if let Some(geoip) = cs.geoip {
                c.geoip = geoip;
            }
            if let Some(locale) = cs.locale.clone() {
                if !locale.trim().is_empty() {
                    c.locale = Some(locale);
                }
            }
            if let Some(tz) = cs.timezone.clone() {
                if !tz.trim().is_empty() {
                    c.timezone = Some(tz);
                }
            }
            if let Some(server) = cs.proxy_server.clone() {
                if !server.trim().is_empty() {
                    c.proxy_server = Some(server);
                }
            }
            if let Some(user) = cs.proxy_username.clone() {
                if !user.trim().is_empty() {
                    c.proxy_username = Some(user);
                }
            }
            if let Some(name) = cs.proxy_password_secret.clone() {
                if !name.trim().is_empty() {
                    c.proxy_password_secret = Some(name);
                }
            }
            if let Some(block) = cs.block_images {
                c.block_images = block;
            }
            if let Some(block) = cs.block_webrtc {
                c.block_webrtc = block;
            }
            if let Some(fp) = cs.fingerprint_preset {
                c.fingerprint_preset = fp;
            }
            if let Some(secs) = cs.timeout_secs.filter(|&s| s > 0) {
                c.timeout_secs = secs;
            } else {
                c.timeout_secs = cfg.timeout_secs;
            }
            if let Some(secs) = cs.startup_timeout_secs.filter(|&s| s > 0) {
                c.startup_timeout_secs = secs;
            }
        } else {
            // No `[browser.camofox]` section: keep the backend's timeout in
            // line with the global `[browser] timeout_secs`.
            cfg.camofox.timeout_secs = cfg.timeout_secs;
        }
        cfg
    }

    /// Build the [`BackendConfig`] for a resolved [`BrowserToolConfig`],
    /// resolving secret names through `secrets`.
    ///
    /// Pure mapping (no I/O, no browser launch): the agent loop uses this at
    /// tool-registration time, and the dashboard's browser stream/input
    /// endpoints use it to drive the identical backend.
    pub fn browser_backend_config(
        cfg: &BrowserToolConfig,
        secrets: &pantheon_secrets::SecretsBroker,
    ) -> pantheon_web::browser::registry::BackendConfig {
        use pantheon_web::browser as b;
        let resolve_secret = |name: Option<&str>| {
            name.and_then(|n| secrets.resolve(n).ok().flatten())
                .map(|v| v.expose().to_owned())
        };
        let timeout = cfg.timeout_secs;
        b::registry::BackendConfig {
            gsd_binary: cfg.binary.clone(),
            gsd_vault_key: resolve_secret(cfg.vault_key_secret.as_deref()),
            native: b::NativeConfig {
                chrome_binary: cfg.chrome_binary.clone(),
                headless: Some(cfg.headless),
                no_sandbox: cfg.no_sandbox,
                timeout_secs: timeout,
            },
            steel: b::SteelConfig {
                api_key: resolve_secret(cfg.steel_api_key_secret.as_deref()),
                base_url: cfg.steel_base_url.clone(),
                timeout_secs: timeout,
            },
            browserbase: b::BrowserbaseConfig {
                api_key: resolve_secret(cfg.browserbase_api_key_secret.as_deref()),
                project_id: cfg.browserbase_project_id.clone(),
                base_url: None,
                timeout_secs: timeout,
            },
            playwright: b::PlaywrightConfig {
                binary: cfg.playwright_binary.clone(),
                timeout_secs: timeout,
            },
            // `[browser.camofox]` arrives resolved (defaults applied). The
            // proxy password resolves here, at build time, like the other
            // secret values - never stored, never logged.
            camofox: b::CamofoxConfig {
                python: cfg.camofox.python.clone(),
                headless: cfg.camofox.headless,
                headless_virtual: cfg.camofox.headless_virtual,
                os: cfg.camofox.os.clone(),
                humanize_secs: cfg.camofox.humanize_secs,
                geoip: cfg.camofox.geoip,
                locale: cfg.camofox.locale.clone(),
                timezone: cfg.camofox.timezone.clone(),
                proxy_server: cfg.camofox.proxy_server.clone(),
                proxy_username: cfg.camofox.proxy_username.clone(),
                proxy_password: resolve_secret(cfg.camofox.proxy_password_secret.as_deref()),
                block_images: cfg.camofox.block_images,
                block_webrtc: cfg.camofox.block_webrtc,
                fingerprint_preset: cfg.camofox.fingerprint_preset,
                timeout_secs: cfg.camofox.timeout_secs,
                startup_timeout_secs: cfg.camofox.startup_timeout_secs,
                ..Default::default()
            },
            timeout_secs: timeout,
        }
    }

    /// Build the configured browser backend from a resolved
    /// [`BrowserToolConfig`], resolving secret names through `secrets`.
    ///
    /// Returns `None` when the browser tool is disabled - callers treat that
    /// as "browser not available", not an error. Secrets resolve here (build
    /// time), like at tool registration - they are never stored, never
    /// logged.
    pub fn build_browser_backend(
        cfg: &BrowserToolConfig,
        secrets: &pantheon_secrets::SecretsBroker,
    ) -> Result<
        Option<std::sync::Arc<dyn pantheon_web::browser::BrowserBackend>>,
        pantheon_api::error::PantheonError,
    > {
        if !cfg.enabled {
            return Ok(None);
        }
        let backend_config = browser_backend_config(cfg, secrets);
        pantheon_web::browser::build_backend(cfg.backend, &backend_config)
            .map(Some)
            .map_err(pantheon_api::error::PantheonError::from)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn code_intel_false_from_section_disables_the_group() {
            let mut section = pantheon_api::config::ToolsSection::default();
            section.code_intel = Some(false);
            let enablement = ToolEnablement::from_section(&section);
            assert!(!enablement.is_enabled(pantheon_api::config::ToolGroup::CodeIntel));
            // Absent stays on, like every other group.
            let enablement =
                ToolEnablement::from_section(&pantheon_api::config::ToolsSection::default());
            assert!(enablement.is_enabled(pantheon_api::config::ToolGroup::CodeIntel));
            // Every flag round-trips, not just code_intel: a future dropped
            // field must fail here instead of silently staying on.
            let mut section = pantheon_api::config::ToolsSection::default();
            section.web_search = Some(false);
            section.browser = Some(false);
            section.terminal = Some(false);
            section.files = Some(false);
            section.memory = Some(false);
            section.skills = Some(false);
            section.tasks = Some(false);
            section.delegation = Some(false);
            section.ask_user = Some(false);
            section.vault = Some(false);
            section.voice = Some(false);
            section.vision = Some(false);
            section.video_analysis = Some(false);
            section.computer_use = Some(false);
            section.plugins = Some(false);
            section.code_intel = Some(false);
            let enablement = ToolEnablement::from_section(&section);
            for group in [
                pantheon_api::config::ToolGroup::WebSearch,
                pantheon_api::config::ToolGroup::Browser,
                pantheon_api::config::ToolGroup::Terminal,
                pantheon_api::config::ToolGroup::Files,
                pantheon_api::config::ToolGroup::Memory,
                pantheon_api::config::ToolGroup::Skills,
                pantheon_api::config::ToolGroup::Tasks,
                pantheon_api::config::ToolGroup::Delegation,
                pantheon_api::config::ToolGroup::AskUser,
                pantheon_api::config::ToolGroup::Vault,
                pantheon_api::config::ToolGroup::Voice,
                pantheon_api::config::ToolGroup::Vision,
                pantheon_api::config::ToolGroup::VideoAnalysis,
                pantheon_api::config::ToolGroup::ComputerUse,
                pantheon_api::config::ToolGroup::Plugins,
                pantheon_api::config::ToolGroup::CodeIntel,
            ] {
                assert!(!enablement.is_enabled(group), "{group:?} must be off");
            }
        }
    }
}

pub mod watchdog {
    //! Activity-based turn watchdog.
    //!
    //! Scope contract (turn-boundary ledger-liveness ONLY):
    //! - The watchdog is polled at turn entry (`drive`'s head), never
    //!   mid-turn: it cannot poll while blocked inside the synchronous
    //!   provider call, so it can never kill a live turn. Silence alone
    //!   is slow, never stuck.
    //! - The probe is a ledger-status read. Only a FAILED probe escalates
    //!   to a kill; a successful probe is ordinary activity, and merely
    //!   observing silence past the budget yields `Probe`, never `Kill`.
    //! - A human pause is an explicit state, not inactivity.
    //!
    //! A turn is not killed because it ran for a long wall-clock duration.  The
    //! runtime calls `activity()` whenever it observes progress, and the
    //! watchdog probes only after a silence longer than the stall budget.  A
    //! human pause is an explicit state, not inactivity.

    use std::time::{Duration, Instant};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum WatchdogAction {
        Continue,
        Probe,
        Kill,
    }

    #[derive(Debug, Clone)]
    pub struct TurnWatchdog {
        stall_budget: Duration,
        probe_timeout: Duration,
        last_activity: Instant,
        probe_started: Option<Instant>,
        paused: bool,
        killed: bool,
    }

    impl Default for TurnWatchdog {
        fn default() -> Self {
            Self::new(Duration::from_secs(30))
        }
    }

    impl TurnWatchdog {
        pub fn new(stall_budget: Duration) -> Self {
            Self {
                stall_budget,
                probe_timeout: Duration::from_secs(2),
                last_activity: Instant::now(),
                probe_started: None,
                paused: false,
                killed: false,
            }
        }

        /// Construct with an explicit clock base, so `poll_at` can be driven
        /// by a test without sleeping. Production uses `new`.
        pub fn new_at(stall_budget: Duration, now: Instant) -> Self {
            Self {
                stall_budget,
                probe_timeout: Duration::from_secs(2),
                last_activity: now,
                probe_started: None,
                paused: false,
                killed: false,
            }
        }

        pub fn with_probe_timeout(mut self, timeout: Duration) -> Self {
            self.probe_timeout = timeout;
            self
        }

        /// Configure from `PANTHEON_STALL_BUDGET_MS` and
        /// `PANTHEON_PROBE_TIMEOUT_MS`, with safe defaults.
        pub fn from_env() -> Self {
            let stall = std::env::var("PANTHEON_STALL_BUDGET_MS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_millis)
                .unwrap_or_else(|| Duration::from_secs(30));
            let probe = std::env::var("PANTHEON_PROBE_TIMEOUT_MS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(Duration::from_millis)
                .unwrap_or_else(|| Duration::from_secs(2));
            Self::new(stall).with_probe_timeout(probe)
        }

        pub fn activity(&mut self) {
            self.activity_at(Instant::now());
        }

        /// Deterministic `activity` at an explicit clock sample.
        pub fn activity_at(&mut self, now: Instant) {
            self.last_activity = now;
            self.probe_started = None;
        }

        /// Pause without charging elapsed time to the stall budget.  This is
        /// used while waiting for a human approval decision.
        pub fn pause(&mut self) {
            self.paused = true;
            self.probe_started = None;
        }

        /// Resume starts a fresh activity window, so a human pause never causes
        /// a post-resume kill.
        pub fn resume(&mut self) {
            self.paused = false;
            self.activity();
        }

        /// Deterministic `resume` at an explicit clock sample.
        pub fn resume_at(&mut self, now: Instant) {
            self.paused = false;
            self.activity_at(now);
        }

        pub fn stall_for(&self) -> Duration {
            self.last_activity.elapsed()
        }

        /// Deterministic `stall_for` at an explicit clock sample.
        pub fn stall_for_at(&self, now: Instant) -> Duration {
            now.saturating_duration_since(self.last_activity)
        }

        /// Report the result of the probe.  Only a failed probe escalates to a
        /// kill; a successful probe is ordinary activity.
        pub fn probe_result(&mut self, succeeded: bool) -> WatchdogAction {
            self.probe_result_at(succeeded, Instant::now())
        }

        /// Deterministic `probe_result` at an explicit clock sample, so a test
        /// can drive the full probe lifecycle on an injected timeline instead of
        /// sleeping and racing the real clock.
        pub fn probe_result_at(&mut self, succeeded: bool, now: Instant) -> WatchdogAction {
            if succeeded {
                self.last_activity = now;
                self.probe_started = None;
                WatchdogAction::Continue
            } else {
                self.killed = true;
                WatchdogAction::Kill
            }
        }

        /// Poll and run a lightweight liveness probe when the stall budget is
        /// exceeded. A successful probe records activity; only an error becomes
        /// `Kill`.
        pub fn poll_with_probe<F>(&mut self, probe: F) -> WatchdogAction
        where
            F: FnOnce() -> Result<(), String>,
        {
            match self.poll() {
                WatchdogAction::Probe => match probe() {
                    Ok(()) => self.probe_result(true),
                    Err(_) => self.probe_result(false),
                },
                other => other,
            }
        }

        pub fn poll(&mut self) -> WatchdogAction {
            if self.killed {
                return WatchdogAction::Kill;
            }
            if self.paused {
                return WatchdogAction::Continue;
            }
            if self.stall_for() < self.stall_budget {
                return WatchdogAction::Continue;
            }
            match self.probe_started {
                None => {
                    self.probe_started = Some(Instant::now());
                    WatchdogAction::Probe
                }
                Some(_) => WatchdogAction::Probe,
            }
        }

        /// Treat an expired in-flight probe as a failed probe. This is explicit
        /// so merely observing silence can never escalate to a kill.
        pub fn probe_timed_out(&mut self) -> WatchdogAction {
            self.probe_timed_out_at(Instant::now())
        }

        /// Deterministic `probe_timed_out` at an explicit clock sample.
        pub fn probe_timed_out_at(&mut self, now: Instant) -> WatchdogAction {
            match self.probe_started {
                Some(started) if now.saturating_duration_since(started) >= self.probe_timeout => {
                    self.probe_result_at(false, now)
                }
                Some(_) => WatchdogAction::Probe,
                None => WatchdogAction::Continue,
            }
        }

        /// Deterministic variant for tests and supervisors that already have a
        /// monotonic clock sample.
        pub fn poll_at(&mut self, now: Instant) -> WatchdogAction {
            if self.killed {
                return WatchdogAction::Kill;
            }
            if self.paused {
                return WatchdogAction::Continue;
            }
            let silent = now.saturating_duration_since(self.last_activity);
            if silent < self.stall_budget {
                return WatchdogAction::Continue;
            }
            match self.probe_started {
                None => {
                    self.probe_started = Some(now);
                    WatchdogAction::Probe
                }
                Some(_) => WatchdogAction::Probe,
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn base() -> (TurnWatchdog, Instant) {
            let now = Instant::now();
            (
                TurnWatchdog::new_at(Duration::from_secs(30), now)
                    .with_probe_timeout(Duration::from_secs(2)),
                now,
            )
        }

        #[test]
        fn poll_at_continues_within_budget_then_probes_after_it() {
            let (mut w, t0) = base();
            assert_eq!(
                w.poll_at(t0 + Duration::from_secs(29)),
                WatchdogAction::Continue
            );
            assert_eq!(
                w.poll_at(t0 + Duration::from_secs(30)),
                WatchdogAction::Probe
            );
            // Still probing until the probe resolves or times out.
            assert_eq!(
                w.poll_at(t0 + Duration::from_secs(31)),
                WatchdogAction::Probe
            );
        }

        #[test]
        fn activity_resets_the_silence_window_and_clears_a_probe() {
            let (mut w, t0) = base();
            assert_eq!(
                w.poll_at(t0 + Duration::from_secs(30)),
                WatchdogAction::Probe
            );
            w.activity_at(t0 + Duration::from_secs(31));
            assert_eq!(
                w.poll_at(t0 + Duration::from_secs(60)),
                WatchdogAction::Continue
            );
            assert_eq!(
                w.poll_at(t0 + Duration::from_secs(61)),
                WatchdogAction::Probe
            );
        }

        #[test]
        fn successful_probe_is_activity_failed_probe_kills_and_sticks() {
            let (mut w, t0) = base();
            assert_eq!(
                w.poll_at(t0 + Duration::from_secs(30)),
                WatchdogAction::Probe
            );
            assert_eq!(
                w.probe_result_at(true, t0 + Duration::from_secs(31)),
                WatchdogAction::Continue
            );
            assert_eq!(
                w.poll_at(t0 + Duration::from_secs(61)),
                WatchdogAction::Probe
            );
            assert_eq!(
                w.probe_result_at(false, t0 + Duration::from_secs(62)),
                WatchdogAction::Kill
            );
            assert_eq!(
                w.poll_at(t0 + Duration::from_secs(999)),
                WatchdogAction::Kill
            );
            assert_eq!(
                w.probe_timed_out_at(t0 + Duration::from_secs(999)),
                WatchdogAction::Kill
            );
        }

        #[test]
        fn probe_timed_out_only_fires_after_the_probe_timeout() {
            let (mut w, t0) = base();
            // No probe in flight: observing silence never kills.
            assert_eq!(
                w.probe_timed_out_at(t0 + Duration::from_secs(3600)),
                WatchdogAction::Continue
            );
            assert_eq!(
                w.poll_at(t0 + Duration::from_secs(30)),
                WatchdogAction::Probe
            );
            assert_eq!(
                w.probe_timed_out_at(t0 + Duration::from_secs(31)),
                WatchdogAction::Probe
            );
            assert_eq!(
                w.probe_timed_out_at(t0 + Duration::from_secs(32)),
                WatchdogAction::Kill
            );
        }

        #[test]
        fn pause_never_kills_and_resume_starts_a_fresh_window() {
            let (mut w, t0) = base();
            w.pause();
            assert_eq!(
                w.poll_at(t0 + Duration::from_secs(3600)),
                WatchdogAction::Continue
            );
            w.resume_at(t0 + Duration::from_secs(3600));
            assert_eq!(
                w.poll_at(t0 + Duration::from_secs(3629)),
                WatchdogAction::Continue
            );
            assert_eq!(
                w.poll_at(t0 + Duration::from_secs(3630)),
                WatchdogAction::Probe
            );
        }

        #[test]
        fn silence_alone_never_kills_without_a_failed_probe() {
            // Turn-boundary scope: however long the silence, observing it
            // only ever yields Probe. Kill requires an explicit failed
            // probe (or an expired in-flight probe), never the clock.
            let (mut w, t0) = base();
            assert_eq!(
                w.poll_at(t0 + Duration::from_secs(3600)),
                WatchdogAction::Probe
            );
            assert_eq!(
                w.poll_at(t0 + Duration::from_secs(7200)),
                WatchdogAction::Probe
            );
            // A probe that succeeds is activity, not a kill.
            assert_eq!(
                w.probe_result_at(true, t0 + Duration::from_secs(7201)),
                WatchdogAction::Continue
            );
        }
    }
}
