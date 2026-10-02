//! Swarm orchestrator, staged review loop, completion judge, and swarm
//! caps — behavioral / integration tests per the test-hygiene policy.
//!
//! Run with `cargo test -p pantheon-eval --test runtime_swarm`.
//!
//! Migrated from the inline `mod tests` in `swarm_exec.rs` and `judge.rs`
//! plus `swarm_tests.rs` (declared at `swarm.rs`). Tool-call verdict
//! coverage is NOT duplicated here: it lives in
//! `eval/tests/swarm_verdict_tool.rs` (8 tests: tool-call verdicts,
//! loop-back, escalation, spawn-path routing).
//!
//! No model, no network, no provider keys: [`ScriptedWorker`] is the
//! deterministic worker double. Public APIs only.

use pantheon_api::config::SwarmSection;
use pantheon_runtime::judge::{
    judge_completion, judge_completion_with, judge_prompt, parse_judge_verdict, JudgeTransport,
};
use pantheon_runtime::swarm::{parse_child_result, Caps, ChildStatus, Swarm};
use pantheon_runtime::swarm_exec::{
    AgentSpec, ExecutionPlan, PlanTopology, ScriptedWorker, StageSpec, SwarmAgentStatus,
    SwarmOrchestrator, SwarmStatus, SwarmWorker,
};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Classic swarm orchestrator lifecycle
// ---------------------------------------------------------------------------

#[test]
fn create_validates_agents_range() {
    let o = SwarmOrchestrator::new(Arc::new(ScriptedWorker::new()), None);
    for bad in [0, 9, 100] {
        let err = o.create("task", bad, false).unwrap_err();
        assert_eq!(err.http_status, 400);
        assert_eq!(err.code, "SWARM_VALIDATION");
    }
    assert!(o.create("task", 1, false).is_ok());
    assert!(o.create("task", 8, false).is_ok());
}

#[test]
fn create_rejects_empty_task() {
    let o = SwarmOrchestrator::new(Arc::new(ScriptedWorker::new()), None);
    let err = o.create("  ", 2, false).unwrap_err();
    assert_eq!(err.http_status, 400);
}

#[test]
fn create_degrades_judge_without_transport() {
    // P1 #8: `judge: true` with no configured [judge] transport
    // degrades to judge-free execution instead of failing, so TUI
    // team launches (which always send judge: true) work out of the
    // box. The degrade is explicit in the returned view.
    let o = SwarmOrchestrator::new(Arc::new(ScriptedWorker::new()), None);
    let created = o.create("task", 2, true).expect("create degrades");
    assert!(
        !created.status.judge,
        "judge reports off when no transport is configured"
    );
}

#[test]
fn create_spawns_named_agents_working() {
    let worker = ScriptedWorker::new();
    let o = SwarmOrchestrator::new(Arc::new(worker), None);
    let created = o.create("write a poem", 3, false).unwrap();
    assert!(created.id.starts_with("sw_"));
    assert_eq!(created.status.status, SwarmStatus::Running);
    assert_eq!(created.status.round, 1);
    let names: Vec<&str> = created
        .status
        .agents
        .iter()
        .map(|a| a.name.as_str())
        .collect();
    assert_eq!(names, vec!["subagent-1", "subagent-2", "subagent-3"]);
    for a in &created.status.agents {
        assert!(a.run_id.starts_with("r_"));
        assert_eq!(a.status, SwarmAgentStatus::Working);
    }
}

#[test]
fn all_done_without_judge_completes() {
    let worker = Arc::new(ScriptedWorker::new());
    let o = SwarmOrchestrator::new(worker.clone(), None);
    let created = o.create("task", 2, false).unwrap();
    for a in &created.status.agents {
        worker.complete(&a.run_id, "finished");
    }
    let view = o.status(&created.id).unwrap();
    assert_eq!(view.status, SwarmStatus::Complete);
    assert!(view.verdict.is_none());
    for a in &view.agents {
        assert_eq!(a.status, SwarmAgentStatus::Done);
    }
}

