//! Replay validation: the improvement gate.
//!
//! Eval-gating ([`crate::gate`]) proves a proposal doesn't break
//! anything. Replay proves it makes something *better* — the SkillOpt
//! learning rule: a Skill/Persona proposal ships only if it strictly
//! improves a held-out task's score, measured by replaying the task with
//! and without the candidate applied.
//!
//! The held-out task set lives at `<data_dir>/nightly/replay-tasks.json`
//! and is managed through the crate API so every client (TUI, web,
//! mobile) shares one set. Tasks the optimizer trains on must never
//! appear here; this set is validation-only.
//!
//! When no replay tasks are configured, the gate REJECTS every
//! skill/persona proposal — loudly, in the audit log. Strict
//! improvement is mandatory; "no tasks to measure against" is not a
//! pass. Configure tasks with the `replay-tasks` CLI.

use crate::propose::Proposal;
use crate::NightlyConfig;
use pantheon_api::model::AuxiliaryModel;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How a replayed task is scored.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ReplayCheck {
    /// The transcript must contain these tool calls in order. Score 1.0
    /// when the full sequence appears, 0.0 otherwise.
    ToolSequence { tools: Vec<String> },
    /// The transcript must (not) contain this text. Score 1.0 on match.
    Contains { text: String, negate: bool },
    /// An LLM judge scores the transcript 0.0–1.0 against the rubric.
    /// Requires LLM steps enabled; otherwise the task is skipped.
    Judge { rubric: String },
}

/// One held-out validation task.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ReplayTask {
    pub id: String,
    pub name: String,
    pub prompt: String,
    pub check: ReplayCheck,
    /// How this task runs itself. When present, the built-in task runner
    /// executes it directly — no `[nightly] replay_command` needed.
    /// When absent, the task needs the external headless-agent command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<TaskExec>,
}

/// A self-contained, deterministic task execution spec.
///
/// The built-in runner spawns `command` with `args` plus the task
/// `prompt` as the final argument, in a fresh empty working directory,
/// with a deadline. Stdout is the transcript that gets scored. Extra
/// `env` entries override the inherited environment for the run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TaskExec {
    /// Program to run, resolved via PATH.
    pub command: String,
    /// Fixed argv before the task prompt.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the run.
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

/// Runs a task and returns the transcript for scoring.
///
/// The `with_proposal` contract: when `Some`, the runner must make the
/// proposal's effect visible to the replay — for a Skill proposal, the
/// candidate `SKILL.md` is staged where the replayed agent will load it;
/// for Persona, the persona note is injected. When `None`, the replay
/// runs clean. Implementations must be bounded (timeout).
pub trait ReplayRunner {
    fn run_transcript(
        &self,
        task: &ReplayTask,
        with_proposal: Option<&Proposal>,
    ) -> Result<String, String>;
}

/// Verdict over the whole replay gate.
#[derive(Debug, Clone)]
pub enum ReplayVerdict {
    /// Total score with the proposal strictly exceeds the baseline.
    Pass { summary: String },
    /// No strict improvement — or the improvement couldn't be measured
    /// (no replay tasks configured, a replay errored, a score wasn't
    /// finite). The proposal is rejected; the reason names the cause.
    /// Missing or broken replay infrastructure NEVER degrades to a
    /// pass: "couldn't measure" is not "improved".
    Fail { reason: String },
}

/// JSON store for the held-out task set.
pub struct ReplayStore {
    path: PathBuf,
    tasks: Vec<ReplayTask>,
}

fn store_path(data_dir: &Path) -> PathBuf {
    data_dir.join("nightly").join("replay-tasks.json")
}

