//! Partial-work handoff when a child run is stopped.
//!
//! Stopping a sub-agent (user stop, timeout, budget exhaustion) used to
//! discard everything the child had done: the parent only learned
//! `SWARM_CHILD_CANCELED` / `SWARM_CHILD_BUDGET` and had to start over.
//! This module captures what the child completed, what was still pending,
//! and a short transcript excerpt from the child's ledger, and returns it
//! to the parent as a structured handoff so the work can resume instead
//! of restarting.
//!
//! Capture is read-only against the child's ledger: completed tool calls
//! (from `ToolCompleted` + `ToolStarted` args), the pending plan (from the
//! child's todo snapshot), and the last few turn outcomes. Fail-open by
//! construction: a ledger read failure yields an empty handoff with a
//! note, never a new error - a stop that cannot capture still stops.
//!
//! The handoff rides to the parent as a fenced ```partial-handoff JSON
//! block appended to the stop error's cause, mirroring the `child-result`
//! envelope pattern: machine-readable, parsed in Rust should anyone need
//! it, and the error code still says the child did NOT finish.

use crate::Supervisor;
use pantheon_api::events::Event;
use pantheon_api::todo::{TodoItem, TodoStatus};
use pantheon_storage::LedgerEntry;
use std::collections::HashMap;

/// Fence name for the handoff block appended to stop errors.
pub const HANDOFF_FENCE: &str = "partial-handoff";

/// Most completed steps kept (most recent first).
const MAX_COMPLETED_STEPS: usize = 12;
/// Most pending todo items kept.
const MAX_PENDING: usize = 20;
/// Turn outcomes kept for the excerpt.
const MAX_EXCERPT_TURNS: usize = 3;
/// Hard cap on the excerpt's total characters.
const MAX_EXCERPT_CHARS: usize = 2000;
/// Hard cap on one step's detail characters.
const MAX_DETAIL_CHARS: usize = 160;

/// One finished unit of child work: what ran and with what arguments.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CompletedStep {
    pub tool: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// Structured partial work returned when a child run is stopped.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PartialHandoff {
    /// How the child stopped: "canceled" | "budget_exhausted".
    pub stopped: String,
    pub agent: String,
    pub reason: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed: Vec<CompletedStep>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub transcript_excerpt: String,
}