#[test]
fn failed_agent_without_judge_is_incomplete() {
    let worker = Arc::new(ScriptedWorker::new());
    let o = SwarmOrchestrator::new(worker.clone(), None);
    let created = o.create("task", 2, false).unwrap();
    worker.complete(&created.status.agents[0].run_id, "finished");
    worker.fail(&created.status.agents[1].run_id, "boom");
    let view = o.status(&created.id).unwrap();
    assert_eq!(view.status, SwarmStatus::Incomplete);
    assert!(view.verdict.is_none());
}

struct DoneJudge;
impl JudgeTransport for DoneJudge {
    fn judge(&self, _prompt: &str) -> Result<String, String> {
        Ok("VERDICT: done\nNOTES: all good.".into())
    }
}

struct NotDoneJudge;
impl JudgeTransport for NotDoneJudge {
    fn judge(&self, _prompt: &str) -> Result<String, String> {
        Ok("VERDICT: not done\nNOTES: the summary is missing.".into())
    }
}

#[test]
fn judge_done_completes_swarm() {
    let worker = Arc::new(ScriptedWorker::new());
    let o = SwarmOrchestrator::new(worker.clone(), Some(Arc::new(DoneJudge)));
    let created = o.create("task", 2, true).unwrap();
    for a in &created.status.agents {
        worker.complete(&a.run_id, "did the thing");
    }
    let view = o.status(&created.id).unwrap();
    assert_eq!(view.status, SwarmStatus::Complete);
    let verdict = view.verdict.expect("verdict recorded");
    assert!(verdict.done);
}

#[test]
fn judge_not_done_marks_incomplete_with_notes() {
    let worker = Arc::new(ScriptedWorker::new());
    let o = SwarmOrchestrator::new(worker.clone(), Some(Arc::new(NotDoneJudge)));
    let created = o.create("task", 1, true).unwrap();
    worker.complete(&created.status.agents[0].run_id, "partial work");
    let view = o.status(&created.id).unwrap();
    assert_eq!(view.status, SwarmStatus::Incomplete);
    let verdict = view.verdict.expect("verdict recorded");
    assert!(!verdict.done);
    assert!(verdict.notes.contains("missing"));
}

#[test]
fn transcript_combines_agents_and_verdict() {
    let worker = Arc::new(ScriptedWorker::new());
    let o = SwarmOrchestrator::new(worker.clone(), Some(Arc::new(NotDoneJudge)));
    let created = o.create("task", 2, true).unwrap();
    worker.complete(&created.status.agents[0].run_id, "alpha done");
    worker.fail(&created.status.agents[1].run_id, "beta exploded");
    let t = o.transcript(&created.id).unwrap();
    assert!(t.contains("subagent-1"));
    assert!(t.contains("subagent-2"));
    assert!(t.contains("task: task"));
    assert!(t.contains("alpha done"));
    assert!(t.contains("beta exploded"));
    // Polling to terminal ran the judge: verdict section appended.
    assert!(t.contains("judge verdict: not done"));
}

#[test]
fn retry_relaunches_with_judge_feedback() {
    let worker = Arc::new(ScriptedWorker::new());
    let o = SwarmOrchestrator::new(worker.clone(), Some(Arc::new(NotDoneJudge)));
    let created = o.create("original task", 2, true).unwrap();
    for a in &created.status.agents {
        worker.complete(&a.run_id, "partial");
    }
    assert_eq!(
        o.status(&created.id).unwrap().status,
        SwarmStatus::Incomplete
    );

    let view = o.retry(&created.id).unwrap();
    assert_eq!(view.round, 2);
    assert_eq!(view.status, SwarmStatus::Running);
    assert!(view.verdict.is_none());
    for a in &view.agents {
        assert_eq!(a.status, SwarmAgentStatus::Working);
        assert_eq!(a.round, 2);
    }
    // The retry round's tasks carry the judge feedback.
    let spawns = worker.spawns();
    assert_eq!(spawns.len(), 4);
    for (_, _, _, task) in spawns.iter().skip(2) {
        assert!(task.contains("original task"));
        assert!(task.contains("Judge feedback from round 1"));
        assert!(task.contains("the summary is missing"));
    }
}