impl ReplayStore {
    pub fn open(data_dir: &Path) -> Result<Self, String> {
        let path = store_path(data_dir);
        let tasks = match std::fs::read_to_string(&path) {
            Ok(text) => {
                serde_json::from_str(&text).map_err(|e| format!("parse {}: {e}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(format!("read {}: {e}", path.display())),
        };
        Ok(Self { path, tasks })
    }

    pub fn list(&self) -> &[ReplayTask] {
        &self.tasks
    }

    /// Save a task. Validation is loud: empty id/name/prompt and empty
    /// check payloads are rejected instead of stored silently.
    pub fn save(&mut self, task: ReplayTask) -> Result<(), String> {
        validate_task(&task)?;
        if let Some(pos) = self.tasks.iter().position(|t| t.id == task.id) {
            self.tasks[pos] = task;
        } else {
            self.tasks.push(task);
        }
        self.persist()
    }

    pub fn delete(&mut self, id: &str) -> Result<bool, String> {
        let before = self.tasks.len();
        self.tasks.retain(|t| t.id != id);
        let removed = self.tasks.len() < before;
        if removed {
            self.persist()?;
        }
        Ok(removed)
    }

    fn persist(&self) -> Result<(), String> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(&self.tasks)
            .map_err(|e| format!("encode replay tasks: {e}"))?;
        std::fs::write(&self.path, text).map_err(|e| format!("write {}: {e}", self.path.display()))
    }
}

fn validate_task(task: &ReplayTask) -> Result<(), String> {
    if task.id.trim().is_empty() {
        return Err("replay task id must not be empty".into());
    }
    if task.name.trim().is_empty() {
        return Err("replay task name must not be empty".into());
    }
    if task.prompt.trim().is_empty() {
        return Err("replay task prompt must not be empty".into());
    }
    match &task.check {
        ReplayCheck::ToolSequence { tools } => {
            if tools.len() < 2 {
                return Err("tool_sequence check needs at least 2 tools".into());
            }
        }
        ReplayCheck::Contains { text, .. } => {
            if text.trim().is_empty() {
                return Err("contains check needs non-empty text".into());
            }
        }
        ReplayCheck::Judge { rubric } => {
            if rubric.trim().is_empty() {
                return Err("judge check needs a non-empty rubric".into());
            }
        }
    }
    if let Some(exec) = &task.exec {
        if exec.command.trim().is_empty() {
            return Err("exec spec needs a non-empty command".into());
        }
        for k in exec.env.keys() {
            if k.trim().is_empty() || k.contains('=') {
                return Err("exec env keys must be non-empty and not contain '='".into());
            }
        }
    }
    Ok(())
}

/// Score a transcript against a check. Deterministic checks are pure;
/// [`ReplayCheck::Judge`] needs the LLM judge.
fn score_transcript(
    check: &ReplayCheck,
    transcript: &str,
    judge: Option<(&dyn crate::llm::NightlyLlm, &AuxiliaryModel)>,
) -> Result<f64, String> {
    match check {
        ReplayCheck::ToolSequence { tools } => {
            // Every tool must appear in order. A transcript line counts
            // when it names the tool (tool-call markers or plain text).
            let mut idx = 0;
            for line in transcript.lines() {
                if idx < tools.len() && line.contains(&tools[idx]) {
                    idx += 1;
                }
            }
            Ok(if idx == tools.len() { 1.0 } else { 0.0 })
        }
        ReplayCheck::Contains { text, negate } => {
            let found = transcript.contains(text);
            Ok(if found != *negate { 1.0 } else { 0.0 })
        }
        ReplayCheck::Judge { rubric } => {
            let (llm, model) =
                judge.ok_or_else(|| "judge check requires LLM steps enabled".to_string())?;
            // Judge scoring goes through the refine_proposal contract
            // (see score_with_judge); the prompt is built there.
            score_with_judge(llm, model, rubric, transcript)
        }
    }
}

fn score_with_judge(
    llm: &dyn crate::llm::NightlyLlm,
    model: &AuxiliaryModel,
    rubric: &str,
    transcript: &str,
) -> Result<f64, String> {
    // The judge prompt is built as a synthetic proposal draft so it
    // flows through the existing `refine_proposal` contract, which is
    // the trait's scoring-shaped entrypoint. The returned text must be
    // a bare number.
    let draft = crate::propose::Proposal {
        id: "nly_judge".into(),
        kind: crate::propose::ProposalKind::MemoryLesson {
            key: "judge".into(),
        },
        title: "replay judge".into(),
        body: format!(
            "Score the following transcript 0.0 to 1.0 against this rubric: {rubric}\n\nTranscript:\n{transcript}\n\nReply with only the number, no other text."
        ),
        provenance_runs: Vec::new(),
        provenance_turns: Vec::new(),
        eval_tags: Vec::new(),
        status: crate::propose::ProposalStatus::Proposed,
    };
    let raw = llm.refine_proposal(model, &draft)?;
    let score: f64 = raw
        .trim()
        .parse()
        .map_err(|_| format!("judge returned non-numeric score: {raw:?}"))?;
    if !score.is_finite() {
        return Err(format!("judge returned non-finite score: {raw:?}"));
    }
    Ok(score.clamp(0.0, 1.0))
}

/// The replay gate: strict improvement required.
///
/// For every applicable task, the task is replayed with and without the
/// proposal. The proposal passes only when the total with-score strictly
/// exceeds the total baseline score. Equal scores fail — "didn't make it
/// worse" is what eval-gating already proves; this gate proves "made it
/// better".
///
/// Anything that prevents a fair measurement fails the proposal:
/// - no replay tasks configured → reject (configure some first)
/// - no tasks scorable (e.g. only judge tasks, LLM disabled) → reject
/// - a replay errors → reject
/// - a score isn't finite → reject
pub fn replay_gate(
    proposal: &Proposal,
    store: &ReplayStore,
    runner: &dyn ReplayRunner,
    judge: Option<(&dyn crate::llm::NightlyLlm, &AuxiliaryModel)>,
    _config: &NightlyConfig,
) -> ReplayVerdict {
    let tasks = store.list();
    if tasks.is_empty() {
        return ReplayVerdict::Fail {
            reason: format!(
                "proposal '{}': no replay tasks configured — strict improvement cannot be measured; add tasks with `pantheon nightly replay-tasks add`",
                proposal.id
            ),
        };
    }
    let mut base_total = 0.0;
    let mut with_total = 0.0;
    let mut scored = 0usize;
    let mut skipped = 0usize;
    for task in tasks {
        if matches!(task.check, ReplayCheck::Judge { .. }) && judge.is_none() {
            skipped += 1;
            continue;
        }
        let base_tx = match runner.run_transcript(task, None) {
            Ok(t) => t,
            Err(e) => {
                return ReplayVerdict::Fail {
                    reason: format!(
                        "proposal '{}': replay of '{}' failed (baseline): {e}",
                        proposal.id, task.id
                    ),
                }
            }
        };
        let with_tx = match runner.run_transcript(task, Some(proposal)) {
            Ok(t) => t,
            Err(e) => {
                return ReplayVerdict::Fail {
                    reason: format!(
                        "proposal '{}': replay of '{}' failed (with proposal): {e}",
                        proposal.id, task.id
                    ),
                }
            }
        };
        let base = match score_transcript(&task.check, &base_tx, judge) {
            Ok(s) => s,
            Err(e) => {
                return ReplayVerdict::Fail {
                    reason: format!(
                        "proposal '{}': scoring '{}' failed (baseline): {e}",
                        proposal.id, task.id
                    ),
                }
            }
        };
        let with = match score_transcript(&task.check, &with_tx, judge) {
            Ok(s) => s,
            Err(e) => {
                return ReplayVerdict::Fail {
                    reason: format!(
                        "proposal '{}': scoring '{}' failed (with proposal): {e}",
                        proposal.id, task.id
                    ),
                }
            }
        };
        // A non-finite score means the measurement is broken; it can
        // never count as improvement.
        if !base.is_finite() || !with.is_finite() {
            return ReplayVerdict::Fail {
                reason: format!(
                    "proposal '{}': non-finite replay score on '{}' (baseline {base:?}, with {with:?})",
                    proposal.id, task.id
                ),
            };
        }
        base_total += base;
        with_total += with;
        scored += 1;
    }
    if scored == 0 {
        return ReplayVerdict::Fail {
            reason: format!(
                "proposal '{}': {skipped} judge task(s) skipped (LLM disabled); nothing scorable",
                proposal.id
            ),
        };
    }
    if with_total > base_total {
        let skipped_note = if skipped > 0 {
            format!(" ({skipped} judge task(s) skipped)")
        } else {
            String::new()
        };
        ReplayVerdict::Pass {
            summary: format!(
                "replay: {with_total:.2} vs baseline {base_total:.2} across {scored} task(s){skipped_note}"
            ),
        }
    } else {
        ReplayVerdict::Fail {
            reason: format!(
                "proposal '{}': no strict improvement (with {with_total:.2} vs baseline {base_total:.2} across {scored} task(s))",
                proposal.id
            ),
        }
    }
}

/// A [`ReplayRunner`] that always fails: used when no replay command is
/// configured (`[nightly] replay_command` absent), so the replay gate
/// rejects loudly instead of silently passing. Pantheon ships no
/// headless agent runner; point `replay_command` at one
/// (`<command> <prompt>`, stdout = transcript) to enable skill/persona
/// replay validation.
pub struct UnconfiguredReplayRunner;

impl ReplayRunner for UnconfiguredReplayRunner {
    fn run_transcript(
        &self,
        task: &ReplayTask,
        _with_proposal: Option<&Proposal>,
    ) -> Result<String, String> {
        Err(format!(
            "no replay command configured ([nightly] replay_command) — cannot replay task '{}'; configure a headless runner to enable replay validation",
            task.id
        ))
    }
}

/// A [`ReplayRunner`] that shells out to a headless agent command.
///
/// Runs `<command> <prompt>` with a deadline, capturing stdout as the
/// transcript. When a proposal is provided, `PANTHEON_REPLAY_SKILL_DIR`
/// points at a staging dir containing the candidate skill/persona
/// overlay, so the replayed agent sees exactly the change under test.
/// (Skill overlays load through Pantheon's normal skill discovery;
/// persona overlays are staged as `persona.md` for the runner to
/// inject — the same overlay the runtime applies to live sessions, see
/// [`crate::persona`].)
pub struct SubprocessReplayRunner {
    pub command: String,
    pub timeout: Duration,
}

impl SubprocessReplayRunner {
    pub fn new(command: String, timeout: Duration) -> Self {
        Self { command, timeout }
    }
}

impl ReplayRunner for SubprocessReplayRunner {
    fn run_transcript(
        &self,
        task: &ReplayTask,
        with_proposal: Option<&Proposal>,
    ) -> Result<String, String> {
        use std::process::{Command, Stdio};
        use std::time::Instant;

        let mut cmd = Command::new(&self.command);
        cmd.arg(&task.prompt)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(p) = with_proposal {
            // Stage the proposal where the headless runner will pick it
            // up. The runner contract: $PANTHEON_REPLAY_SKILL_DIR holds
            // the candidate overlay.
            let dir = stage_proposal_overlay(p)?;
            cmd.env("PANTHEON_REPLAY_SKILL_DIR", &dir);
            // Note: the temp dir is intentionally left for the child to
            // read; it is cleaned on next pass start.
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn replay command '{}': {e}", self.command))?;
        let deadline = Instant::now() + self.timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let out = child
                        .wait_with_output()
                        .map_err(|e| format!("read replay output: {e}"))?;
                    if !status.success() {
                        return Err(format!(
                            "replay command exited {}",
                            status.code().unwrap_or(-1)
                        ));
                    }
                    return String::from_utf8(out.stdout)
                        .map_err(|e| format!("replay output not UTF-8: {e}"));
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(format!("replay timed out after {:?}", self.timeout));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(format!("wait on replay child: {e}")),
            }
        }
    }
}

/// Stage a proposal as a skill/persona overlay for the replay runner.
/// Returns the staging dir path. Memory lessons need no overlay — they
/// are data, not behavior — so this is a no-op dir for them.
fn stage_proposal_overlay(p: &Proposal) -> Result<String, String> {
    let dir = std::env::temp_dir().join(format!("pantheon-replay-{}", p.id));
    std::fs::create_dir_all(&dir).map_err(|e| format!("create replay staging dir: {e}"))?;
    match &p.kind {
        crate::propose::ProposalKind::Skill { name, .. } => {
            std::fs::write(dir.join(format!("{name}.md")), &p.body)
                .map_err(|e| format!("stage skill overlay: {e}"))?;
        }
        crate::propose::ProposalKind::Persona { .. } => {
            std::fs::write(dir.join("persona.md"), &p.body)
                .map_err(|e| format!("stage persona overlay: {e}"))?;
        }
        crate::propose::ProposalKind::MemoryLesson { .. } => {}
    }
    Ok(dir.to_string_lossy().into_owned())
}

/// A [`ReplayRunner`] that executes the task's own [`TaskExec`] spec —
/// the built-in headless runner. No external agent needed: the task
/// defines exactly how it runs, so a held-out task is a deterministic
/// command whose stdout is the scored transcript.
///
/// Isolation: each run gets a fresh empty working directory and a
/// deadline; the child inherits the process environment plus the
/// task's `env` overrides. When a proposal is under test,
/// `PANTHEON_REPLAY_SKILL_DIR` points at the staged overlay
/// ([`stage_proposal_overlay`]). This is process isolation, not a
/// sandbox — a task spec is trusted code. For untrusted or
/// agent-driven replays, point `[nightly] replay_command` at a
/// sandboxed headless agent instead.
pub struct TaskSpecReplayRunner {
    pub timeout: Duration,
}

impl TaskSpecReplayRunner {
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }
}