impl PartialHandoff {
    /// Canonical JSON for the parent: stable field order, no extra
    /// whitespace. Empty when nothing was captured (fail-open).
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            "{\"stopped\":\"unknown\",\"summary\":\"serialization failed\"}".to_string()
        })
    }

    /// True when the ledger yielded nothing at all.
    pub fn is_empty(&self) -> bool {
        self.completed.is_empty() && self.pending.is_empty() && self.transcript_excerpt.is_empty()
    }

    fn empty(agent: &str, stopped: &str, reason: &str, note: &str) -> Self {
        Self {
            stopped: stopped.to_string(),
            agent: agent.to_string(),
            reason: reason.to_string(),
            summary: format!("child agent {agent} stopped ({stopped}): {reason}. {note}"),
            completed: Vec::new(),
            pending: Vec::new(),
            transcript_excerpt: String::new(),
        }
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Pure capture from already-read ledger data. No I/O, no failure modes:
///
/// - `entries`: the child run's ledger events in seq order.
/// - `todos`: the child run's latest todo snapshot.
///
/// Completed steps come from `ToolCompleted` events paired with their
/// `ToolStarted` args by call id (most recent last in the vec). The
/// pending plan is every todo not marked completed. The excerpt is the
/// last few `TurnCompleted` outcomes.
pub fn build_handoff(
    entries: &[LedgerEntry],
    todos: &[TodoItem],
    agent: &str,
    stopped: &str,
    reason: &str,
) -> PartialHandoff {
    let mut started_args: HashMap<String, (String, String)> = HashMap::new();
    let mut completed: Vec<CompletedStep> = Vec::new();
    let mut outcomes: Vec<String> = Vec::new();
    let mut turns = 0usize;

    for entry in entries {
        match &entry.event {
            Event::ToolStarted {
                call_id,
                tool,
                args,
                ..
            } => {
                started_args.insert(call_id.clone(), (tool.clone(), args.clone()));
            }
            Event::ToolCompleted { call_id, tool, .. } => {
                let detail = started_args
                    .get(call_id)
                    .map(|(_, args)| truncate_chars(args.trim(), MAX_DETAIL_CHARS))
                    .unwrap_or_default();
                completed.push(CompletedStep {
                    tool: tool.clone(),
                    detail,
                });
            }
            Event::TurnCompleted { outcome, .. } => {
                turns += 1;
                let o = outcome.trim();
                if !o.is_empty() {
                    outcomes.push(o.to_string());
                }
            }
            _ => {}
        }
    }
    // Keep the most recent steps: resume cares about where the child
    // stopped, not where it started.
    if completed.len() > MAX_COMPLETED_STEPS {
        completed.drain(..completed.len() - MAX_COMPLETED_STEPS);
    }
    let mut pending: Vec<String> = todos
        .iter()
        .filter(|t| t.status != TodoStatus::Completed)
        .map(|t| t.content.trim().to_string())
        .filter(|c| !c.is_empty())
        .take(MAX_PENDING)
        .collect();
    // In-progress work reads better first.
    pending.sort_by_key(|c| {
        todos
            .iter()
            .find(|t| t.content.trim() == c)
            .map(|t| match t.status {
                TodoStatus::InProgress => 0,
                _ => 1,
            })
            .unwrap_or(1)
    });

    let excerpt: String = {
        let tail: Vec<&str> = outcomes
            .iter()
            .rev()
            .take(MAX_EXCERPT_TURNS)
            .map(String::as_str)
            .collect();
        let mut joined = tail.into_iter().rev().collect::<Vec<_>>().join("\n---\n");
        if joined.chars().count() > MAX_EXCERPT_CHARS {
            joined = truncate_chars(&joined, MAX_EXCERPT_CHARS);
        }
        joined
    };

    let summary = format!(
        "child agent {agent} stopped ({stopped}): {reason}. \
         Completed {} tool call{} across {} turn{}, {} todo item{} still open.",
        completed.len(),
        if completed.len() == 1 { "" } else { "s" },
        turns,
        if turns == 1 { "" } else { "s" },
        pending.len(),
        if pending.len() == 1 { "" } else { "s" },
    );
    PartialHandoff {
        stopped: stopped.to_string(),
        agent: agent.to_string(),
        reason: reason.to_string(),
        summary,
        completed,
        pending,
        transcript_excerpt: excerpt,
    }
}

/// Capture a stopped child's partial work from its ledger. Fail-open:
/// any read failure returns an empty handoff with a note, so the stop
/// itself is never blocked by capture.
pub fn capture_partial_handoff(
    supervisor: &Supervisor,
    child_run: &str,
    agent: &str,
    stopped: &str,
    reason: &str,
) -> PartialHandoff {
    let entries = match supervisor.replay(child_run) {
        Ok(e) => e,
        Err(e) => {
            return PartialHandoff::empty(
                agent,
                stopped,
                reason,
                &format!("ledger replay failed ({e}); no partial work captured"),
            )
        }
    };
    let todos = supervisor.load_todos(child_run).unwrap_or_default();
    let handoff = build_handoff(&entries, &todos, agent, stopped, reason);
    if handoff.is_empty() {
        PartialHandoff::empty(
            agent,
            stopped,
            reason,
            "the child recorded no tool calls, turns, or todos before stopping",
        )
    } else {
        handoff
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pantheon_api::provenance::{Provenance, TrustTier};
    use pantheon_storage::Ledger;

    fn prov() -> Provenance {
        Provenance {
            trust: TrustTier::Untrusted,
            source: "test".to_string(),
            origin_seq: None,
        }
    }

    fn entry(seq: i64, event: Event) -> LedgerEntry {
        LedgerEntry {
            id: seq,
            run_id: "child-1".to_string(),
            seq,
            ts_ms: seq * 1000,
            event,
        }
    }

    fn sample_entries() -> Vec<LedgerEntry> {
        vec![
            entry(
                1,
                Event::ToolStarted {
                    run_id: "child-1".to_string(),
                    call_id: "call_0_0".to_string(),
                    tool: "read_file".to_string(),
                    args: "{\"path\": \"src/main.rs\"}".to_string(),
                    provenance: prov(),
                },
            ),
            entry(
                2,
                Event::ToolCompleted {
                    run_id: "child-1".to_string(),
                    call_id: "call_0_0".to_string(),
                    tool: "read_file".to_string(),
                    provenance: prov(),
                },
            ),
            entry(
                3,
                Event::TurnCompleted {
                    run_id: "child-1".to_string(),
                    turn_id: "t1".to_string(),
                    outcome: "Read the entry point; the bug is in the router.".to_string(),
                },
            ),
            entry(
                4,
                Event::ToolStarted {
                    run_id: "child-1".to_string(),
                    call_id: "call_1_0".to_string(),
                    tool: "edit_file".to_string(),
                    args: "{\"path\": \"src/router.rs\"}".to_string(),
                    provenance: prov(),
                },
            ),
            entry(
                5,
                Event::ToolCompleted {
                    run_id: "child-1".to_string(),
                    call_id: "call_1_0".to_string(),
                    tool: "edit_file".to_string(),
                    provenance: prov(),
                },
            ),
            entry(
                6,
                Event::TurnCompleted {
                    run_id: "child-1".to_string(),
                    turn_id: "t2".to_string(),
                    outcome: "Fixed the router; tests still to run.".to_string(),
                },
            ),
        ]
    }

    fn sample_todos() -> Vec<TodoItem> {
        vec![
            TodoItem {
                content: "Find the bug".to_string(),
                status: TodoStatus::Completed,
                active_form: None,
            },
            TodoItem {
                content: "Fix the router".to_string(),
                status: TodoStatus::Completed,
                active_form: None,
            },
            TodoItem {
                content: "Run the test suite".to_string(),
                status: TodoStatus::InProgress,
                active_form: Some("Running the test suite".to_string()),
            },
            TodoItem {
                content: "Update the changelog".to_string(),
                status: TodoStatus::Pending,
                active_form: None,
            },
        ]
    }

    #[test]
    fn handoff_captures_completed_pending_and_excerpt() {
        let h = build_handoff(
            &sample_entries(),
            &sample_todos(),
            "coder",
            "canceled",
            "user pressed stop",
        );
        assert_eq!(h.stopped, "canceled");
        assert_eq!(h.agent, "coder");
        assert_eq!(h.completed.len(), 2);
        assert_eq!(h.completed[0].tool, "read_file");
        assert!(h.completed[0].detail.contains("src/main.rs"));
        assert_eq!(h.completed[1].tool, "edit_file");
        // Pending plan: only non-completed todos, in-progress first.
        assert_eq!(h.pending.len(), 2);
        assert_eq!(h.pending[0], "Run the test suite");
        assert_eq!(h.pending[1], "Update the changelog");
        // Excerpt: both turn outcomes, oldest first.
        assert!(h.transcript_excerpt.contains("the bug is in the router"));
        assert!(h.transcript_excerpt.contains("tests still to run"));
        assert!(h.summary.contains("2 tool calls"));
        assert!(h.summary.contains("2 turns"));
        assert!(h.summary.contains("2 todo items still open"));
        // Machine-readable JSON round-trips.
        let back: PartialHandoff = serde_json::from_str(&h.to_json()).unwrap();
        assert_eq!(back, h);
    }

    #[test]
    fn empty_run_yields_empty_handoff_without_panic() {
        let h = build_handoff(&[], &[], "coder", "canceled", "stopped early");
        assert!(h.is_empty());
        assert!(h.completed.is_empty());
        assert!(h.pending.is_empty());
        assert!(h.transcript_excerpt.is_empty());
        // Still valid JSON with the stop reason preserved.
        let v: serde_json::Value = serde_json::from_str(&h.to_json()).unwrap();
        assert_eq!(v["stopped"], "canceled");
        assert_eq!(v["agent"], "coder");
    }

    #[test]
    fn completed_without_started_still_captured() {
        // A ToolCompleted with no matching ToolStarted (e.g. ledger gap)
        // must not drop the step; detail is just empty.
        let entries = vec![entry(
            1,
            Event::ToolCompleted {
                run_id: "child-1".to_string(),
                call_id: "call_missing".to_string(),
                tool: "shell".to_string(),
                provenance: prov(),
            },
        )];
        let h = build_handoff(&entries, &[], "coder", "canceled", "x");
        assert_eq!(h.completed.len(), 1);
        assert_eq!(h.completed[0].tool, "shell");
        assert!(h.completed[0].detail.is_empty());
    }

    #[test]
    fn step_cap_keeps_most_recent() {
        let mut entries = Vec::new();
        for i in 0..20 {
            let call = format!("call_{i}");
            entries.push(entry(
                i * 2,
                Event::ToolStarted {
                    run_id: "child-1".to_string(),
                    call_id: call.clone(),
                    tool: format!("tool_{i}"),
                    args: "{}".to_string(),
                    provenance: prov(),
                },
            ));
            entries.push(entry(
                i * 2 + 1,
                Event::ToolCompleted {
                    run_id: "child-1".to_string(),
                    call_id: call,
                    tool: format!("tool_{i}"),
                    provenance: prov(),
                },
            ));
        }
        let h = build_handoff(&entries, &[], "coder", "canceled", "x");
        assert_eq!(h.completed.len(), MAX_COMPLETED_STEPS);
        assert_eq!(h.completed[0].tool, "tool_8");
        assert_eq!(h.completed[MAX_COMPLETED_STEPS - 1].tool, "tool_19");
    }

    #[test]
    fn capture_from_real_ledger_end_to_end() {
        // The IO wrapper against a real (in-memory) ledger: events and
        // the todo snapshot both flow through.
        let ledger = Ledger::open_in_memory().expect("in-memory ledger");
        for e in sample_entries() {
            ledger.append(&e.event).expect("append");
        }
        ledger
            .set_todos("child-1", &sample_todos())
            .expect("set todos");
        let entries = ledger.replay("child-1").expect("replay");
        let todos = ledger.todos("child-1").expect("todos");
        let h = build_handoff(&entries, &todos, "coder", "budget_exhausted", "token cap");
        assert_eq!(h.stopped, "budget_exhausted");
        assert_eq!(h.completed.len(), 2);
        assert_eq!(h.pending.len(), 2);
        assert!(!h.transcript_excerpt.is_empty());
    }
}