#[test]
fn retry_refused_when_round_limit_reached() {
    let worker = Arc::new(ScriptedWorker::new());
    let o = SwarmOrchestrator::new(worker.clone(), Some(Arc::new(NotDoneJudge)));
    let created = o.create("task", 1, true).unwrap();
    // Round 1 -> retry -> round 2 -> retry -> round 3 -> retry refused.
    for _ in 0..2 {
        let view = o.status(&created.id).unwrap();
        for a in &view.agents {
            worker.complete(&a.run_id, "partial");
        }
        o.status(&created.id).unwrap();
        o.retry(&created.id).unwrap();
    }
    let view = o.status(&created.id).unwrap();
    for a in &view.agents {
        worker.complete(&a.run_id, "partial");
    }
    let view = o.status(&created.id).unwrap();
    assert_eq!(view.round, 3);
    assert_eq!(view.status, SwarmStatus::Incomplete);
    let err = o.retry(&created.id).unwrap_err();
    assert_eq!(err.http_status, 400);
    assert!(err.message.contains("max rounds"));
}

#[test]
fn retry_refused_when_complete_or_running() {
    let worker = Arc::new(ScriptedWorker::new());
    let o = SwarmOrchestrator::new(worker.clone(), Some(Arc::new(DoneJudge)));
    let created = o.create("task", 1, true).unwrap();
    // Still running: retry refused.
    let err = o.retry(&created.id).unwrap_err();
    assert_eq!(err.http_status, 400);
    worker.complete(&created.status.agents[0].run_id, "done");
    assert_eq!(o.status(&created.id).unwrap().status, SwarmStatus::Complete);
    // Complete (verdict done): retry refused.
    let err = o.retry(&created.id).unwrap_err();
    assert_eq!(err.http_status, 400);
}

#[test]
fn retry_refused_without_verdict() {
    let worker = Arc::new(ScriptedWorker::new());
    let o = SwarmOrchestrator::new(worker.clone(), None);
    let created = o.create("task", 1, false).unwrap();
    worker.fail(&created.status.agents[0].run_id, "boom");
    assert_eq!(
        o.status(&created.id).unwrap().status,
        SwarmStatus::Incomplete
    );
    let err = o.retry(&created.id).unwrap_err();
    assert_eq!(err.http_status, 400);
    assert!(err.message.contains("no judge verdict"));
}

#[test]
fn unknown_swarm_is_404() {
    let o = SwarmOrchestrator::new(Arc::new(ScriptedWorker::new()), None);
    let cases = [
        o.status("sw_999").map(|_| ()).unwrap_err(),
        o.transcript("sw_999").map(|_| ()).unwrap_err(),
        o.retry("sw_999").map(|_| ()).unwrap_err(),
    ];
    for err in cases {
        assert_eq!(err.http_status, 404);
        assert_eq!(err.code, "SWARM_NOT_FOUND");
    }
}

#[test]
fn unknown_run_reports_failed_never_hangs() {
    let worker = ScriptedWorker::new();
    assert_eq!(
        worker.agent_status("r_999").unwrap(),
        SwarmAgentStatus::Failed
    );
}

// ---------------------------------------------------------------------------
// Staged execution (review loop)
// ---------------------------------------------------------------------------

fn staged_spec(name: &str) -> AgentSpec {
    AgentSpec {
        name: name.to_string(),
        profile: format!("{name} profile"),
    }
}

/// gather -> review (loop_back_to 0) -> publish, with a lead.
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

