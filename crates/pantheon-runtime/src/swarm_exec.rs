//! Swarm execution: fan one task out to N agents, judge completion,
//! retry with judge feedback.
//!
//! [`SwarmOrchestrator`] is the runtime half of the swarm HTTP API. It
//! owns swarm records (keyed by `sw_…` ids), drives agents through a
//! [`SwarmWorker`] seam, and runs the completion judge from
//! [`crate::judge`] when all agents settle.
//!
//! Lifecycle of one swarm:
//!
//! 1. `create` registers N agents as `waiting`, then spawns each through
//!    the worker (`waiting` → `working`). The swarm is `running`.
//! 2. Status polls refresh each agent from the worker. When every agent
//!    is `done`/`failed`, the swarm moves to `judging`; with
//!    `judge: true` the judge runs over the combined transcripts.
//! 3. Verdict done → `complete`. Verdict not done (or, with no judge,
//!    any failed agent) → `incomplete`, which is retryable.
//! 4. `retry` (rounds < 3, verdict present, verdict not done) registers
//!    a fresh round of agents as `waiting` with the judge's feedback
//!    appended to their task, then spawns them (`waiting` → `working`).
//!
//! Staged execution (`create_staged`) runs an [`ExecutionPlan`]: ordered
//! stages, each stage's members launched in parallel, the next stage
//! spawning only after the previous one settles. Stage outputs (member
//! transcripts) are collected and embedded as structured input in the
//! next stage's task — the orchestrator moves bytes, not schemas: it
//! does not parse or validate outputs against the contracts (that would
//! need a model call per handoff); contracts shape the members' output
//! through their task text, and the review loop checks the result.
//! Stages carrying `loop_back_to` are review stages: after settling, the
//! orchestrator extracts the reviewer's verdict. The primary channel is
//! the `verdict` tool: reviewer runs are spawned with the tool registered
//! (via [`SwarmWorker::spawn_reviewer`]) and instructed to call
//! `verdict(pass, failed_items, notes)` exactly once at the end of the
//! review; the orchestrator reads the structured args from the run's tool
//! calls (last call wins). Two fallbacks follow, in order: a fenced
//! ```verdict JSON block (`{"pass": bool, "failed_items": [...]}`) in the
//! transcript, then a keyword scan for verdict pass/fail language. On
//! fail the orchestrator re-runs from the target stage with the failed
//! items as feedback, bounded by `max_review_iterations` (default 3);
//! exhausting the bound marks the swarm `incomplete` with an escalation
//! note for the lead. A review stage whose reviewer yields no verdict
//! from any source counts as a failed verification ("no verdict emitted").
//!
//! The lead: `create_staged` spawns the lead's coordination run first.
//! Its run id is the swarm's `lead_run_id` — the single user-facing run,
//! which `use team` returns. Member runs are never user-facing: their
//! tasks instruct them to report to the lead, and their outputs reach
//! the lead through the stage handoff chain and the swarm's execution
//! log. What the orchestrator cannot do with the current worker seam:
//! message a running lead run (no such primitive), so the lead's run
//! does not receive live stage updates — the full execution log lives
//! in the swarm record (`transcript()`).
//!
//! [`ScriptedWorker`] is the deterministic test double.

use crate::judge::{judge_completion_with, JudgeTransport, JudgeVerdict};
use pantheon_api::capability::VERDICT_TOOL_NAME;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Structured swarm error. `http_status` lets the HTTP layer map it
/// without inventing its own codes.
#[derive(Debug, Clone)]
pub struct SwarmError {
    pub code: String,
    pub message: String,
    pub http_status: u16,
}

impl SwarmError {
    fn bad_request(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            http_status: 400,
        }
    }

    fn not_found(swarm_id: &str) -> Self {
        Self {
            code: "SWARM_NOT_FOUND".into(),
            message: format!("no such swarm {swarm_id:?}"),
            http_status: 404,
        }
    }

    pub fn worker(message: impl Into<String>) -> Self {
        Self {
            code: "SWARM_WORKER".into(),
            message: message.into(),
            http_status: 500,
        }
    }
}

/// Lifecycle status of a whole swarm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SwarmStatus {
    Running,
    Judging,
    Complete,
    Incomplete,
}

impl SwarmStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SwarmStatus::Running => "running",
            SwarmStatus::Judging => "judging",
            SwarmStatus::Complete => "complete",
            SwarmStatus::Incomplete => "incomplete",
        }
    }
}

/// Lifecycle status of one agent inside a swarm. `waiting` = registered
/// for an upcoming round but not yet spawned; observable in the retry
/// relaunch window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SwarmAgentStatus {
    Working,
    Waiting,
    Done,
    Failed,
}

impl SwarmAgentStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            SwarmAgentStatus::Working => "working",
            SwarmAgentStatus::Waiting => "waiting",
            SwarmAgentStatus::Done => "done",
            SwarmAgentStatus::Failed => "failed",
        }
    }
}

/// How the orchestrator runs one agent. Implemented by the dashboard's
/// `SubprocessWorker` in production (one `pantheon run` child per
/// agent) and by [`ScriptedWorker`] in tests.
pub trait SwarmWorker: Send + Sync {
    /// Spawn the agent's work; returns its run id (`r_…`). The worker
    /// seeds the run's transcript with the task. `profile` is the
    /// `[agents.<name>]` table the agent represents (`"default"` in count
    /// mode); workers surface it in titles/logs. (Per-profile subprocess
    /// execution threads the profile into `pantheon run --agent`, so the
    /// child session loads that profile's SOUL.md / USER.md / AGENTS.md.
    /// the profile currently drives naming, avatars, and validation.)
    fn spawn_agent(
        &self,
        swarm_id: &str,
        agent_name: &str,
        profile: &str,
        task: &str,
    ) -> Result<String, SwarmError>;
    /// Current agent status. Unknown run ids report `Failed` — a lost
    /// child is a failed child, never a silent hang.
    fn agent_status(&self, run_id: &str) -> Result<SwarmAgentStatus, SwarmError>;
    /// The agent's transcript so far.
    fn agent_transcript(&self, run_id: &str) -> Result<String, SwarmError>;
    /// Every tool call the agent's run issued, in call order. Backs
    /// verdict extraction for staged review stages: the orchestrator
    /// reads the `verdict` tool's structured args from here instead of
    /// parsing the transcript. Unknown run ids report an empty list.
    fn agent_tool_calls(&self, run_id: &str) -> Result<Vec<ToolCall>, SwarmError>;
    /// Spawn a reviewer for a staged review stage. The default impl
    /// delegates to [`SwarmWorker::spawn_agent`]; workers whose agents
    /// are real model runs override this to register the `verdict` tool
    /// on the child (so the reviewer can emit its verdict as a tool
    /// call rather than prose).
    fn spawn_reviewer(
        &self,
        swarm_id: &str,
        agent_name: &str,
        profile: &str,
        task: &str,
    ) -> Result<String, SwarmError> {
        self.spawn_agent(swarm_id, agent_name, profile, task)
    }
}

/// One tool call a run issued, in call order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    /// Tool name as registered (e.g. `verdict`).
    pub tool: String,
    /// Arguments as issued (JSON for model-issued calls).
    pub args: String,
}

#[derive(Debug, Clone)]
struct AgentRecord {
    name: String,
    /// Profile this agent was launched for (`"default"` in count mode, the
    /// `[agents.<name>]` table name in profiles mode). Used for display and
    /// retry; the production worker also threads it into the run title.
    profile: String,
    run_id: String,
    status: SwarmAgentStatus,
    round: u32,
    /// Staged execution: which stage this agent belongs to (`None` for
    /// classic swarms and for the lead coordination run).
    stage: Option<u32>,
    /// True for the lead coordination run of a staged swarm.
    lead: bool,
}

/// One agent to launch: a display name plus the profile it represents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSpec {
    pub name: String,
    pub profile: String,
}

/// Primary execution pattern of a staged plan. A label for the plan
/// text: orchestration is always "stages in order, members in parallel
/// within a stage, review via `loop_back_to`" — the topology names the
/// dominant shape for the crew's benefit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanTopology {
    Pipeline,
    ParallelMerge,
    ReviewLoop,
}