impl ReplayRunner for TaskSpecReplayRunner {
    fn run_transcript(
        &self,
        task: &ReplayTask,
        with_proposal: Option<&Proposal>,
    ) -> Result<String, String> {
        use std::process::{Command, Stdio};
        use std::time::Instant;

        let spec = task.exec.as_ref().ok_or_else(|| {
            format!(
                "replay task '{}' has no exec spec — give it one (`replay-tasks add --exec ...`) or configure [nightly] replay_command",
                task.id
            )
        })?;
        if spec.command.trim().is_empty() {
            return Err(format!("replay task '{}': exec command is empty", task.id));
        }
        // Fresh empty working directory per run: the task starts with a
        // clean slate, but this is NOT a sandbox — it inherits our
        // environment and can still reach the filesystem. Task specs
        // are trusted code (see the struct docs).
        let workdir = std::env::temp_dir().join(format!(
            "pantheon-replay-run-{}-{}",
            sanitize_id(&task.id),
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&workdir);
        std::fs::create_dir_all(&workdir).map_err(|e| format!("create replay workdir: {e}"))?;

        let mut cmd = Command::new(&spec.command);
        cmd.args(&spec.args)
            .arg(&task.prompt)
            .current_dir(&workdir)
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        if let Some(p) = with_proposal {
            let dir = stage_proposal_overlay(p)?;
            cmd.env("PANTHEON_REPLAY_SKILL_DIR", &dir);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn replay task '{}': {e}", task.id))?;
        let deadline = Instant::now() + self.timeout;
        let result = loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let out = child
                        .wait_with_output()
                        .map_err(|e| format!("read replay output: {e}"))?;
                    if !status.success() {
                        break Err(format!(
                            "replay task '{}' exited {}",
                            task.id,
                            status.code().unwrap_or(-1)
                        ));
                    }
                    break String::from_utf8(out.stdout)
                        .map_err(|e| format!("replay task '{}' output not UTF-8: {e}", task.id));
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        break Err(format!(
                            "replay task '{}' timed out after {:?}",
                            task.id, self.timeout
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => break Err(format!("wait on replay child: {e}")),
            }
        };
        let _ = std::fs::remove_dir_all(&workdir);
        result
    }
}

/// Keep temp-dir names filesystem-safe: task ids are validated
/// non-empty but may contain anything else.
fn sanitize_id(id: &str) -> String {
    let clean: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let clean = clean.trim_matches('_').to_string();
    if clean.is_empty() {
        "task".to_string()
    } else {
        clean
    }
}

/// The runner hosts should use: picks the right strategy per task.
///
/// 1. Task has an [`TaskExec`] spec → [`TaskSpecReplayRunner`]
///    (built-in, deterministic, no configuration needed).
/// 2. Else `[nightly] replay_command` is set →
///    [`SubprocessReplayRunner`] (headless agent).
/// 3. Else → [`UnconfiguredReplayRunner`] (loud failure, the gate
///    rejects — "couldn't measure" is never a pass).
pub struct CompositeReplayRunner {
    /// External headless-agent command (`<command> <prompt>`), if any.
    pub command: Option<String>,
    /// Deadline per replay, for both strategies.
    pub timeout: Duration,
}

impl CompositeReplayRunner {
    pub fn new(command: Option<String>, timeout: Duration) -> Self {
        Self { command, timeout }
    }
}

impl ReplayRunner for CompositeReplayRunner {
    fn run_transcript(
        &self,
        task: &ReplayTask,
        with_proposal: Option<&Proposal>,
    ) -> Result<String, String> {
        if task.exec.is_some() {
            return TaskSpecReplayRunner::new(self.timeout).run_transcript(task, with_proposal);
        }
        match &self.command {
            Some(cmd) => SubprocessReplayRunner::new(cmd.clone(), self.timeout)
                .run_transcript(task, with_proposal),
            None => UnconfiguredReplayRunner.run_transcript(task, with_proposal),
        }
    }
}

// Small deterministic invariant tests only.