#[test]
fn staged_create_spawns_lead_first_then_stage_one() {
    let worker = Arc::new(ScriptedWorker::new());
    let o = SwarmOrchestrator::new(worker.clone(), None);
    let created = o
        .create_staged("do the thing", test_plan(3), false)
        .unwrap();

    let staged = created.status.staged.as_ref().expect("staged state");
    assert_eq!(staged.team, "Test Crew");
    assert_eq!(staged.stage_index, 0);
    assert_eq!(staged.stage_count, 3);
    assert_eq!(staged.stage_name, "gather");

    // The lead coordination run is the user-facing run; members are
    // never user-facing (lead_run_id names the single openable run).
    assert_eq!(created.status.lead_run_id.as_deref(), Some("r_1"));
    let names: Vec<&str> = created
        .status
        .agents
        .iter()
        .map(|a| a.name.as_str())
        .collect();
    assert_eq!(names, vec!["Lead", "Gatherer"]);
    assert!(created.status.agents[0].lead);

    // Spawn order: lead first, then stage 1's members only.
    let spawns = worker.spawns();
    assert_eq!(spawns.len(), 2);
    assert_eq!(spawns[0].1, "Lead");
    assert_eq!(spawns[1].1, "Gatherer");
    assert!(
        spawns[1]
            .3
            .contains("address your output to the lead, never directly to the user"),
        "member task must carry the lead rule"
    );
}

#[test]
fn staged_pipeline_advances_in_order_with_handoff() {
    let worker = Arc::new(ScriptedWorker::new());
    let o = SwarmOrchestrator::new(worker.clone(), None);
    let created = o
        .create_staged("do the thing", test_plan(3), false)
        .unwrap();

    worker.complete("r_2", "gathered-data");
    let view = o.status(&created.id).unwrap();

    // Stage 2 spawned only after stage 1 settled.
    let staged = view.staged.as_ref().unwrap();
    assert_eq!(staged.stage_index, 1);
    assert_eq!(staged.stage_name, "review");
    let spawns = worker.spawns();
    assert_eq!(spawns.len(), 3);
    assert_eq!(spawns[2].1, "Reviewer");
    // Handoff: stage 1's output flows into stage 2's task, labeled
    // with the output contract.
    assert!(
        spawns[2].3.contains("gathered-data"),
        "handoff carries stage 1 output"
    );
    assert!(
        spawns[2].3.contains("gather-output"),
        "handoff names the output contract"
    );
    // View exposes only the lead plus current-stage agents.
    let names: Vec<&str> = view.agents.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, vec!["Lead", "Reviewer"]);
}

#[test]
fn staged_review_bound_records_escalation_note() {
    let worker = Arc::new(ScriptedWorker::new());
    let o = SwarmOrchestrator::new(worker.clone(), None);
    let created = o
        .create_staged("do the thing", test_plan(1), false)
        .unwrap();

    worker.complete("r_2", "data");
    o.status(&created.id).unwrap();
    worker.complete(
        "r_3",
        "```verdict\n{\"pass\": false, \"failed_items\": [\"x\"]}\n```",
    );
    o.status(&created.id).unwrap();
    // Re-run: gatherer is r_4, reviewer re-spawns as r_5.
    worker.complete("r_4", "data2");
    o.status(&created.id).unwrap();
    worker.complete(
        "r_5",
        "```verdict\n{\"pass\": false, \"failed_items\": [\"y\"]}\n```",
    );
    let view = o.status(&created.id).unwrap();

    assert_eq!(view.status, SwarmStatus::Incomplete);
    let staged = view.staged.as_ref().unwrap();
    assert_eq!(staged.review_iterations, 1, "bound of 1 respected");
    let escalation = staged.escalation.as_ref().expect("escalation recorded");
    assert!(
        escalation.contains("escalation note") && escalation.contains("lead is not notified"),
        "the note records the escalation and is honest that the lead is not notified: {escalation}"
    );
    // Polling again stays settled and does not re-spawn.
    let before = worker.spawns().len();
    let again = o.status(&created.id).unwrap();
    assert_eq!(again.status, SwarmStatus::Incomplete);
    assert_eq!(worker.spawns().len(), before);
}

