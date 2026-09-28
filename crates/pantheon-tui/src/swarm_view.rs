//! `/swarm` — an honest snapshot of this session's delegation tree.
//!
//! The ledger records `AgentSpawned { run_id, agent }` on the parent run and
//! `AgentCompleted { run_id, agent }` when a delegation finishes. What the
//! ledger does NOT record is a child run id or an explicit parent link, so a
//! true nested tree is not reconstructible: `/swarm` renders the root plus
//! its direct delegations, capped at a fixed depth, and says so plainly
//! rather than fabricating relationships.
//!
//! The empty case renders exactly: `no subagents — this run is working solo`.

use pantheon_api::events::Event;
use pantheon_storage::LedgerEntry;
use std::sync::Arc;

use crate::session::TuiState;
use pantheon_runtime::session::Session;

/// Maximum rendered depth of the delegation tree.
pub const MAX_DEPTH: usize = 5;

/// One delegation seen in the ledger: profile name + completion state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delegation {
    pub agent: String,
    /// True once an `AgentCompleted` for this agent followed its spawn.
    pub done: bool,
}

/// Direct delegations of this run, in spawn order.
///
/// An agent that was spawned more than once appears once, reflecting its
/// latest state (done if any completion followed its latest spawn).
pub fn delegations(entries: &[LedgerEntry]) -> Vec<Delegation> {
    let mut out: Vec<Delegation> = Vec::new();
    for e in entries {
        match &e.event {
            Event::AgentSpawned { agent, .. } => {
                if let Some(d) = out.iter_mut().find(|d| d.agent == *agent) {
                    d.done = false;
                } else {
                    out.push(Delegation {
                        agent: agent.clone(),
                        done: false,
                    });
                }
            }
            Event::AgentCompleted { agent, .. } => {
                if let Some(d) = out.iter_mut().find(|d| d.agent == *agent) {
                    d.done = true;
                }
            }
            _ => {}
        }
    }
    out
}

/// Render the delegation tree as plain text, one node per line.
///
/// The ledger carries no child run ids and no parent links, so only the
/// root and its direct delegations are rendered; deeper nesting is not
/// shown because it is not durably recorded.
pub fn render_swarm(entries: &[LedgerEntry], run_id: &str) -> String {
    let dels = delegations(entries);
    if dels.is_empty() {
        return "no subagents — this run is working solo".to_string();
    }
    let short = run_id.chars().take(8).collect::<String>();
    let mut lines = vec![format!(
        "swarm {short} (running) — {} delegation{}",
        dels.len(),
        if dels.len() == 1 { "" } else { "s" }
    )];
    for (i, d) in dels.iter().enumerate().take(MAX_DEPTH) {
        let last = i + 1 == dels.len().min(MAX_DEPTH);
        let branch = if last { "└─" } else { "├─" };
        let status = if d.done { "done" } else { "running" };
        lines.push(format!("{branch} {} ({status})", d.agent));
    }
    if dels.len() > MAX_DEPTH {
        lines.push(format!(
            "… {} more (depth cap {})",
            dels.len() - MAX_DEPTH,
            MAX_DEPTH
        ));
    }
    lines.join("\n")
}

/// `/swarm` — show this session's delegation tree.
pub fn cmd_swarm(state: &mut TuiState, session: &Arc<Session>) {
    let entries = match session.supervisor.replay(&state.session_id) {
        Ok(e) => e,
        Err(e) => {
            state.add_status(format!("ledger replay failed: {e}"));
            return;
        }
    };
    for line in render_swarm(&entries, &state.session_id).lines() {
        state.add_status(line.to_string());
    }
}
