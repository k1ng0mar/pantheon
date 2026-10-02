//! `/swarm` renders an honest delegation snapshot from the ledger.
//!
//! Behavioral / integration tests per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval`.
use pantheon_api::events::Event;
use pantheon_storage::{Ledger, LedgerEntry};
use pantheon_tui::swarm_view as sv;

fn replay(ledger: &Ledger, run: &str) -> Vec<LedgerEntry> {
    ledger.replay(run).unwrap()
}

fn spawn(ledger: &Ledger, run: &str, agent: &str) {
    ledger
        .append(&Event::AgentSpawned {
            run_id: run.into(),
            agent: agent.into(),
            child_run_id: None,
            call_id: None,
        })
        .unwrap();
}

fn complete(ledger: &Ledger, run: &str, agent: &str) {
    ledger
        .append(&Event::AgentCompleted {
            run_id: run.into(),
            agent: agent.into(),
            child_run_id: None,
            call_id: None,
        })
        .unwrap();
}

#[test]
fn empty_run_reports_working_solo() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .append(&Event::RunStarted { run_id: "r".into() })
        .unwrap();

    assert_eq!(
        sv::render_swarm(&replay(&ledger, "r"), "run-abcdef123456"),
        "no subagents — this run is working solo"
    );
}

#[test]
fn delegations_render_as_parent_child_tree_with_statuses() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .append(&Event::RunStarted { run_id: "r".into() })
        .unwrap();
    spawn(&ledger, "r", "coder");
    spawn(&ledger, "r", "reader");
    complete(&ledger, "r", "coder");

    let out = sv::render_swarm(&replay(&ledger, "r"), "run-abcdef123456");
    let lines: Vec<&str> = out.lines().collect();
    // Root names the short run id and its running status.
    assert!(lines[0].contains("run-abcd"), "root line: {}", lines[0]);
    assert!(lines[0].contains("running"), "root line: {}", lines[0]);
    assert!(lines[0].contains("2 delegation"), "root line: {}", lines[0]);
    // Children carry their profile label and completion state.
    assert!(out.contains("coder (done)"), "tree:\n{out}");
    assert!(out.contains("reader (running)"), "tree:\n{out}");
    // Tree glyphs: first child branches, last child terminates.
    assert!(out.contains("├─"), "tree:\n{out}");
    assert!(out.contains("└─"), "tree:\n{out}");
}

#[test]
fn respawned_agent_reflects_latest_state() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .append(&Event::RunStarted { run_id: "r".into() })
        .unwrap();
    spawn(&ledger, "r", "coder");
    complete(&ledger, "r", "coder");
    spawn(&ledger, "r", "coder");

    let out = sv::render_swarm(&replay(&ledger, "r"), "r");
    assert!(out.contains("coder (running)"), "tree:\n{out}");
    // One line for the agent, not two.
    assert_eq!(out.lines().filter(|l| l.contains("coder")).count(), 1);
}

#[test]
fn tree_is_capped_at_max_depth() {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .append(&Event::RunStarted { run_id: "r".into() })
        .unwrap();
    for i in 0..7 {
        spawn(&ledger, "r", &format!("agent-{i}"));
    }

    let out = sv::render_swarm(&replay(&ledger, "r"), "r");
    assert!(out.contains("agent-4"), "tree:\n{out}");
    assert!(!out.contains("agent-5"), "tree:\n{out}");
    assert!(out.contains("2 more"), "tree:\n{out}");
}