#[test]
fn staged_review_pass_advances_to_final_stage_and_completes() {
    let worker = Arc::new(ScriptedWorker::new());
    let o = SwarmOrchestrator::new(worker.clone(), None);
    let created = o
        .create_staged("do the thing", test_plan(3), false)
        .unwrap();

    worker.complete("r_2", "data");
    o.status(&created.id).unwrap();
    worker.complete(
        "r_3",
        "```verdict\n{\"pass\": true, \"failed_items\": []}\n```",
    );
    let view = o.status(&created.id).unwrap();
    let staged = view.staged.as_ref().unwrap();
    assert_eq!(staged.stage_index, 2, "passing review advances to publish");
    assert_eq!(worker.spawns().len(), 4);

    worker.complete("r_4", "final deliverable");
    let view = o.status(&created.id).unwrap();
    assert_eq!(view.status, SwarmStatus::Complete);
}

/// Keyword-scan fallback: no tool call, no fenced block, but verdict
/// pass/fail language in the transcript.
#[test]
fn staged_review_keyword_scan_last_resort() {
    let worker = Arc::new(ScriptedWorker::new());
    let o = SwarmOrchestrator::new(worker.clone(), None);
    let created = o
        .create_staged("do the thing", test_plan(3), false)
        .unwrap();

    worker.complete("r_2", "data");
    o.status(&created.id).unwrap();
    worker.complete("r_3", "my verdict: fail, the claims are unverified");
    let view = o.status(&created.id).unwrap();

    assert_eq!(
        view.staged.as_ref().unwrap().review_iterations,
        1,
        "keyword verdict drives the loop-back"
    );
}

