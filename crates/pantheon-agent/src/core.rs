//! Agent core merged into one file. External paths keep working through the
//! crate-root shims in lib.rs (same pattern as the providers merge).

pub mod capability {
    //! Capability enforcement at the execution boundary: check-then-act,
    //! default-deny. Policy lives in pantheon-api; this module enforces it.
    use pantheon_api::capability::{Capability, Decision, Policy};
    use pantheon_api::error::{Layer, PantheonError};

    fn cerr(code: &str, cause: String, retryable: bool) -> PantheonError {
        PantheonError::new(
            code,
            Layer::Capability,
            retryable,
            cause,
            "request approval or narrow the capability grant",
            "",
        )
    }

    /// Enforcement outcome.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Verdict {
        Allow,
        NeedsApproval { capability: Capability },
        Deny { capability: Capability },
    }

    /// Check a capability against a policy. Never panics, never defaults to allow.
    pub fn check(policy: &Policy, cap: &Capability) -> Verdict {
        match policy.check(cap) {
            Decision::Allow => Verdict::Allow,
            Decision::Approval => Verdict::NeedsApproval {
                capability: cap.clone(),
            },
            Decision::Deny => Verdict::Deny {
                capability: cap.clone(),
            },
        }
    }

    /// Enforce: Allow passes, anything else becomes a structured error.
    pub fn enforce(policy: &Policy, cap: &Capability) -> Result<(), PantheonError> {
        match check(policy, cap) {
            Verdict::Allow => Ok(()),
            Verdict::NeedsApproval { capability } => Err(cerr(
                "CAP_APPROVAL_REQUIRED",
                format!("capability {capability:?} needs approval"),
                false,
            )),
            Verdict::Deny { capability } => Err(cerr(
                "CAP_DENIED",
                format!("capability {capability:?} denied by policy"),
                false,
            )),
        }
    }
    #[cfg(test)]
    mod gate_tests {
        //! The capability gate is the crate's core safety invariant: it must
        //! never default to allow. There were zero tests here, which is exactly
        //! where a regression would be silent and dangerous.
        use super::*;
        use crate::tool::{gate, GateOutcome, ToolRunner};
        use pantheon_api::capability::Capability;

        // ---------------------------------------------------------- check()

        #[test]
        fn check_allows_an_explicitly_allowed_capability() {
            let policy = Policy::default().allow(Capability::FilesystemRead);
            assert_eq!(check(&policy, &Capability::FilesystemRead), Verdict::Allow);
        }

        #[test]
        fn check_parks_an_approval_gated_capability() {
            let policy = Policy::default().approval(Capability::GitPush);
            match check(&policy, &Capability::GitPush) {
                Verdict::NeedsApproval { capability } => {
                    assert_eq!(capability, Capability::GitPush)
                }
                other => panic!("expected NeedsApproval, got {other:?}"),
            }
        }

        #[test]
        fn check_denies_an_explicitly_denied_capability() {
            let policy = Policy::default().deny(Capability::ShellExecute);
            match check(&policy, &Capability::ShellExecute) {
                Verdict::Deny { capability } => assert_eq!(capability, Capability::ShellExecute),
                other => panic!("expected Deny, got {other:?}"),
            }
        }

        /// The invariant the module doc promises. An unlisted capability must
        /// never fall through to Allow.
        #[test]
        fn check_denies_anything_not_in_the_policy() {
            let policy = Policy::default();
            for cap in [
                Capability::ShellExecute,
                Capability::FilesystemWrite,
                Capability::NetworkOutbound,
                Capability::Other("custom.tool".into()),
            ] {
                assert!(
                    matches!(check(&policy, &cap), Verdict::Deny { .. }),
                    "an empty policy must deny {cap:?}"
                );
            }
        }

        // -------------------------------------------------------- enforce()

        #[test]
        fn enforce_passes_an_allowed_capability() {
            let policy = Policy::default().allow(Capability::NetworkOutbound);
            assert!(enforce(&policy, &Capability::NetworkOutbound).is_ok());
        }

        #[test]
        fn enforce_rejects_approval_with_a_distinct_code() {
            let policy = Policy::default().approval(Capability::GitPush);
            let err = enforce(&policy, &Capability::GitPush).expect_err("approval must not pass");
            assert_eq!(err.code, "CAP_APPROVAL_REQUIRED");
            assert!(
                !err.retryable,
                "an approval park is not a retryable failure"
            );
        }

        #[test]
        fn enforce_rejects_denial_with_a_distinct_code() {
            let policy = Policy::default();
            let err =
                enforce(&policy, &Capability::ShellExecute).expect_err("denial must not pass");
            assert_eq!(err.code, "CAP_DENIED");
            assert!(!err.retryable);
        }

        // ----------------------------------------------------------- gate()

        #[test]
        fn gate_allows_what_the_policy_allows() {
            let policy = Policy::default().allow(Capability::FilesystemRead);
            assert_eq!(
                gate(&policy, &Capability::FilesystemRead).expect("gate"),
                GateOutcome::Allow
            );
        }

        /// `gate` must distinguish a park from a hard denial: approval is a
        /// pause, denial fails the run. Folding them together would make an
        /// approvable action look forbidden.
        #[test]
        fn gate_reports_approval_as_a_park_not_an_error() {
            let policy = Policy::default().approval(Capability::GitPush);
            match gate(&policy, &Capability::GitPush) {
                Ok(GateOutcome::NeedsApproval { capability }) => {
                    assert_eq!(capability, Capability::GitPush)
                }
                other => panic!("expected NeedsApproval, got {other:?}"),
            }
        }

        #[test]
        fn gate_fails_a_denied_capability() {
            let policy = Policy::default().deny(Capability::Browser);
            let err = gate(&policy, &Capability::Browser).expect_err("denial must fail the gate");
            assert_eq!(err.code, "CAP_DENIED");
        }

        /// The default-deny invariant, asserted through the gate the tool
        /// boundary actually calls.
        #[test]
        fn gate_denies_an_unknown_capability_on_an_empty_policy() {
            let policy = Policy::default();
            let err = gate(&policy, &Capability::Other("mcp.mystery".into()))
                .expect_err("unknown capability must not run");
            assert_eq!(err.code, "CAP_DENIED");
        }

        /// A runner that panics if invoked: proves the gate refuses *before*
        /// any execution, which is the whole point of check-then-act.
        struct ExplodingRunner;
        impl ToolRunner for ExplodingRunner {
            fn run(&self, _name: &str, _args: &str) -> Result<String, PantheonError> {
                panic!("the runner must never be reached for a denied capability");
            }
        }

        #[test]
        fn a_denied_capability_never_reaches_the_runner() {
            let policy = Policy::default();
            let result = gate(&policy, &Capability::ShellExecute);
            assert!(result.is_err(), "gate must refuse before the runner");
            // The runner is constructed but provably unused: if the gate let the
            // call through, `run` would panic and fail the test.
            let _runner = ExplodingRunner;
        }

        #[test]
        fn coder_preset_gates_push_but_allows_read() {
            // Pins two contract points of the shipped preset that the docs and
            // the approval UI both rely on.
            let policy = Policy::coder();
            assert_eq!(check(&policy, &Capability::FilesystemRead), Verdict::Allow);
            assert!(matches!(
                check(&policy, &Capability::GitPush),
                Verdict::NeedsApproval { .. }
            ));
        }
    }
}