impl PlanTopology {
    pub fn describe(&self) -> &'static str {
        match self {
            PlanTopology::Pipeline => "pipeline — stages run in order, each feeding the next",
            PlanTopology::ParallelMerge => {
                "parallel merge — members work in parallel, then results merge"
            }
            PlanTopology::ReviewLoop => {
                "review loop — stages run in order with a bounded verify loop"
            }
        }
    }
}

/// One stage of a staged execution plan.
#[derive(Debug, Clone)]
pub struct StageSpec {
    /// Stage name (unique within the plan).
    pub name: String,
    /// Members launched in parallel when the stage starts.
    pub members: Vec<AgentSpec>,
    /// Shown to members; describes the structured input they receive
    /// from earlier stages.
    pub input_contract: String,
    /// Shown to members; describes the structured output they must
    /// produce for the handoff.
    pub output_contract: String,
    /// Review edge: index of an *earlier* stage to re-run when this
    /// stage's verification fails. A stage with `loop_back_to` is a
    /// review stage: its members are spawned via `spawn_reviewer` (which
    /// registers the `verdict` tool) and must call
    /// `verdict(pass, failed_items, notes)` exactly once at the end of
    /// the review. The fenced ```verdict block
    /// (`{"pass": bool, "failed_items": [...]}`) remains as a fallback
    /// for reviewers that cannot call tools.
    pub loop_back_to: Option<usize>,
}

/// A staged execution plan: ordered stages plus the lead.
#[derive(Debug, Clone)]
pub struct ExecutionPlan {
    pub team_name: String,
    pub topology: PlanTopology,
    pub stages: Vec<StageSpec>,
    /// Display name of the lead agent. The lead's coordination run is
    /// spawned first; its run id is the swarm's `lead_run_id`.
    pub lead_name: String,
    pub lead_profile: String,
    /// Review-loop bound: how many failed verifications re-run the loop
    /// before escalation. Must be >= 1.
    pub max_review_iterations: u32,
}

/// Latest output of one completed stage.
#[derive(Debug, Clone)]
struct StageOutput {
    stage_name: String,
    /// (agent name, transcript) per member, in spawn order.
    outputs: Vec<(String, String)>,
}

/// Mutable staged-execution state on a [`SwarmRecord`].
#[derive(Debug)]
struct StagedState {
    plan: ExecutionPlan,
    /// Index of the stage currently running (or most recently run).
    stage_index: usize,
    /// Failed verifications so far (across all review stages).
    review_iterations: u32,
    /// Latest output per completed stage index.
    stage_outputs: HashMap<usize, StageOutput>,
    /// Human-readable review/loop events for the execution log.
    review_log: Vec<String>,
    /// Set when the review bound is exhausted: the escalation note.
    escalation: Option<String>,
}

#[derive(Debug)]
struct SwarmRecord {
    id: String,
    task: String,
    judge_enabled: bool,
    /// The agent roster for every round: retry rebuilds the same specs so
    /// profiles-mode swarms keep their profiles across rounds.
    specs: Vec<AgentSpec>,
    round: u32,
    status: SwarmStatus,
    agents: Vec<AgentRecord>,
    verdict: Option<JudgeVerdict>,
    /// `Some` for staged swarms (`create_staged`).
    staged: Option<StagedState>,
}

/// One agent in a status view.
#[derive(Debug, Clone, Serialize)]
pub struct SwarmAgentView {
    pub name: String,
    pub profile: String,
    pub run_id: String,
    pub status: SwarmAgentStatus,
    pub round: u32,
    /// True for the lead coordination run of a staged swarm — the single
    /// user-facing run. Member runs are never user-facing.
    pub lead: bool,
    /// Last non-empty line of the agent's transcript, truncated — a live
    /// one-line pulse for the UI. `None` when there is nothing to show.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// One stage of a staged plan, as exposed to clients (stage UIs and
/// handoff rendering).
#[derive(Debug, Clone, Serialize)]
pub struct StageView {
    /// 0-based stage index.
    pub index: usize,
    /// Stage name.
    pub name: String,
    /// Display names of the stage's members, in spawn order.
    pub members: Vec<String>,
    /// 0-based index of the stage a review failure loops back to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loop_back_to: Option<usize>,
}

/// Staged-execution progress, surfaced in [`SwarmStatusView::staged`].
#[derive(Debug, Clone, Serialize)]
pub struct StagedView {
    pub team: String,
    pub topology: PlanTopology,
    pub stage_index: usize,
    pub stage_name: String,
    pub stage_count: usize,
    /// The plan's stages with their members.
    pub stages: Vec<StageView>,
    pub review_iterations: u32,
    pub max_review_iterations: u32,
    /// Set when the review bound was exhausted: who must take over.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub escalation: Option<String>,
}

/// `GET /api/swarm/status` body.
#[derive(Debug, Clone, Serialize)]
pub struct SwarmStatusView {
    pub id: String,
    pub task: String,
    pub status: SwarmStatus,
    pub round: u32,
    pub judge: bool,
    pub agents: Vec<SwarmAgentView>,
    pub verdict: Option<JudgeVerdictView>,
    /// Staged swarms only: the lead's run id — the single user-facing
    /// run. Clients open this run for the user; member runs are never
    /// user-facing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lead_run_id: Option<String>,
    /// Staged swarms only: plan progress.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staged: Option<StagedView>,
}

/// Serializable judge verdict.
#[derive(Debug, Clone, Serialize)]
pub struct JudgeVerdictView {
    pub done: bool,
    pub notes: String,
}

impl From<&JudgeVerdict> for JudgeVerdictView {
    fn from(v: &JudgeVerdict) -> Self {
        Self {
            done: v.done,
            notes: v.notes.clone(),
        }
    }
}

/// `POST /api/swarm` result: the id plus the first status view.
#[derive(Debug, Clone, Serialize)]
pub struct SwarmCreated {
    pub id: String,
    pub status: SwarmStatusView,
}

/// One agent's live transcript.
#[derive(Debug, Clone, Serialize)]
pub struct AgentTranscriptView {
    pub name: String,
    pub profile: String,
    pub status: SwarmAgentStatus,
    pub transcript: String,
}

/// Deterministic [`SwarmWorker`] for tests. Spawned runs start
/// `working`; the test settles them with [`ScriptedWorker::complete`]
/// / [`fail`](ScriptedWorker::fail). Every spawn (swarm id, agent
/// name, task) is recorded for assertions.
pub struct ScriptedWorker {
    runs: Mutex<HashMap<String, (SwarmAgentStatus, String)>>,
    spawns: Mutex<Vec<(String, String, String, String)>>,
    tool_calls: Mutex<HashMap<String, Vec<ToolCall>>>,
    next_run: AtomicU64,
}

impl ScriptedWorker {
    pub fn new() -> Self {
        Self {
            runs: Mutex::new(HashMap::new()),
            spawns: Mutex::new(vec![]),
            tool_calls: Mutex::new(HashMap::new()),
            next_run: AtomicU64::new(1),
        }
    }

    /// Mark a run done with `result` appended to its transcript.
    pub fn complete(&self, run_id: &str, result: &str) {
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((status, transcript)) = runs.get_mut(run_id) {
            *status = SwarmAgentStatus::Done;
            transcript.push_str(&format!("\nresult: {result}"));
        }
    }

    /// Mark a run failed with `error` appended to its transcript.
    pub fn fail(&self, run_id: &str, error: &str) {
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((status, transcript)) = runs.get_mut(run_id) {
            *status = SwarmAgentStatus::Failed;
            transcript.push_str(&format!("\nerror: {error}"));
        }
    }

    /// Every spawn so far, in order: (swarm_id, agent_name, profile, task).
    pub fn spawns(&self) -> Vec<(String, String, String, String)> {
        self.spawns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Script one tool call on a run, as if the agent's model issued it.
    /// Recorded in call order; the orchestrator reads these back through
    /// `agent_tool_calls`.
    pub fn tool_call(&self, run_id: &str, tool: &str, args: &str) {
        self.tool_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(run_id.to_string())
            .or_default()
            .push(ToolCall {
                tool: tool.to_string(),
                args: args.to_string(),
            });
    }
}

impl Default for ScriptedWorker {
    fn default() -> Self {
        Self::new()
    }
}

impl SwarmWorker for ScriptedWorker {
    fn spawn_agent(
        &self,
        swarm_id: &str,
        agent_name: &str,
        profile: &str,
        task: &str,
    ) -> Result<String, SwarmError> {
        let run_id = format!("r_{}", self.next_run.fetch_add(1, Ordering::SeqCst));
        self.spawns.lock().unwrap_or_else(|e| e.into_inner()).push((
            swarm_id.to_string(),
            agent_name.to_string(),
            profile.to_string(),
            task.to_string(),
        ));
        self.runs.lock().unwrap_or_else(|e| e.into_inner()).insert(
            run_id.clone(),
            (SwarmAgentStatus::Working, format!("task: {task}")),
        );
        Ok(run_id)
    }

