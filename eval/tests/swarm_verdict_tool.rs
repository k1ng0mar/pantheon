//! Reviewer `verdict` tool integration: the staged review loop's primary
//! verdict channel is a structured tool call, not transcript parsing.
//!
//! Behavioral / integration tests per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval --test swarm_verdict_tool`.
//!
//! No model, no network, no provider keys: [`ScriptedWorker`] is the
//! deterministic worker double, and `verdict` tool calls are scripted
//! through [`ScriptedWorker::tool_call`] exactly as a reviewer's model
//! turn would emit them.

use pantheon_runtime::swarm_exec::{
    AgentSpec, ExecutionPlan, PlanTopology, ScriptedWorker, StageSpec, SwarmAgentStatus,
    SwarmError, SwarmOrchestrator, SwarmStatus, SwarmWorker, ToolCall,
};
use std::sync::{Arc, Mutex};

fn staged_spec(name: &str) -> AgentSpec {
    AgentSpec {
        name: name.to_string(),
        profile: format!("{name} profile"),
    }
}

/// gather -> review (loop_back_to 0) -> publish.
fn test_plan(max_review_iterations: u32) -> ExecutionPlan {
    ExecutionPlan {
        team_name: "Test Crew".to_string(),
        topology: PlanTopology::ReviewLoop,
        stages: vec![
            StageSpec {
                name: "gather".to_string(),
                members: vec![staged_spec("Gatherer")],
                input_contract: "none".to_string(),
                output_contract: "gather-output".to_string(),
                loop_back_to: None,
            },
            StageSpec {
                name: "review".to_string(),
                members: vec![staged_spec("Reviewer")],
                input_contract: "gather-output".to_string(),
                output_contract: "verification verdict".to_string(),
                loop_back_to: Some(0),
            },
            StageSpec {
                name: "publish".to_string(),
                members: vec![staged_spec("Writer")],
                input_contract: "verified gather-output".to_string(),
                output_contract: "final".to_string(),
                loop_back_to: None,
            },
        ],
        lead_name: "Lead".to_string(),
        lead_profile: "lead profile".to_string(),
        max_review_iterations,
    }
}

fn verdict_args(pass: bool, failed_items: &[&str]) -> String {
    let items: Vec<String> = failed_items.iter().map(|s| format!("\"{s}\"")).collect();
    format!(
        "{{\"pass\": {pass}, \"failed_items\": [{}]}}",
        items.join(", ")
    )
}

/// Drive the swarm to the review stage: gatherer (r_2) completes, reviewer
/// (r_3) is spawned and waiting.
fn reach_review(worker: &Arc<ScriptedWorker>, o: &SwarmOrchestrator, id: &str) {
    worker.complete("r_2", "gathered-data");
    let view = o.status(id).unwrap();
    assert_eq!(view.staged.as_ref().unwrap().stage_index, 1);
}

#[test]
fn tool_call_pass_advances_without_loopback() {
    let worker = Arc::new(ScriptedWorker::new());
    let o = SwarmOrchestrator::new(worker.clone(), None);
    let created = o
        .create_staged("do the thing", test_plan(3), false)
        .unwrap();
    reach_review(&worker, &o, &created.id);

    // The reviewer emits its verdict as a structured tool call - no fenced
    // block, no verdict prose in the transcript.
    worker.tool_call("r_3", "verdict", &verdict_args(true, &[]));
    worker.complete("r_3", "looks good");
    let view = o.status(&created.id).unwrap();

    let staged = view.staged.as_ref().unwrap();
    assert_eq!(staged.stage_index, 2, "passing verdict advances to publish");
    assert_eq!(staged.review_iterations, 0, "no loop-back on pass");
    assert!(staged.escalation.is_none());
}

#[test]
fn tool_call_fail_triggers_loopback_with_feedback() {
    let worker = Arc::new(ScriptedWorker::new());
    let o = SwarmOrchestrator::new(worker.clone(), None);
    let created = o
        .create_staged("do the thing", test_plan(3), false)
        .unwrap();
    reach_review(&worker, &o, &created.id);

    worker.tool_call(
        "r_3",
        "verdict",
        &verdict_args(false, &["claims unverified"]),
    );
    worker.complete("r_3", "I reviewed the output and found problems.");
    let view = o.status(&created.id).unwrap();

    let staged = view.staged.as_ref().unwrap();
    assert_eq!(
        staged.stage_index, 0,
        "failed verdict re-runs the target stage"
    );
    assert_eq!(staged.review_iterations, 1);
    // Gatherer re-spawned (r_4) with the failed items as feedback.
    let spawns = worker.spawns();
    assert_eq!(spawns.len(), 4);
    assert_eq!(spawns[3].1, "Gatherer");
    assert!(
        spawns[3].3.contains("claims unverified"),
        "loop-back carries the tool-call failed items"
    );
}