/// Last `verdict` tool call wins when the reviewer calls it twice.
#[test]
fn staged_review_last_tool_call_wins() {
    let worker = Arc::new(ScriptedWorker::new());
    let o = SwarmOrchestrator::new(worker.clone(), None);
    let created = o
        .create_staged("do the thing", test_plan(3), false)
        .unwrap();

    worker.complete("r_2", "data");
    o.status(&created.id).unwrap();
    worker.tool_call("r_3", "verdict", r#"{"pass": true, "failed_items": []}"#);
    worker.tool_call(
        "r_3",
        "verdict",
        r#"{"pass": false, "failed_items": ["second thought"]}"#,
    );
    worker.complete("r_3", "done");
    let view = o.status(&created.id).unwrap();

    assert_eq!(view.staged.as_ref().unwrap().review_iterations, 1);
}

#[test]
fn staged_member_failure_fails_closed_with_escalation() {
    let worker = Arc::new(ScriptedWorker::new());
    let o = SwarmOrchestrator::new(worker.clone(), None);
    let created = o
        .create_staged("do the thing", test_plan(3), false)
        .unwrap();

    worker.fail("r_2", "worker crashed");
    let view = o.status(&created.id).unwrap();
    assert_eq!(view.status, SwarmStatus::Incomplete);
    assert!(view.staged.as_ref().unwrap().escalation.is_some());
}

#[test]
fn staged_retry_is_refused() {
    let worker = Arc::new(ScriptedWorker::new());
    let o = SwarmOrchestrator::new(worker.clone(), None);
    let created = o
        .create_staged("do the thing", test_plan(3), false)
        .unwrap();

    let err = o.retry(&created.id).unwrap_err();
    assert_eq!(err.http_status, 400);
    assert!(err.message.contains("review loop"));
}

#[test]
fn staged_plan_validation_rejects_bad_plans() {
    let o = SwarmOrchestrator::new(Arc::new(ScriptedWorker::new()), None);
    // Empty stages.
    let mut p = test_plan(3);
    p.stages.clear();
    assert_eq!(o.create_staged("t", p, false).unwrap_err().http_status, 400);
    // loop_back_to that is not an earlier stage.
    let mut p = test_plan(3);
    p.stages[0].loop_back_to = Some(1);
    let err = o.create_staged("t", p, false).unwrap_err();
    assert!(err.message.contains("not earlier"));
    // Zero review iterations.
    let err = o.create_staged("t", test_plan(0), false).unwrap_err();
    assert!(err.message.contains("max_review_iterations"));
    // Empty lead name.
    let mut p = test_plan(3);
    p.lead_name.clear();
    assert_eq!(o.create_staged("t", p, false).unwrap_err().http_status, 400);
    // Stage with no members.
    let mut p = test_plan(3);
    p.stages[0].members.clear();
    assert_eq!(o.create_staged("t", p, false).unwrap_err().http_status, 400);
}

#[test]
fn classic_swarms_are_unaffected_by_staged_changes() {
    // Regression: plain swarms keep the old shapes.
    let worker = Arc::new(ScriptedWorker::new());
    let o = SwarmOrchestrator::new(worker.clone(), None);
    let created = o.create("task", 2, false).unwrap();
    assert!(created.status.staged.is_none());
    assert_eq!(created.status.lead_run_id, None);
    for a in &created.status.agents {
        assert!(!a.lead);
    }
    worker.complete("r_1", "done");
    worker.complete("r_2", "done");
    let view = o.status(&created.id).unwrap();
    assert_eq!(view.status, SwarmStatus::Complete);
}

// ---------------------------------------------------------------------------
// Completion judge (fail-closed verdict parsing)
// ---------------------------------------------------------------------------

struct ScriptedJudge {
    reply: Result<String, String>,
}
impl JudgeTransport for ScriptedJudge {
    fn judge(&self, _prompt: &str) -> Result<String, String> {
        self.reply.clone()
    }
}

#[test]
fn done_verdict_parses() {
    let v = parse_judge_verdict("VERDICT: done\nNOTES: all three files written and tests pass.");
    assert!(v.done);
    assert!(v.notes.contains("tests pass"));
}

#[test]
fn not_done_verdict_parses() {
    let v = parse_judge_verdict("VERDICT: not done\nNOTES: the migration script is missing.");
    assert!(!v.done);
    assert!(v.notes.contains("migration script"));
}

#[test]
fn verdict_line_is_case_insensitive_but_must_be_exact() {
    assert!(parse_judge_verdict("verdict: done\nNOTES: ok").done);
    // "done" buried in prose is not a verdict line: fail closed.
    assert!(!parse_judge_verdict("I think it is done.\nNOTES: looks fine.").done);
    assert!(!parse_judge_verdict("").done);
}

#[test]
fn transport_error_is_fail_closed() {
    let t = ScriptedJudge {
        reply: Err("connection refused".into()),
    };
    let v = judge_completion_with(&t, "summary", "task");
    assert!(!v.done);
    assert!(v.notes.contains("connection refused"));
}

#[test]
fn transport_reply_flows_through() {
    let t = ScriptedJudge {
        reply: Ok("VERDICT: done\nNOTES: ship it.".into()),
    };
    let v = judge_completion_with(&t, "summary", "task");
    assert!(v.done);
    assert!(v.notes.contains("ship it"));
}

#[test]
fn bare_judge_is_fail_closed() {
    let v = judge_completion("summary", "task");
    assert!(!v.done);
    assert!(v.notes.contains("judge_completion_with"));
}

#[test]
fn prompt_contains_task_summary_and_format_contract() {
    let p = judge_prompt("did X", "do X");
    assert!(p.contains("do X"));
    assert!(p.contains("did X"));
    assert!(p.contains("VERDICT: done"));
    assert!(p.contains("VERDICT: not done"));
}

// ---------------------------------------------------------------------------
// Swarm caps (spawn limits) and child-result envelope parsing
// ---------------------------------------------------------------------------

#[test]
fn depth_cap_blocks_level_two() {
    let mut s = Swarm::new(Caps {
        max_depth: 1,
        max_concurrent: 8,
        max_total_agents: 16,
        ..Caps::default()
    });
    assert!(s.spawn("researcher", 0, "sonnet").is_ok());
    let err = s.spawn("source-hunter", 1, "sonnet").unwrap_err();
    assert_eq!(err.code, "SWARM_MAX_DEPTH");
}

#[test]
fn concurrency_cap_blocks_extra_agents() {
    let mut s = Swarm::new(Caps {
        max_concurrent: 2,
        max_depth: 5,
        max_total_agents: 16,
        ..Caps::default()
    });
    s.spawn("a", 0, "sonnet").unwrap();
    s.spawn("b", 0, "sonnet").unwrap();
    let err = s.spawn("c", 0, "sonnet").unwrap_err();
    assert_eq!(err.code, "SWARM_MAX_CONCURRENT");
}

#[test]
fn budget_cap_fires_once_usage_is_folded_in() {
    let mut s = Swarm::new(Caps {
        token_budget: 1_000,
        max_total_agents: 16,
        ..Caps::default()
    });
    s.spawn("a", 0, "sonnet").unwrap();
    s.complete("a", 1_000, 0, 0);
    let err = s.spawn("b", 0, "sonnet").unwrap_err();
    assert_eq!(err.code, "SWARM_TOKEN_BUDGET");
}

#[test]
fn model_restriction_is_enforced() {
    let mut s = Swarm::new(Caps {
        allowed_models: vec!["kimi".into()],
        ..Caps::default()
    });
    let err = s.spawn("a", 0, "sonnet").unwrap_err();
    assert_eq!(err.code, "SWARM_MODEL_NOT_ALLOWED");
    assert!(s.spawn("b", 0, "kimi").is_ok());
}

#[test]
fn complete_frees_a_slot() {
    let mut s = Swarm::new(Caps {
        max_concurrent: 1,
        max_total_agents: 4,
        max_depth: 4,
        ..Caps::default()
    });
    s.spawn("a", 0, "sonnet").unwrap();
    assert!(s.spawn("b", 0, "sonnet").is_err());
    s.complete("a", 0, 0, 0);
    assert!(s.spawn("c", 0, "sonnet").is_ok());
}

#[test]
fn envelope_parses_fenced_child_result() {
    let text = "Some prose before.\n```child-result\n{\"status\": \"completed\", \
        \"files_changed\": [\"a.rs\"], \"summary\": \"did the thing\", \
        \"decisions\": [\"chose x\"], \"open_questions\": [], \"followups\": [\"next\"]}\n```\nSome prose after.";
    let r = parse_child_result(text);
    assert_eq!(r.status, ChildStatus::Completed);
    assert_eq!(r.files_changed, vec!["a.rs".to_string()]);
    assert_eq!(r.summary, "did the thing");
    assert_eq!(r.decisions, vec!["chose x".to_string()]);
    assert!(r.is_completed());
}

#[test]
fn envelope_parses_bare_json() {
    let r = parse_child_result("{\"status\":\"partial\",\"summary\":\"half done\"}");
    assert_eq!(r.status, ChildStatus::Partial);
    assert_eq!(r.summary, "half done");
    assert!(!r.is_completed());
}

#[test]
fn envelope_missing_fields_take_defaults() {
    let r = parse_child_result("```child-result\n{\"summary\": \"only a summary\"}\n```");
    assert_eq!(r.status, ChildStatus::Unknown);
    assert_eq!(r.summary, "only a summary");
    assert!(r.files_changed.is_empty());
}

#[test]
fn envelope_unknown_status_string_degrades_to_unknown() {
    let r = parse_child_result("{\"status\": \"banana\", \"summary\": \"weird\"}");
    assert_eq!(r.status, ChildStatus::Unknown);
    assert!(!r.is_completed());
}

#[test]
fn envelope_free_text_degrades_gracefully() {
    let r = parse_child_result("I fixed the bug by rewriting the parser.");
    assert_eq!(r.status, ChildStatus::Unknown);
    assert_eq!(r.summary, "I fixed the bug by rewriting the parser.");
    assert!(!r.is_completed());
}

#[test]
fn envelope_empty_text_is_unknown_with_empty_summary() {
    let r = parse_child_result("   \n  ");
    assert_eq!(r.status, ChildStatus::Unknown);
    assert!(r.summary.is_empty());
}

#[test]
fn envelope_to_json_round_trips() {
    let r = parse_child_result(
        "```child-result\n{\"status\":\"failed\",\"summary\":\"could not do it\",\"open_questions\":[\"why?\"]}\n```",
    );
    let json = r.to_json();
    let back = parse_child_result(&json);
    assert_eq!(back, r);
    assert!(json.contains("\"status\":\"failed\""));
}

#[test]
fn per_agent_cap_refuses_after_max_subagents() {
    let mut s = Swarm::new(Caps {
        max_concurrent: 8,
        max_total_agents: 16,
        max_subagents: 2,
        ..Caps::default()
    });
    assert!(s.spawn_for("parent", "child-1", 0, "sonnet").is_ok());
    assert!(s.spawn_for("parent", "child-2", 0, "sonnet").is_ok());
    // A different parent still has its own budget.
    assert!(s.spawn_for("other", "child-3", 0, "sonnet").is_ok());
    let err = s.spawn_for("parent", "child-4", 0, "sonnet").unwrap_err();
    assert_eq!(err.code, "SWARM_PER_AGENT_CAP");
    assert!(err.cause.contains("parent"));
}

#[test]
fn child_spawn_denied_when_allow_child_spawn_false() {
    let mut s = Swarm::new(Caps {
        allow_child_spawn: false,
        max_depth: 4,
        max_concurrent: 8,
        max_total_agents: 16,
        ..Caps::default()
    });
    // The primary agent (depth 0) may still delegate.
    assert!(s.spawn_for("primary", "child", 0, "sonnet").is_ok());
    // A child (depth 1) attempting to delegate is refused.
    let err = s.spawn_for("child", "grandchild", 1, "sonnet").unwrap_err();
    assert_eq!(err.code, "SWARM_CHILD_SPAWN_DENIED");
}

#[test]
fn child_spawn_allowed_by_default() {
    let mut s = Swarm::new(Caps {
        max_depth: 4,
        max_concurrent: 8,
        max_total_agents: 16,
        ..Caps::default()
    });
    assert!(s.spawn_for("child", "grandchild", 1, "sonnet").is_ok());
}

#[test]
fn caps_from_swarm_section_maps_config_keys() {
    let section = SwarmSection {
        max_subagents: 6,
        max_depth: 3,
        max_concurrent: 9,
        allow_child_spawn: false,
    };
    let caps = Caps::from_swarm_section(&section);
    assert_eq!(caps.max_subagents, 6);
    assert_eq!(caps.max_depth, 3);
    assert_eq!(caps.max_concurrent, 9);
    assert!(!caps.allow_child_spawn);
    // Budgets keep built-in defaults (no config spelling yet).
    assert_eq!(caps.max_total_agents, 8);
    assert_eq!(caps.token_budget, 200_000);
}

#[test]
fn caps_for_profile_applies_override() {
    let section = SwarmSection {
        max_subagents: 4,
        ..SwarmSection::default()
    };
    let base = Caps::for_profile(&section, None);
    assert_eq!(base.max_subagents, 4);
    let over = Caps::for_profile(&section, Some(1));
    assert_eq!(over.max_subagents, 1);
}