    fn agent_status(&self, run_id: &str) -> Result<SwarmAgentStatus, SwarmError> {
        Ok(self
            .runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(run_id)
            .map(|(s, _)| *s)
            .unwrap_or(SwarmAgentStatus::Failed))
    }

    fn agent_transcript(&self, run_id: &str) -> Result<String, SwarmError> {
        Ok(self
            .runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(run_id)
            .map(|(_, t)| t.clone())
            .unwrap_or_default())
    }

    fn agent_tool_calls(&self, run_id: &str) -> Result<Vec<ToolCall>, SwarmError> {
        Ok(self
            .tool_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(run_id)
            .cloned()
            .unwrap_or_default())
    }
}

/// Runs swarms: fan-out, judge, retry. Thread-safe; the HTTP layer
/// holds one behind an `Arc`.
pub struct SwarmOrchestrator {
    swarms: Mutex<HashMap<String, SwarmRecord>>,
    worker: Arc<dyn SwarmWorker>,
    judge: Option<Arc<dyn JudgeTransport>>,
    next_swarm: AtomicU64,
}

impl SwarmOrchestrator {
    pub fn new(worker: Arc<dyn SwarmWorker>, judge: Option<Arc<dyn JudgeTransport>>) -> Self {
        Self {
            swarms: Mutex::new(HashMap::new()),
            worker,
            judge,
            next_swarm: AtomicU64::new(1),
        }
    }

    /// Create a swarm from explicit agent specs (name + profile per agent).
    /// `specs` must be 1..=8.
    pub fn create_with_specs(
        &self,
        task: &str,
        specs: Vec<AgentSpec>,
        judge: bool,
    ) -> Result<SwarmCreated, SwarmError> {
        if task.trim().is_empty() {
            return Err(SwarmError::bad_request(
                "SWARM_VALIDATION",
                "task must not be empty",
            ));
        }
        if specs.is_empty() || specs.len() > 8 {
            return Err(SwarmError::bad_request(
                "SWARM_VALIDATION",
                format!("agents must be 1..=8, got {}", specs.len()),
            ));
        }
        // P1 #8: `judge: true` without a configured `[judge]` transport
        // degrades to judge-free execution instead of failing. Rationale:
        // the TUI's `/team <id> <task>` always sends `judge: true` and
        // cannot know the server's config, and the dashboard documents
        // "the caller then runs swarms judge-free" — failing closed here
        // makes team launch unusable out of the box. The degrade is
        // explicit: the record keeps `judge_enabled: false`, which the
        // status view reports back to the caller, and the absence is
        // logged.
        let judge = if judge && self.judge.is_none() {
            log_warn!(
                "swarm requested a judge but no [judge] transport is configured; continuing judge-free"
            );
            false
        } else {
            judge
        };
        let id = format!("sw_{}", self.next_swarm.fetch_add(1, Ordering::SeqCst));
        let mut record = SwarmRecord {
            id: id.clone(),
            task: task.to_string(),
            judge_enabled: judge,
            specs: specs.clone(),
            round: 1,
            status: SwarmStatus::Running,
            agents: Vec::new(),
            verdict: None,
            staged: None,
        };
        for spec in &specs {
            record.agents.push(AgentRecord {
                name: spec.name.clone(),
                profile: spec.profile.clone(),
                run_id: String::new(),
                status: SwarmAgentStatus::Waiting,
                round: 1,
                stage: None,
                lead: false,
            });
        }
        self.launch_waiting(&mut record)?;
        let view = self.view(&record);
        self.swarms
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), record);
        Ok(SwarmCreated { id, status: view })
    }

    /// Create a swarm: validate, register N agents (`waiting`), spawn
    /// each (`waiting` → `working`). `agents` must be 1..=8. Count mode:
    /// agents are `subagent-{i}` on the `"default"` profile.
    pub fn create(&self, task: &str, agents: u32, judge: bool) -> Result<SwarmCreated, SwarmError> {
        let specs = (1..=agents)
            .map(|i| AgentSpec {
                name: format!("subagent-{i}"),
                profile: "default".to_string(),
            })
            .collect();
        self.create_with_specs(task, specs, judge)
    }