#[test]
fn three_failed_verdicts_escalate_to_note() {
    let worker = Arc::new(ScriptedWorker::new());
    let o = SwarmOrchestrator::new(worker.clone(), None);
    let created = o
        .create_staged("do the thing", test_plan(2), false)
        .unwrap();

    // Failure 1: r_3 reviewer rejects.
    worker.complete("r_2", "data");
    o.status(&created.id).unwrap();
    worker.tool_call("r_3", "verdict", &verdict_args(false, &["x"]));
    worker.complete("r_3", "bad");
    let view = o.status(&created.id).unwrap();
    assert_eq!(view.staged.as_ref().unwrap().review_iterations, 1);

    // Failure 2: r_5 reviewer rejects.
    worker.complete("r_4", "data2");
    o.status(&created.id).unwrap();
    worker.tool_call("r_5", "verdict", &verdict_args(false, &["y"]));
    worker.complete("r_5", "still bad");
    let view = o.status(&created.id).unwrap();
    assert_eq!(view.staged.as_ref().unwrap().review_iterations, 2);

    // Failure 3: bound exhausted -> escalation, no further spawns.
    worker.complete("r_6", "data3");
    o.status(&created.id).unwrap();
    worker.tool_call("r_7", "verdict", &verdict_args(false, &["z"]));
    worker.complete("r_7", "bad again");
    let view = o.status(&created.id).unwrap();

    assert_eq!(view.status, SwarmStatus::Incomplete);
    let staged = view.staged.as_ref().unwrap();
    assert_eq!(staged.review_iterations, 2, "bound of 2 respected");
    let escalation = staged.escalation.as_ref().expect("escalation recorded");
    assert!(
        escalation.contains("escalation note") && escalation.contains("lead is not notified"),
        "the note records the escalation and is honest that the lead is not notified: {escalation}"
    );
    let before = worker.spawns().len();
    let again = o.status(&created.id).unwrap();
    assert_eq!(again.status, SwarmStatus::Incomplete);
    assert_eq!(
        worker.spawns().len(),
        before,
        "settled swarm does not re-spawn"
    );
}

#[test]
fn no_verdict_from_any_source_fails_verification() {
    let worker = Arc::new(ScriptedWorker::new());
    let o = SwarmOrchestrator::new(worker.clone(), None);
    let created = o
        .create_staged("do the thing", test_plan(3), false)
        .unwrap();
    reach_review(&worker, &o, &created.id);

    // No tool call, no fenced block, no verdict keywords - the reviewer
    // simply never emitted a verdict.
    worker.complete(
        "r_3",
        "I looked at the output. It seems fine, nothing to add.",
    );
    let view = o.status(&created.id).unwrap();

    let staged = view.staged.as_ref().unwrap();
    assert_eq!(
        staged.stage_index, 0,
        "missing verdict counts as failed verification and loops back"
    );
    assert_eq!(staged.review_iterations, 1);
    let spawns = worker.spawns();
    assert!(
        spawns[3].3.contains("reviewer emitted no verdict"),
        "loop-back feedback names the missing verdict"
    );
}

#[test]
fn fenced_block_fallback_when_no_tool_call() {
    let worker = Arc::new(ScriptedWorker::new());
    let o = SwarmOrchestrator::new(worker.clone(), None);
    let created = o
        .create_staged("do the thing", test_plan(3), false)
        .unwrap();
    reach_review(&worker, &o, &created.id);

    // No tool call: the fenced ```verdict block still drives the loop.
    worker.complete(
        "r_3",
        "checked\n```verdict\n{\"pass\": false, \"failed_items\": [\"stale numbers\"]}\n```",
    );
    let view = o.status(&created.id).unwrap();

    let staged = view.staged.as_ref().unwrap();
    assert_eq!(staged.stage_index, 0);
    assert_eq!(staged.review_iterations, 1);
    assert!(worker.spawns()[3].3.contains("stale numbers"));
}

#[test]
fn tool_call_wins_over_contradictory_fenced_block() {
    let worker = Arc::new(ScriptedWorker::new());
    let o = SwarmOrchestrator::new(worker.clone(), None);
    let created = o
        .create_staged("do the thing", test_plan(3), false)
        .unwrap();
    reach_review(&worker, &o, &created.id);

    // Structured tool call says pass; the transcript's fenced block says
    // fail. The tool call is the primary channel and wins.
    worker.tool_call("r_3", "verdict", &verdict_args(true, &[]));
    worker.complete(
        "r_3",
        "```verdict\n{\"pass\": false, \"failed_items\": [\"stale\"]}\n```",
    );
    let view = o.status(&created.id).unwrap();

    let staged = view.staged.as_ref().unwrap();
    assert_eq!(
        staged.stage_index, 2,
        "tool call verdict wins over fenced block"
    );
    assert_eq!(staged.review_iterations, 0);
}