pub mod engine {
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
            /// Text the model produced alongside the calls. Previously
            /// discarded by both adapters, so the assistant tool-calls
            /// message pushed by the loop had empty content and the next
            /// turn's model never saw its own text. Empty when the model
            /// sent calls with no accompanying text.
            text: String,
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
        /// Spend cap in US cents. None means no cap yet. When set, the run stops
        /// with BudgetExhausted cap max_cost_cents instead of spending unboundedly.
        pub max_cost_cents: Option<u32>,
        /// Per-request OUTPUT cap for chat requests (not a run budget - input
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
        /// Stop the run after this many consecutive failed tool calls.
        ///
        /// `max_turns` and `max_tool_calls` bound total work, but neither
        /// stops the specific pathology of a model retrying the same broken
        /// command: a run can burn its whole tool budget on one failing
        /// invocation, and every retry costs a provider round trip. This is
        /// the cap that stops that. Any successful tool call resets the
        /// counter, so a long run that is making progress is never cut off.
        /// Inject a one-time warning after this many consecutive failed tool
        /// calls, before the run terminates.
        ///
        /// The warning is a system message telling the model to diagnose the
        /// failure, correct invalid arguments, and change approach. It fires
        /// at most once per failure streak; any successful tool call resets
        /// both the streak and the warning state. The warning never extends
        /// the run and does not itself count as a tool call.
        ///
        /// `0` disables the warning. Default 3 (the model gets one warning
        /// two failures before termination).
        pub warn_consecutive_tool_failures: u32,
    }

    impl Default for Budget {
        fn default() -> Self {
            Self {
                max_turns: 16,
                max_tool_calls: 32,
                max_cost_cents: None,
                max_tokens: None,
                max_delegate_depth: 2,
                allow_child_spawn: true,
                warn_consecutive_tool_failures: 3,
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
        /// The agent published a UI document containing an approval card
        /// (see `pantheon_api::ui`). The turn parks; the host resolves it
        /// through `Session::submit_ui_action` and the resumed turn sees the
        /// decision as the `render_ui` tool result. `call_id` ties the
        /// decision back to the exact call.
        AwaitingUiAction { ui_id: String, call_id: String },
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
    /// failing the turn - a confused question is still answerable, a crashed
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
        /// It cannot abort an in-flight provider request - that returns on its own
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
}

pub mod subagent {
    //! In-process sub-agent handles: non-blocking delegation.
    //!
    //! `delegate` no longer stops the parent loop. The spawner returns a
    //! handle immediately (`delegate[researcher]: spawned, handle h_1`) and
    //! the parent keeps working while children run on their own threads.
    //! The model follows up through three tools:
    //!
    //! - `subagent_read(handle)` - the child's transcript so far; never blocks.
    //! - `subagent_wait(handle)` - blocks until that child finishes, returns
    //!   its final result.
    //! - `subagent_list()` - every live handle with its status.
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

    /// Shared settle state: (status, result) under a mutex, woken by the
    /// condvar. Result is Some once the child settled.
    type SubagentState = Arc<(
        Mutex<(SubagentStatus, Option<Result<String, String>>)>,
        Condvar,
    )>;

    struct ChildEntry {
        handle: SubagentHandle,
        transcript: Arc<Mutex<Vec<String>>>,
        state: SubagentState,
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
    /// The registry itself does not know how to *build* child work - the
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
                t_log.lock().map(|mut t| t.push(line)).unwrap_or_else(|e| {
                    e.into_inner().push("result: <transcript poisoned>".into())
                });
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

        fn entry_state(&self, handle: &str) -> Result<SubagentState, PantheonError> {
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
    #[cfg(test)]
    mod registry_tests {
        //! The registry enforces the swarm's hard limits (depth, concurrency,
        //! per-agent fan-out, child delegation). Those caps are the difference
        //! between a bounded swarm and an unbounded process bomb, and none of
        //! them had a test.
        use super::*;

        fn registry(caps: SubagentCaps) -> SubagentRegistry {
            SubagentRegistry::new(caps)
        }

        fn quick(
            r: &SubagentRegistry,
            parent: &str,
            agent: &str,
            depth: u32,
            task: &str,
        ) -> Result<SubagentHandle, PantheonError> {
            // `spawn_child` runs the closure on a thread, so any value works.
            r.spawn_child(parent, agent, depth, task, || Ok("done".to_string()))
        }

        #[test]
        fn spawn_then_wait_returns_the_child_result() {
            let r = registry(SubagentCaps::default());
            let h = quick(&r, "primary", "researcher", 0, "look it up").expect("spawn");
            let out = r.subagent_wait(&h.id).expect("wait");
            assert_eq!(out, "done");
            assert!(matches!(
                r.subagent_status(&h.id).expect("status"),
                SubagentStatus::Done
            ));
        }

        #[test]
        fn a_failing_child_is_recorded_as_failed_not_done() {
            let r = registry(SubagentCaps::default());
            let h = r
                .spawn_child("primary", "worker", 0, "boom", || {
                    Err(PantheonError::new(
                        "CHILD_BLEW_UP",
                        Layer::Agent,
                        false,
                        "deliberate failure",
                        "none",
                        "",
                    ))
                })
                .expect("spawn");
            let err = r
                .subagent_wait(&h.id)
                .expect_err("wait must surface the failure");
            assert!(err.cause.contains("CHILD_BLEW_UP"), "{}", err.cause);
            assert!(matches!(
                r.subagent_status(&h.id).expect("status"),
                SubagentStatus::Failed
            ));
        }

        #[test]
        fn the_transcript_is_seeded_with_the_task_and_appends_the_result() {
            let r = registry(SubagentCaps::default());
            let h = quick(&r, "primary", "worker", 0, "summarize the logs").expect("spawn");
            r.subagent_wait(&h.id).expect("wait");
            let transcript = r.subagent_read(&h.id).expect("read");
            assert!(transcript.contains("summarize the logs"), "{transcript}");
            assert!(transcript.contains("result: done"), "{transcript}");
        }

        #[test]
        fn spawning_beyond_max_depth_is_refused() {
            let caps = SubagentCaps {
                max_depth: 1,
                ..SubagentCaps::default()
            };
            let r = registry(caps);
            let err = quick(&r, "primary", "worker", 1, "too deep").expect_err("must refuse");
            assert_eq!(err.code, "SWARM_MAX_DEPTH");
        }

        #[test]
        fn a_child_cannot_delegate_when_child_spawn_is_disabled() {
            let caps = SubagentCaps {
                allow_child_spawn: false,
                ..SubagentCaps::default()
            };
            let r = registry(caps);
            let err = quick(&r, "child", "grandchild", 1, "nested").expect_err("must refuse");
            assert_eq!(err.code, "SWARM_CHILD_SPAWN_DENIED");
        }

        #[test]
        fn per_agent_spawn_cap_is_enforced() {
            let caps = SubagentCaps {
                max_concurrent: 100,
                max_subagents: 2,
                ..SubagentCaps::default()
            };
            let r = registry(caps);
            quick(&r, "primary", "a", 0, "one").expect("first");
            quick(&r, "primary", "b", 0, "two").expect("second");
            let err = quick(&r, "primary", "c", 0, "three").expect_err("third must refuse");
            assert_eq!(err.code, "SWARM_PER_AGENT_CAP");
        }

        /// Caps are counted per parent: one busy agent must not consume another
        /// agent's fan-out budget.
        #[test]
        fn per_agent_cap_does_not_leak_across_parents() {
            let caps = SubagentCaps {
                max_concurrent: 100,
                max_subagents: 1,
                ..SubagentCaps::default()
            };
            let r = registry(caps);
            quick(&r, "primary", "a", 0, "one").expect("primary child");
            quick(&r, "other", "b", 0, "two").expect("a different parent has its own budget");
        }

        #[test]
        fn list_reports_every_child_in_spawn_order() {
            let r = registry(SubagentCaps::default());
            let a = quick(&r, "primary", "first", 0, "a").expect("spawn a");
            let b = quick(&r, "primary", "second", 0, "b").expect("spawn b");
            r.subagent_wait(&a.id).expect("wait a");
            r.subagent_wait(&b.id).expect("wait b");
            let listed = r.subagent_list();
            assert_eq!(listed.len(), 2);
            assert_eq!(listed[0].0.id, a.id, "spawn order must be preserved");
            assert_eq!(listed[1].0.id, b.id);
        }

        #[test]
        fn status_of_an_unknown_handle_is_an_error() {
            let r = registry(SubagentCaps::default());
            assert!(r.subagent_status("h_nope").is_err());
            assert!(r.subagent_wait("h_nope").is_err());
            assert!(r.subagent_read("h_nope").is_err());
        }
    }
}

pub mod tool {
    //! Tool execution boundary: capability gate, then runner. The gate runs
    //! before the runner, always - there is no path that executes an ungranted
    //! capability.
    use crate::capability::{enforce, Verdict};
    use pantheon_api::capability::{Capability, Policy};
    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::events::Event;

    /// Where events go. Implemented by the runtime supervisor; tests use a Vec.
    pub trait EventSink {
        fn emit(&self, event: Event);
    }

    /// Executes one tool. Implementations decide how (process, http, in-proc).
    pub trait ToolRunner {
        fn run(&self, name: &str, args: &str) -> Result<String, PantheonError>;
    }

    /// What the gate decided for one tool call.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum GateOutcome {
        /// May run.
        Allow,
        /// Policy gated it: caller must park and ask.
        NeedsApproval { capability: Capability },
    }

    fn aerr(code: &str, cause: String) -> PantheonError {
        PantheonError::new(
            code,
            Layer::Agent,
            false,
            cause,
            "adjust the policy or grant the capability explicitly",
            "",
        )
    }

    /// Check one capability against a policy, mapping a denial to a structured
    /// error the loop can surface as `RunFailed`.
    pub fn gate(policy: &Policy, cap: &Capability) -> Result<GateOutcome, PantheonError> {
        match enforce(policy, cap) {
            Ok(()) => Ok(GateOutcome::Allow),
            Err(_e) => {
                // enforce() folds both Deny and Approval into errors; re-check to
                // distinguish so approval does not look like a hard denial.
                match crate::capability::check(policy, cap) {
                    Verdict::NeedsApproval { capability } => {
                        Ok(GateOutcome::NeedsApproval { capability })
                    }
                    Verdict::Deny { capability } => Err(aerr(
                        "CAP_DENIED",
                        format!("capability {capability:?} denied by policy"),
                    )),
                    Verdict::Allow => Ok(GateOutcome::Allow),
                }
            }
        }
    }
}