    /// Create a staged swarm from an [`ExecutionPlan`]: validate, spawn
    /// the lead coordination run (the user-facing run), then spawn stage
    /// 1's members. Later stages spawn from [`Self::refresh_locked`] as
    /// earlier stages settle — advancement is driven by `status()` polls,
    /// the same client-polled lifecycle as classic swarms; there is no
    /// background driver.
    pub fn create_staged(
        &self,
        task: &str,
        plan: ExecutionPlan,
        judge: bool,
    ) -> Result<SwarmCreated, SwarmError> {
        if task.trim().is_empty() {
            return Err(SwarmError::bad_request(
                "SWARM_VALIDATION",
                "task must not be empty",
            ));
        }
        if plan.stages.is_empty() {
            return Err(SwarmError::bad_request(
                "SWARM_VALIDATION",
                "plan must define at least one stage",
            ));
        }
        if plan.lead_name.trim().is_empty() {
            return Err(SwarmError::bad_request(
                "SWARM_VALIDATION",
                "plan must name a lead",
            ));
        }
        if plan.max_review_iterations == 0 {
            return Err(SwarmError::bad_request(
                "SWARM_VALIDATION",
                "max_review_iterations must be >= 1",
            ));
        }
        for (i, s) in plan.stages.iter().enumerate() {
            if s.name.trim().is_empty() {
                return Err(SwarmError::bad_request(
                    "SWARM_VALIDATION",
                    format!("stage {} name must not be empty", i + 1),
                ));
            }
            if s.members.is_empty() || s.members.len() > 8 {
                return Err(SwarmError::bad_request(
                    "SWARM_VALIDATION",
                    format!(
                        "stage {:?} must have 1..=8 members, got {}",
                        s.name,
                        s.members.len()
                    ),
                ));
            }
            if let Some(t) = s.loop_back_to {
                if t >= i {
                    return Err(SwarmError::bad_request(
                        "SWARM_VALIDATION",
                        format!(
                            "stage {:?} loops back to stage {}, which is not earlier",
                            s.name,
                            t + 1
                        ),
                    ));
                }
            }
        }
        // P1 #8: `judge: true` without a configured `[judge]` transport
        // degrades to judge-free execution instead of failing (same
        // rationale as `create_with_specs` above): team launch must work
        // out of the box, and the degrade is explicit in the status view.
        let judge = if judge && self.judge.is_none() {
            log_warn!(
                "staged swarm requested a judge but no [judge] transport is configured; continuing judge-free"
            );
            false
        } else {
            judge
        };
        let id = format!("sw_{}", self.next_swarm.fetch_add(1, Ordering::SeqCst));
        let lead_name = plan.lead_name.clone();
        let lead_profile = plan.lead_profile.clone();
        let lead_task = build_lead_task(task, &plan);
        let mut record = SwarmRecord {
            id: id.clone(),
            task: task.to_string(),
            judge_enabled: judge,
            specs: plan.stages.iter().flat_map(|s| s.members.clone()).collect(),
            round: 1,
            status: SwarmStatus::Running,
            agents: Vec::new(),
            verdict: None,
            staged: Some(StagedState {
                plan,
                stage_index: 0,
                review_iterations: 0,
                stage_outputs: HashMap::new(),
                review_log: Vec::new(),
                escalation: None,
            }),
        };
        let lead_run_id = self
            .worker
            .spawn_agent(&id, &lead_name, &lead_profile, &lead_task)?;
        record.agents.push(AgentRecord {
            name: lead_name,
            profile: lead_profile,
            run_id: lead_run_id,
            status: SwarmAgentStatus::Working,
            round: 1,
            stage: None,
            lead: true,
        });
        self.spawn_stage_agents(&mut record, 0, None)?;
        let view = self.view(&record);
        self.swarms
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), record);
        Ok(SwarmCreated { id, status: view })
    }

    /// Spawn one stage's members. `feedback` is `Some` on review-loop
    /// re-runs, and lands in the members' tasks.
    fn spawn_stage_agents(
        &self,
        record: &mut SwarmRecord,
        stage_idx: usize,
        feedback: Option<ReviewFeedback>,
    ) -> Result<(), SwarmError> {
        // P1 #11: one live record per (stage, member). Re-spawning a
        // stage supersedes its previous records: without this, a retried
        // stage's stale records (rejected-iteration transcripts, crashed
        // `Failed` statuses) pollute `advance_staged`'s output
        // collection, its fail-closed check, settle detection in
        // `refresh_staged_locked`, and the end-of-plan judge summary —
        // all of which filter records by stage index. Review history
        // itself survives textually in `staged.review_log`; the records
        // are always the current iteration's.
        record
            .agents
            .retain(|a| a.lead || a.stage != Some(stage_idx as u32));
        // Snapshot everything the task text needs (borrow-friendly).
        let snap = {
            let st = record.staged.as_ref().expect("staged swarm");
            let plan = &st.plan;
            let sp = &plan.stages[stage_idx];
            let mut inputs = String::from("None — you start from the task above.");
            if stage_idx > 0 {
                let mut parts = Vec::new();
                for j in 0..stage_idx {
                    if let Some(out) = st.stage_outputs.get(&j) {
                        let contract = &plan.stages[j].output_contract;
                        let mut p = format!(
                            "### From stage \"{}\" — output contract: {}\n",
                            out.stage_name, contract
                        );
                        for (agent, transcript) in &out.outputs {
                            p.push_str(&format!("**{agent}:**\n{transcript}\n\n"));
                        }
                        parts.push(p);
                    }
                }
                if !parts.is_empty() {
                    inputs = parts.join("\n");
                }
            }
            StageSpawnSnapshot {
                team_name: plan.team_name.clone(),
                topology: plan.topology,
                lead_name: plan.lead_name.clone(),
                stage_name: sp.name.clone(),
                stage_count: plan.stages.len(),
                members: sp.members.clone(),
                input_contract: sp.input_contract.clone(),
                output_contract: sp.output_contract.clone(),
                is_review: sp.loop_back_to.is_some(),
                inputs,
            }
        };
        let base_task = record.task.clone();
        for spec in &snap.members {
            let task = build_member_task(MemberTaskCtx {
                base_task: &base_task,
                team_name: &snap.team_name,
                topology: snap.topology,
                lead_name: &snap.lead_name,
                stage_idx,
                stage_count: snap.stage_count,
                stage_name: &snap.stage_name,
                member_name: &spec.name,
                input_contract: &snap.input_contract,
                output_contract: &snap.output_contract,
                is_review: snap.is_review,
                inputs_block: &snap.inputs,
                feedback: feedback.as_ref(),
            });
            let run_id = if snap.is_review {
                // Review stage: reviewers get the `verdict` tool registered
                // (the worker's `spawn_reviewer` override handles the real
                // model runs); their verdict is read from the tool call.
                self.worker
                    .spawn_reviewer(&record.id, &spec.name, &spec.profile, &task)?
            } else {
                self.worker
                    .spawn_agent(&record.id, &spec.name, &spec.profile, &task)?
            };
            record.agents.push(AgentRecord {
                name: spec.name.clone(),
                profile: spec.profile.clone(),
                run_id,
                status: SwarmAgentStatus::Working,
                round: record.round,
                stage: Some(stage_idx as u32),
                lead: false,
            });
        }
        Ok(())
    }

    /// Move execution to stage `next`: spawn it, or finish the swarm when
    /// the plan is exhausted (judge over everything when enabled).
    fn goto_stage(&self, record: &mut SwarmRecord, next: usize) -> Result<(), SwarmError> {
        let stage_count = record
            .staged
            .as_ref()
            .expect("staged swarm")
            .plan
            .stages
            .len();
        if next >= stage_count {
            if record.judge_enabled {
                let summary = self.combined_transcripts_staged(record);
                let transport = self
                    .judge
                    .as_ref()
                    .expect("judge enabled implies transport");
                let verdict = judge_completion_with(transport.as_ref(), &summary, &record.task);
                record.status = if verdict.done {
                    SwarmStatus::Complete
                } else {
                    SwarmStatus::Incomplete
                };
                record.verdict = Some(verdict);
            } else {
                record.status = SwarmStatus::Complete;
            }
            return Ok(());
        }
        record.staged.as_mut().expect("staged swarm").stage_index = next;
        self.spawn_stage_agents(record, next, None)
    }

    /// Advance a staged swarm whose current stage just settled: collect
    /// outputs, run review logic for review stages, spawn the next stage.
    fn advance_staged(&self, record: &mut SwarmRecord) -> Result<(), SwarmError> {
        let stage_idx = record.staged.as_ref().expect("staged swarm").stage_index;
        // Any failed member fails the stage: fail closed.
        let failed: Vec<String> = record
            .agents
            .iter()
            .filter(|a| a.stage == Some(stage_idx as u32) && a.status == SwarmAgentStatus::Failed)
            .map(|a| a.name.clone())
            .collect();
        if !failed.is_empty() {
            let name = record.staged.as_ref().expect("staged swarm").plan.stages[stage_idx]
                .name
                .clone();
            let msg = format!(
                "stage {} {name:?} failed: member(s) {} failed; swarm marked incomplete — \
                 escalation note recorded on the swarm record (the lead is not notified: \
                 the orchestrator cannot message a running lead run)",
                stage_idx + 1,
                failed.join(", ")
            );
            record.staged.as_mut().expect("staged swarm").escalation = Some(msg.clone());
            record.status = SwarmStatus::Incomplete;
            record.verdict = Some(JudgeVerdict {
                done: false,
                notes: msg,
            });
            return Ok(());
        }
        // Collect this stage's outputs (member transcripts).
        let outputs: Vec<(String, String)> = record
            .agents
            .iter()
            .filter(|a| a.stage == Some(stage_idx as u32))
            .map(|a| {
                (
                    a.name.clone(),
                    self.worker.agent_transcript(&a.run_id).unwrap_or_default(),
                )
            })
            .collect();
        let (stage_name, loop_back_to) = {
            let st = record.staged.as_ref().expect("staged swarm");
            let sp = &st.plan.stages[stage_idx];
            (sp.name.clone(), sp.loop_back_to)
        };
        record
            .staged
            .as_mut()
            .expect("staged swarm")
            .stage_outputs
            .insert(
                stage_idx,
                StageOutput {
                    stage_name: stage_name.clone(),
                    outputs: outputs.clone(),
                },
            );
        if let Some(target) = loop_back_to {
            // Review stage: extract the verdict — structured tool calls
            // first, fenced block and keyword scan as fallbacks.
            let (pass, failed_items) = self.extract_review_verdict(record, stage_idx, &outputs);
            if pass {
                record
                    .staged
                    .as_mut()
                    .expect("staged swarm")
                    .review_log
                    .push(format!(
                        "stage {} {stage_name:?}: verification passed",
                        stage_idx + 1
                    ));
                self.goto_stage(record, stage_idx + 1)?;
            } else {
                let (iterations, max) = {
                    let st = record.staged.as_ref().expect("staged swarm");
                    (st.review_iterations, st.plan.max_review_iterations)
                };
                if iterations < max {
                    let target_name = {
                        let st = record.staged.as_ref().expect("staged swarm");
                        st.plan.stages[target].name.clone()
                    };
                    {
                        let st = record.staged.as_mut().expect("staged swarm");
                        st.review_iterations += 1;
                        st.review_log.push(format!(
                            "stage {} {stage_name:?}: verification failed (iteration {}/{max}) → re-running from stage {} {target_name:?}: {}",
                            stage_idx + 1,
                            iterations + 1,
                            target + 1,
                            failed_items.join("; ")
                        ));
                        // Drop stale outputs at/after the target: they
                        // consumed the rejected output.
                        st.stage_outputs.retain(|&k, _| k < target);
                        st.stage_index = target;
                    }
                    self.spawn_stage_agents(
                        record,
                        target,
                        Some(ReviewFeedback {
                            reviewer_stage: stage_name,
                            iteration: iterations + 1,
                            max,
                            failed_items,
                        }),
                    )?;
                } else {
                    let msg = format!(
                        "stage {} {stage_name:?}: verification failed {max} times (bound reached); \
                         swarm marked incomplete — failed items recorded in the escalation note \
                         on the swarm record (the lead is not notified: the orchestrator cannot \
                         message a running lead run): {}",
                        stage_idx + 1,
                        failed_items.join("; ")
                    );
                    let st = record.staged.as_mut().expect("staged swarm");
                    st.review_log.push(msg.clone());
                    st.escalation = Some(msg.clone());
                    record.status = SwarmStatus::Incomplete;
                    record.verdict = Some(JudgeVerdict {
                        done: false,
                        notes: msg,
                    });
                }
            }
        } else {
            self.goto_stage(record, stage_idx + 1)?;
        }
        Ok(())
    }

    /// Staged refresh: poll the current stage's agents; when settled,
    /// advance the plan.
    /// Extract a review stage's verdict. Three sources, in order:
    ///
    /// 1. Structured tool calls: every `verdict` tool call on a review
    ///    stage agent's run, in call order (last call wins). This is the
    ///    primary channel — reviewer runs are spawned via
    ///    [`SwarmWorker::spawn_reviewer`] with the tool registered.
    /// 2. Fenced ```verdict JSON blocks in the transcripts (fallback for
    ///    reviewers that cannot call tools).
    /// 3. A keyword scan for verdict pass/fail language (last resort).
    ///
    /// No verdict from any source counts as failed verification.
    fn extract_review_verdict(
        &self,
        record: &SwarmRecord,
        stage_idx: usize,
        outputs: &[(String, String)],
    ) -> (bool, Vec<String>) {
        let mut verdict: Option<(bool, Vec<String>)> = None;
        for agent in record
            .agents
            .iter()
            .filter(|a| a.stage == Some(stage_idx as u32))
        {
            let calls = self
                .worker
                .agent_tool_calls(&agent.run_id)
                .unwrap_or_default();
            for call in calls {
                if call.tool == VERDICT_TOOL_NAME {
                    if let Some(v) = parse_verdict_args(&call.args) {
                        verdict = Some(v);
                    }
                }
            }
        }
        if verdict.is_none() {
            for (_, transcript) in outputs {
                if let Some(v) = parse_verdict(transcript) {
                    verdict = Some(v);
                }
            }
        }
        if verdict.is_none() {
            for (_, transcript) in outputs {
                if let Some(v) = scan_verdict_keywords(transcript) {
                    verdict = Some(v);
                }
            }
        }
        verdict.unwrap_or((false, vec!["reviewer emitted no verdict".into()]))
    }

    fn refresh_staged_locked(&self, record: &mut SwarmRecord) -> Result<(), SwarmError> {
        let stage_idx = record.staged.as_ref().expect("staged swarm").stage_index;
        for agent in record
            .agents
            .iter_mut()
            .filter(|a| a.stage == Some(stage_idx as u32) && a.status == SwarmAgentStatus::Working)
        {
            // A worker that cannot report a status is treated as dead.
            let s = self
                .worker
                .agent_status(&agent.run_id)
                .unwrap_or(SwarmAgentStatus::Failed);
            agent.status = s;
        }
        let settled = record
            .agents
            .iter()
            .filter(|a| a.stage == Some(stage_idx as u32))
            .all(|a| matches!(a.status, SwarmAgentStatus::Done | SwarmAgentStatus::Failed));
        if settled && record.status == SwarmStatus::Running {
            self.advance_staged(record)?;
        }
        Ok(())
    }
    /// Spawn every `waiting` agent in the record (`waiting` → `working`).
    fn launch_waiting(&self, record: &mut SwarmRecord) -> Result<(), SwarmError> {
        for agent in record
            .agents
            .iter_mut()
            .filter(|a| a.status == SwarmAgentStatus::Waiting)
        {
            let run_id =
                self.worker
                    .spawn_agent(&record.id, &agent.name, &agent.profile, &record.task)?;
            agent.run_id = run_id;
            agent.status = SwarmAgentStatus::Working;
        }
        Ok(())
    }

    /// Refresh one record from the worker; run the judge when the round
    /// settles. Returns false when the swarm does not exist. Staged
    /// swarms take the staged path: stage gating replaces the
    /// single-round settle logic.
    fn refresh_locked(&self, record: &mut SwarmRecord) -> Result<(), SwarmError> {
        if record.staged.is_some() {
            return self.refresh_staged_locked(record);
        }
        let round = record.round;
        let mut changed = false;
        for agent in record
            .agents
            .iter_mut()
            .filter(|a| a.round == round && a.status == SwarmAgentStatus::Working)
        {
            // A worker that cannot report a status is treated as dead:
            // the alternative is polling forever.
            let s = self
                .worker
                .agent_status(&agent.run_id)
                .unwrap_or(SwarmAgentStatus::Failed);
            if s != agent.status {
                agent.status = s;
                changed = true;
            }
        }
        let _ = changed;
        let settled = record
            .agents
            .iter()
            .filter(|a| a.round == round)
            .all(|a| matches!(a.status, SwarmAgentStatus::Done | SwarmAgentStatus::Failed));
        if record.status == SwarmStatus::Running && settled {
            record.status = SwarmStatus::Judging;
            if record.judge_enabled {
                let summary = self.combined_transcripts(record, round);
                let transport = self
                    .judge
                    .as_ref()
                    .expect("judge enabled implies transport");
                let verdict = judge_completion_with(transport.as_ref(), &summary, &record.task);
                record.status = if verdict.done {
                    SwarmStatus::Complete
                } else {
                    SwarmStatus::Incomplete
                };
                record.verdict = Some(verdict);
            } else {
                let all_done = record
                    .agents
                    .iter()
                    .filter(|a| a.round == round)
                    .all(|a| a.status == SwarmAgentStatus::Done);
                record.status = if all_done {
                    SwarmStatus::Complete
                } else {
                    SwarmStatus::Incomplete
                };
            }
        }
        Ok(())
    }

    fn combined_transcripts(&self, record: &SwarmRecord, round: u32) -> String {
        if record.staged.is_some() {
            return self.combined_transcripts_staged(record);
        }
        record
            .agents
            .iter()
            .filter(|a| a.round == round)
            .map(|a| {
                format!(
                    "=== {} ({}) [{}] ===\n{}",
                    a.name,
                    a.run_id,
                    a.status.as_str(),
                    self.worker.agent_transcript(&a.run_id).unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    /// Combined transcripts of every agent in a staged swarm (all
    /// stages plus the lead), each headed by name, run id, status, and
    /// role. Used for the end-of-plan judge.
    fn combined_transcripts_staged(&self, record: &SwarmRecord) -> String {
        record
            .agents
            .iter()
            .map(|a| {
                let role = if a.lead {
                    "lead".to_string()
                } else {
                    format!("stage {}", a.stage.map(|s| s + 1).unwrap_or(0))
                };
                format!(
                    "=== {} ({}) [{}] [{}] ===\n{}",
                    a.name,
                    a.run_id,
                    a.status.as_str(),
                    role,
                    self.worker.agent_transcript(&a.run_id).unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    fn view(&self, record: &SwarmRecord) -> SwarmStatusView {
        let (agents, lead_run_id, staged) = match record.staged.as_ref() {
            Some(st) => {
                // The lead coordination run plus the current stage's
                // agents, lead first. Member runs are never user-facing:
                // `lead_run_id` names the single run clients open.
                let mut agents: Vec<SwarmAgentView> = record
                    .agents
                    .iter()
                    .filter(|a| a.lead || a.stage == Some(st.stage_index as u32))
                    .map(|a| self.agent_view(a))
                    .collect();
                agents.sort_by_key(|a| !a.lead);
                let lead_run_id = record
                    .agents
                    .iter()
                    .find(|a| a.lead)
                    .map(|a| a.run_id.clone());
                let staged = StagedView {
                    team: st.plan.team_name.clone(),
                    topology: st.plan.topology,
                    stage_index: st.stage_index,
                    stage_name: st.plan.stages[st.stage_index].name.clone(),
                    stage_count: st.plan.stages.len(),
                    stages: st
                        .plan
                        .stages
                        .iter()
                        .enumerate()
                        .map(|(i, s)| StageView {
                            index: i,
                            name: s.name.clone(),
                            members: s.members.iter().map(|m| m.name.clone()).collect(),
                            loop_back_to: s.loop_back_to,
                        })
                        .collect(),
                    review_iterations: st.review_iterations,
                    max_review_iterations: st.plan.max_review_iterations,
                    escalation: st.escalation.clone(),
                };
                (agents, lead_run_id, Some(staged))
            }
            None => (
                record
                    .agents
                    .iter()
                    .filter(|a| a.round == record.round)
                    .map(|a| self.agent_view(a))
                    .collect(),
                None,
                None,
            ),
        };
        SwarmStatusView {
            id: record.id.clone(),
            task: record.task.clone(),
            status: record.status,
            round: record.round,
            judge: record.judge_enabled,
            agents,
            verdict: record.verdict.as_ref().map(JudgeVerdictView::from),
            lead_run_id,
            staged,
        }
    }

    fn agent_view(&self, a: &AgentRecord) -> SwarmAgentView {
        SwarmAgentView {
            name: a.name.clone(),
            profile: a.profile.clone(),
            run_id: a.run_id.clone(),
            status: a.status,
            round: a.round,
            lead: a.lead,
            summary: self.agent_summary(&a.run_id),
        }
    }

    /// One-line live pulse for an agent: the last non-empty line of its
    /// transcript, truncated. `None` when there is nothing to show yet.
    fn agent_summary(&self, run_id: &str) -> Option<String> {
        if run_id.is_empty() {
            return None;
        }
        let text = self.worker.agent_transcript(run_id).ok()?;
        let line = text.lines().rev().find(|l| !l.trim().is_empty())?;
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        const MAX: usize = 200;
        if line.len() > MAX {
            Some(format!("{}…", &line[..MAX]))
        } else {
            Some(line.to_string())
        }
    }

    /// Current status, refreshing agent states from the worker first.
    pub fn status(&self, swarm_id: &str) -> Result<SwarmStatusView, SwarmError> {
        let mut swarms = self.swarms.lock().unwrap_or_else(|e| e.into_inner());
        let record = swarms
            .get_mut(swarm_id)
            .ok_or_else(|| SwarmError::not_found(swarm_id))?;
        self.refresh_locked(record)?;
        Ok(self.view(record))
    }

    /// Combined transcripts of every agent across all rounds, each
    /// headed by its name, run id, round, and status. Judge notes are
    /// appended when a verdict exists.
    pub fn transcript(&self, swarm_id: &str) -> Result<String, SwarmError> {
        let mut swarms = self.swarms.lock().unwrap_or_else(|e| e.into_inner());
        let record = swarms
            .get_mut(swarm_id)
            .ok_or_else(|| SwarmError::not_found(swarm_id))?;
        self.refresh_locked(record)?;
        let mut out: Vec<String> = record
            .agents
            .iter()
            .map(|a| {
                format!(
                    "=== {} ({}) round {} [{}] ===\n{}",
                    a.name,
                    a.run_id,
                    a.round,
                    a.status.as_str(),
                    self.worker.agent_transcript(&a.run_id).unwrap_or_default()
                )
            })
            .collect();
        if let Some(v) = &record.verdict {
            out.push(format!(
                "=== judge verdict: {} ===\n{}",
                if v.done { "done" } else { "not done" },
                v.notes
            ));
        }
        if let Some(st) = &record.staged {
            out.push(format!(
                "=== staged execution log: {} ({}) ===",
                st.plan.team_name,
                st.plan.topology.describe()
            ));
            let mut idx: Vec<usize> = st.stage_outputs.keys().copied().collect();
            idx.sort_unstable();
            for i in idx {
                let o = &st.stage_outputs[&i];
                let agents: Vec<&str> = o.outputs.iter().map(|(n, _)| n.as_str()).collect();
                out.push(format!(
                    "stage {} {:?}: settled (agents: {})",
                    i + 1,
                    o.stage_name,
                    agents.join(", ")
                ));
            }
            for line in &st.review_log {
                out.push(format!("review: {line}"));
            }
            if let Some(e) = &st.escalation {
                out.push(format!("escalation: {e}"));
            }
        }
        Ok(out.join("\n\n"))
    }

    /// One agent's live transcript plus its current status.
    pub fn agent_transcript(
        &self,
        swarm_id: &str,
        agent_name: &str,
    ) -> Result<AgentTranscriptView, SwarmError> {
        let mut swarms = self.swarms.lock().unwrap_or_else(|e| e.into_inner());
        let record = swarms
            .get_mut(swarm_id)
            .ok_or_else(|| SwarmError::not_found(swarm_id))?;
        self.refresh_locked(record)?;
        let agent = record
            .agents
            .iter()
            .filter(|a| a.round == record.round)
            .find(|a| a.name == agent_name)
            .ok_or_else(|| SwarmError::not_found(&format!("agent {agent_name}")))?;
        let transcript = self
            .worker
            .agent_transcript(&agent.run_id)
            .unwrap_or_default();
        Ok(AgentTranscriptView {
            name: agent.name.clone(),
            profile: agent.profile.clone(),
            status: agent.status,
            transcript,
        })
    }

    /// Retry an `incomplete` swarm: round < 3, a verdict exists, and the
    /// verdict is not done. Registers a fresh round of agents as
    /// `waiting` with the judge's feedback appended to their task, then
    /// spawns them (`waiting` → `working`). Staged swarms are refused:
    /// their review loop is the re-execution mechanism.
    pub fn retry(&self, swarm_id: &str) -> Result<SwarmStatusView, SwarmError> {
        let mut swarms = self.swarms.lock().unwrap_or_else(|e| e.into_inner());
        let record = swarms
            .get_mut(swarm_id)
            .ok_or_else(|| SwarmError::not_found(swarm_id))?;
        if record.staged.is_some() {
            return Err(SwarmError::bad_request(
                "SWARM_RETRY_REFUSED",
                "staged swarms re-execute through their review loop; retry is not supported",
            ));
        }
        self.refresh_locked(record)?;
        if record.status != SwarmStatus::Incomplete {
            return Err(SwarmError::bad_request(
                "SWARM_RETRY_REFUSED",
                format!(
                    "swarm is {}, not incomplete: only incomplete swarms can be retried",
                    record.status.as_str()
                ),
            ));
        }
        if record.round >= 3 {
            return Err(SwarmError::bad_request(
                "SWARM_RETRY_REFUSED",
                format!("max rounds (3) reached for {}", record.id),
            ));
        }
        let verdict = record.verdict.as_ref().ok_or_else(|| {
            SwarmError::bad_request(
                "SWARM_RETRY_REFUSED",
                "no judge verdict to retry from (judge was disabled)",
            )
        })?;
        if verdict.done {
            return Err(SwarmError::bad_request(
                "SWARM_RETRY_REFUSED",
                "verdict is done: nothing to retry",
            ));
        }
        let feedback = verdict.notes.clone();
        record.round += 1;
        let round = record.round;
        let base_task = record.task.clone();
        // Rebuild the same roster (profiles mode keeps its profiles).
        let specs = record.specs.clone();
        for spec in &specs {
            record.agents.push(AgentRecord {
                name: spec.name.clone(),
                profile: spec.profile.clone(),
                run_id: String::new(),
                status: SwarmAgentStatus::Waiting,
                round,
                stage: None,
                lead: false,
            });
        }
        // The retry round's task carries the judge's feedback so agents
        // address what was missing instead of repeating round 1.
        let retry_task = format!(
            "{base_task}\n\nJudge feedback from round {} (address every point):\n{feedback}",
            round - 1
        );
        let saved_task = std::mem::replace(&mut record.task, retry_task);
        let launch = self.launch_waiting(record);
        // Restore the original task: the feedback is per-round, baked
        // into what the worker received at spawn.
        record.task = saved_task;
        launch?;
        record.status = SwarmStatus::Running;
        record.verdict = None;
        Ok(self.view(record))
    }
}

/// Review feedback carried into a looped-back stage's task.
#[derive(Debug, Clone)]
struct ReviewFeedback {
    reviewer_stage: String,
    iteration: u32,
    max: u32,
    failed_items: Vec<String>,
}

/// Borrow-friendly snapshot of one stage spawn.
struct StageSpawnSnapshot {
    team_name: String,
    topology: PlanTopology,
    lead_name: String,
    stage_name: String,
    stage_count: usize,
    members: Vec<AgentSpec>,
    input_contract: String,
    output_contract: String,
    is_review: bool,
    inputs: String,
}

/// Context for [`build_member_task`].
struct MemberTaskCtx<'a> {
    base_task: &'a str,
    team_name: &'a str,
    topology: PlanTopology,
    lead_name: &'a str,
    stage_idx: usize,
    stage_count: usize,
    stage_name: &'a str,
    member_name: &'a str,
    input_contract: &'a str,
    output_contract: &'a str,
    is_review: bool,
    inputs_block: &'a str,
    feedback: Option<&'a ReviewFeedback>,
}

/// The lead's coordination brief: the task, the plan, and the one rule
/// that matters (only the lead talks to the user).
fn build_lead_task(base_task: &str, plan: &ExecutionPlan) -> String {
    let mut out = String::new();
    out.push_str(base_task);
    out.push_str(&format!(
        "\n\n## Your role — {}, lead of the {} crew\n\
         Only you talk to the user. The crew members below never address the user directly.\n\
         \n\
         ## Execution plan\n\
         Topology: {}.\n",
        plan.lead_name,
        plan.team_name,
        plan.topology.describe()
    ));
    for (i, s) in plan.stages.iter().enumerate() {
        let members: Vec<&str> = s.members.iter().map(|m| m.name.as_str()).collect();
        out.push_str(&format!(
            "### Stage {}: {} — {}\n",
            i + 1,
            s.name,
            members.join(", ")
        ));
    }
    out.push_str(
        "\nAcknowledge the plan briefly. When the user asks about progress, \
         summarize the plan and what each stage is producing.",
    );
    out
}

/// A stage member's task: plan context, handoff inputs from earlier
/// stages, its contracts, and review feedback on loop-back re-runs.
/// Members are told to address the lead, never the user.
fn build_member_task(c: MemberTaskCtx<'_>) -> String {
    let mut out = String::new();
    out.push_str(c.base_task);
    out.push_str(&format!(
        "\n\n## Execution plan — {}: stage {}/{} \"{}\"\n\
         {}.\n\
         Lead: {} — only the lead talks to the user. You are {}, a crew member: \
         address your output to the lead, never directly to the user.\n",
        c.team_name,
        c.stage_idx + 1,
        c.stage_count,
        c.stage_name,
        c.topology.describe(),
        c.lead_name,
        c.member_name
    ));
    if let Some(fb) = c.feedback {
        out.push_str(&format!(
            "\n## Review feedback — the \"{}\" stage rejected the previous attempt \
             (iteration {} of {}):\n",
            fb.reviewer_stage, fb.iteration, fb.max
        ));
        for item in &fb.failed_items {
            out.push_str(&format!("- {item}\n"));
        }
        out.push_str("Address every failed item, then produce a corrected output.\n");
    }
    out.push_str("\n## Inputs from earlier stages\n");
    out.push_str(c.inputs_block);
    out.push_str(&format!(
        "\n## Your assignment\n\
         Input contract: {}\n\
         Produce your output per this output contract: {}\n",
        c.input_contract, c.output_contract
    ));
    if c.is_review {
        // The reviewer prompt is deliberately written so the verdict
        // extraction below cannot mistake the instructions for a
        // verdict: the fenced example uses <...> placeholders (not
        // parseable JSON), and no pass/fail word sits within the
        // keyword scan's window after any "verdict" mention. A
        // reviewer that emits nothing must read as "no verdict",
        // never as a pass or a fail.
        out.push_str(
            "\nYou are the reviewer. End your review by calling the `verdict` tool exactly once. \
             Give it your decision as a boolean (true when the stage output satisfies the \
             contracts, false otherwise), every failed item as a list, and notes for the lead. \
             Do not finish without calling it. If the tool is unavailable, end your response \
             with a fenced block marked verdict holding JSON with your boolean decision and \
             the list of failed items, like so:\n\
             ```verdict\n\
             {\"pass\": <true|false>, \"failed_items\": [\"<failed item>\"]}\n\
             ```",
        );
    }
    out
}

/// Parse a review verdict from a reviewer's transcript: the last fenced
/// ```verdict block holding JSON `{"pass": bool, "failed_items": [...]}`.
/// Returns `None` when no verdict block is present.
fn parse_verdict(transcript: &str) -> Option<(bool, Vec<String>)> {
    let mut last: Option<&str> = None;
    let mut search = transcript;
    while let Some(i) = search.find("```verdict") {
        let start = i + "```verdict".len();
        let rest = &search[start..];
        if let Some(end) = rest.find("```") {
            last = Some(rest[..end].trim());
        }
        search = &search[start..];
    }
    let v: serde_json::Value = serde_json::from_str(last?).ok()?;
    let pass = v.get("pass")?.as_bool()?;
    let failed_items = v
        .get("failed_items")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Some((pass, failed_items))
}

/// Parse a verdict from a `verdict` tool call's structured args:
/// `{"pass": bool, "failed_items": [...]}`. Returns `None` on
/// malformed args so the fallbacks can still try the transcript.
fn parse_verdict_args(args: &str) -> Option<(bool, Vec<String>)> {
    let v: serde_json::Value = serde_json::from_str(args).ok()?;
    let pass = v.get("pass")?.as_bool()?;
    let failed_items = v
        .get("failed_items")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Some((pass, failed_items))
}

/// Last-resort verdict scan: a case-insensitive `verdict` mention
/// followed by pass/fail language in the next few tokens. Fail words
/// take precedence over pass words (fail-closed), and matching is on
/// whole tokens so "bypass" never reads as "pass".
///
/// Fenced code blocks are stripped before scanning: transcripts include
/// the reviewer's task text, which carries a fenced `verdict` template
/// as an example, and real fenced verdicts are already handled by
/// [`parse_verdict`]. Only prose may vote here.
fn scan_verdict_keywords(transcript: &str) -> Option<(bool, Vec<String>)> {
    let lower = strip_fenced_blocks(transcript).to_lowercase();
    let mut search = lower.as_str();
    while let Some(i) = search.find("verdict") {
        let end = (i + "verdict".len() + 32).min(search.len());
        let tokens: Vec<&str> = search[i..end]
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|t| !t.is_empty())
            .collect();
        // tokens[0] is "verdict" itself; inspect what follows it.
        let rest = &tokens[1..];
        let failed = rest.iter().any(|t| {
            matches!(
                *t,
                "fail" | "failed" | "failure" | "false" | "reject" | "rejected"
            )
        });
        if failed {
            return Some((false, Vec::new()));
        }
        let passed = rest.iter().any(|t| {
            matches!(
                *t,
                "pass" | "passed" | "true" | "approve" | "approved" | "ok" | "okay"
            )
        });
        if passed {
            return Some((true, Vec::new()));
        }
        search = &search[i + "verdict".len()..];
    }
    None
}

/// Remove fenced ``` code blocks (fences included) from text. Used by
/// [`scan_verdict_keywords`] so template blocks in the task text never
/// read as a verdict.
fn strip_fenced_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("```") {
        out.push_str(&rest[..i]);
        match rest[i + 3..].find("```") {
            Some(j) => rest = &rest[i + 3 + j + 3..],
            None => break, // unclosed fence: drop it and the rest
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod swarm_exec_p1_tests {
    use super::*;

    struct StubJudge;
    impl JudgeTransport for StubJudge {
        fn judge(&self, _prompt: &str) -> Result<String, String> {
            Ok("VERDICT: done".to_string())
        }
    }

    fn orch_no_judge() -> (SwarmOrchestrator, Arc<ScriptedWorker>) {
        let worker = Arc::new(ScriptedWorker::new());
        (SwarmOrchestrator::new(worker.clone(), None), worker)
    }

    fn one_spec() -> Vec<AgentSpec> {
        vec![AgentSpec {
            name: "a".into(),
            profile: "default".into(),
        }]
    }

    fn review_plan() -> ExecutionPlan {
        let member = |name: &str| AgentSpec {
            name: name.into(),
            profile: "default".into(),
        };
        ExecutionPlan {
            team_name: "team".into(),
            topology: PlanTopology::ReviewLoop,
            stages: vec![
                StageSpec {
                    name: "build".into(),
                    members: vec![member("builder")],
                    input_contract: "in".into(),
                    output_contract: "out".into(),
                    loop_back_to: None,
                },
                StageSpec {
                    name: "review".into(),
                    members: vec![member("reviewer")],
                    input_contract: "in".into(),
                    output_contract: "out".into(),
                    loop_back_to: Some(0),
                },
            ],
            lead_name: "lead".into(),
            lead_profile: "default".into(),
            max_review_iterations: 3,
        }
    }

    // P1 #8: `judge: true` with no `[judge]` transport must degrade to
    // judge-free execution, not 400.
    #[test]
    fn judge_true_without_transport_degrades_to_judge_free() {
        let (orch, _w) = orch_no_judge();
        let created = orch
            .create_with_specs("do things", one_spec(), true)
            .expect("judge:true with no transport must not 400");
        assert!(
            !created.status.judge,
            "degraded swarm must report judge disabled"
        );
    }

    #[test]
    fn staged_judge_true_without_transport_degrades_to_judge_free() {
        let (orch, _w) = orch_no_judge();
        let created = orch
            .create_staged("do things", review_plan(), true)
            .expect("judge:true with no transport must not 400");
        assert!(
            !created.status.judge,
            "degraded swarm must report judge disabled"
        );
    }

    #[test]
    fn judge_true_with_transport_stays_enabled() {
        let worker = Arc::new(ScriptedWorker::new());
        let orch = SwarmOrchestrator::new(worker, Some(Arc::new(StubJudge)));
        let created = orch
            .create_with_specs("do things", one_spec(), true)
            .expect("create");
        assert!(created.status.judge, "judge must stay enabled");
    }

    /// P1 #11 (main): a review-loop retry must not leak the rejected
    /// iteration's transcripts into the re-run's inputs. Drives the
    /// public `status()` polling path end to end.
    #[test]
    fn review_retry_feeds_only_fresh_outputs_downstream() {
        let (orch, worker) = orch_no_judge();
        let created = orch
            .create_staged("task", review_plan(), false)
            .expect("create");
        let id = created.id.clone();
        // r_1 lead, r_2 builder.
        worker.complete("r_2", "BUILD OUTPUT v1");
        orch.status(&id).expect("status"); // stage 0 settles -> reviewer r_3
        worker.tool_call(
            "r_3",
            VERDICT_TOOL_NAME,
            r#"{"pass": false, "failed_items": ["item-x"]}"#,
        );
        worker.complete("r_3", "review done");
        orch.status(&id).expect("status"); // verdict fails -> loop back, builder r_4
        worker.complete("r_4", "BUILD OUTPUT v2");
        orch.status(&id).expect("status"); // stage 0 settles -> reviewer r_5
        let reviewer_task = worker
            .spawns()
            .iter()
            .rev()
            .find(|s| s.1 == "reviewer")
            .expect("reviewer re-spawn")
            .3
            .clone();
        assert!(
            reviewer_task.contains("BUILD OUTPUT v2"),
            "fresh iteration output must reach the reviewer"
        );
        assert!(
            !reviewer_task.contains("BUILD OUTPUT v1"),
            "stale rejected-iteration output must not reach the reviewer"
        );
    }

    /// P1 #11 (latent): a crashed previous-iteration member leaves a
    /// `Failed` record; the fail-closed check matches by stage index, so
    /// without superseding the re-run escalates immediately even when
    /// the fresh iteration passes. The stale record is injected
    /// directly: through the public API a `Failed` record always
    /// escalates at settle, so the "crashed but never superseded"
    /// precondition is a record-level state.
    #[test]
    fn stale_failed_record_does_not_escalate_rerun_stage() {
        let (orch, worker) = orch_no_judge();
        let created = orch
            .create_staged("task", review_plan(), false)
            .expect("create");
        let id = created.id.clone();
        {
            let mut swarms = orch.swarms.lock().unwrap_or_else(|e| e.into_inner());
            let record = swarms.get_mut(&id).expect("record");
            record.agents.push(AgentRecord {
                name: "reviewer".into(),
                profile: "default".into(),
                run_id: "r_stale".into(),
                status: SwarmAgentStatus::Failed,
                round: 1,
                stage: Some(1),
                lead: false,
            });
        }
        worker.complete("r_2", "BUILD OUTPUT v1");
        let view = orch.status(&id).expect("status");
        assert_eq!(
            view.staged.as_ref().expect("staged").stage_index,
            1,
            "stage 0 must settle and spawn the review stage"
        );
        worker.tool_call(
            "r_3",
            VERDICT_TOOL_NAME,
            r#"{"pass": true, "failed_items": []}"#,
        );
        worker.complete("r_3", "looks good");
        let view = orch.status(&id).expect("status");
        assert_eq!(
            view.status,
            SwarmStatus::Complete,
            "fresh passing verdict must complete the plan, not escalate on the stale Failed record"
        );
    }

    /// #14: the escalation note must describe what actually happens. The
    /// orchestrator has no primitive to message the lead run, so the note
    /// must not claim the lead was notified — it records the failure on
    /// the swarm record and marks the swarm incomplete.
    #[test]
    fn review_bound_exhaustion_records_escalation_without_claiming_lead_notification() {
        let (orch, worker) = orch_no_judge();
        let created = orch
            .create_staged("task", review_plan(), false)
            .expect("create");
        let id = created.id.clone();
        // max_review_iterations = 3, so the 4th failing verdict exhausts
        // the bound. r_1 lead; even ids builders, odd ids (from r_3)
        // reviewers.
        for (round, reviewer_n) in [3u32, 5, 7, 9].iter().enumerate() {
            let round = round + 1;
            worker.complete(&format!("r_{}", reviewer_n - 1), "BUILD OUTPUT");
            orch.status(&id).expect("status"); // stage 0 settles -> reviewer
            let reviewer = format!("r_{reviewer_n}");
            worker.tool_call(
                &reviewer,
                VERDICT_TOOL_NAME,
                &format!(r#"{{"pass": false, "failed_items": ["item-{round}"]}}"#),
            );
            worker.complete(&reviewer, "review done");
            orch.status(&id).expect("status"); // verdict fails -> loop back or escalate
        }
        let view = orch.status(&id).expect("status");
        assert_eq!(
            view.status,
            SwarmStatus::Incomplete,
            "exhausted review bound must mark the swarm incomplete"
        );
        let escalation = view
            .staged
            .as_ref()
            .expect("staged")
            .escalation
            .clone()
            .expect("escalation note must be recorded");
        assert!(
            !escalation.contains("escalated to the lead"),
            "must not claim the lead was notified: {escalation}"
        );
        assert!(
            escalation.contains("swarm marked incomplete"),
            "must describe the actual outcome: {escalation}"
        );
        assert!(
            escalation.contains("item-4"),
            "failed items must be recorded in the note: {escalation}"
        );
    }

    /// #14, second site: a failed stage member records the same honest
    /// escalation note — no claim of lead notification.
    #[test]
    fn stage_failure_records_escalation_without_claiming_lead_notification() {
        let (orch, worker) = orch_no_judge();
        let created = orch
            .create_staged("task", review_plan(), false)
            .expect("create");
        let id = created.id.clone();
        worker.fail("r_2", "builder crashed");
        let view = orch.status(&id).expect("status");
        assert_eq!(
            view.status,
            SwarmStatus::Incomplete,
            "failed member must fail the stage closed"
        );
        let escalation = view
            .staged
            .as_ref()
            .expect("staged")
            .escalation
            .clone()
            .expect("escalation note must be recorded");
        assert!(
            !escalation.contains("escalated to the lead"),
            "must not claim the lead was notified: {escalation}"
        );
        assert!(
            escalation.contains("builder"),
            "note must name the failed member: {escalation}"
        );
    }
}