#[test]
fn malformed_tool_call_args_fall_back_to_transcript() {
    let worker = Arc::new(ScriptedWorker::new());
    let o = SwarmOrchestrator::new(worker.clone(), None);
    let created = o
        .create_staged("do the thing", test_plan(3), false)
        .unwrap();
    reach_review(&worker, &o, &created.id);

    // Garbage args on the tool call: extraction skips it and falls back
    // to the fenced block in the transcript.
    worker.tool_call("r_3", "verdict", "not json at all");
    worker.complete(
        "r_3",
        "```verdict\n{\"pass\": true, \"failed_items\": []}\n```",
    );
    let view = o.status(&created.id).unwrap();

    let staged = view.staged.as_ref().unwrap();
    assert_eq!(
        staged.stage_index, 2,
        "fenced fallback rescues malformed tool args"
    );
}

/// Worker that records which spawn path each agent took: reviewers must go
/// through `spawn_reviewer` (the verdict-tool registration path); the lead
/// and ordinary members must go through `spawn_agent`.
struct SpawnPathWorker {
    inner: ScriptedWorker,
    reviewer_spawns: Mutex<Vec<String>>,
    agent_spawns: Mutex<Vec<String>>,
}

impl SpawnPathWorker {
    fn new() -> Self {
        Self {
            inner: ScriptedWorker::new(),
            reviewer_spawns: Mutex::new(Vec::new()),
            agent_spawns: Mutex::new(Vec::new()),
        }
    }
}

impl SwarmWorker for SpawnPathWorker {
    fn spawn_agent(
        &self,
        swarm_id: &str,
        agent_name: &str,
        profile: &str,
        task: &str,
    ) -> Result<String, SwarmError> {
        self.agent_spawns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(agent_name.to_string());
        self.inner.spawn_agent(swarm_id, agent_name, profile, task)
    }

    fn spawn_reviewer(
        &self,
        swarm_id: &str,
        agent_name: &str,
        profile: &str,
        task: &str,
    ) -> Result<String, SwarmError> {
        self.reviewer_spawns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(agent_name.to_string());
        // Scripted runs have no real session; delegate to the scripted
        // spawn so the run exists for status/transcript polling.
        self.inner.spawn_agent(swarm_id, agent_name, profile, task)
    }

    fn agent_status(&self, run_id: &str) -> Result<SwarmAgentStatus, SwarmError> {
        self.inner.agent_status(run_id)
    }

    fn agent_transcript(&self, run_id: &str) -> Result<String, SwarmError> {
        self.inner.agent_transcript(run_id)
    }

    fn agent_tool_calls(&self, run_id: &str) -> Result<Vec<ToolCall>, SwarmError> {
        self.inner.agent_tool_calls(run_id)
    }
}

#[test]
fn only_reviewers_take_the_verdict_tool_spawn_path() {
    let worker = Arc::new(SpawnPathWorker::new());
    let o = SwarmOrchestrator::new(worker.clone(), None);
    let created = o
        .create_staged("do the thing", test_plan(3), false)
        .unwrap();

    // Lead (r_1) and gatherer (r_2) spawn through the plain agent path.
    worker.inner.complete("r_2", "gathered-data");
    o.status(&created.id).unwrap();
    // Reviewer (r_3) spawns through the reviewer path.
    worker.inner.complete(
        "r_3",
        "```verdict\n{\"pass\": true, \"failed_items\": []}\n```",
    );
    o.status(&created.id).unwrap();

    let reviewer_spawns = worker.reviewer_spawns.lock().unwrap();
    let agent_spawns = worker.agent_spawns.lock().unwrap();
    assert_eq!(
        reviewer_spawns.as_slice(),
        &["Reviewer".to_string()],
        "only the review-stage member uses spawn_reviewer"
    );
    assert!(
        agent_spawns.contains(&"Lead".to_string()),
        "lead uses the plain spawn path"
    );
    assert!(
        agent_spawns.contains(&"Gatherer".to_string()),
        "non-review members use the plain spawn path"
    );
    assert!(
        !agent_spawns.contains(&"Reviewer".to_string()),
        "reviewer never uses the plain spawn path"
    );

    // The reviewer's task carries the verdict-tool instruction; the
    // gatherer's does not.
    let spawns = worker.inner.spawns();
    let reviewer_task = spawns.iter().find(|s| s.1 == "Reviewer").unwrap();
    assert!(
        reviewer_task
            .3
            .contains("calling the `verdict` tool exactly once"),
        "reviewer task instructs the verdict tool call"
    );
    let gatherer_task = spawns.iter().find(|s| s.1 == "Gatherer").unwrap();
    assert!(
        !gatherer_task
            .3
            .contains("calling the `verdict` tool exactly once"),
        "non-reviewer task has no verdict instruction"
    );
}
