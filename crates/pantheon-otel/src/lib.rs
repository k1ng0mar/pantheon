//! Observability (spec section 19): runtime events -> OTel-shaped records,
//! plus the offline explain path that never needs a collector.
//!
//! Mapping rule: one span per run, one child span per tool/agent/model
//! transition, metrics derived from the same events. The durable ledger
//! stays the source of truth; OTel is the export format.
use pantheon_core::events::Event;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpanKind { Run, Tool, Model, Agent, Approval, Memory }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpanRecord {
    pub run_id: String,
    pub name: String,
    pub kind: SpanKind,
    pub status: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metrics {
    pub runs_started: u64,
    pub runs_completed: u64,
    pub runs_failed: u64,
    pub tool_calls: u64,
    pub model_turns: u64,
    pub approvals_requested: u64,
    pub agents_spawned: u64,
}

/// Translate one event into a span (when it starts/finishes work) or None.
pub fn span_for(ev: &Event) -> Option<SpanRecord> {
    let mk = |run: &str, name: String, kind: SpanKind, status: &str| Some(SpanRecord {
        run_id: run.to_string(), name, kind, status: status.to_string() });
    match ev {
        Event::RunStarted { run_id } => mk(run_id, "run".into(), SpanKind::Run, "started"),
        Event::RunCompleted { run_id } => mk(run_id, "run".into(), SpanKind::Run, "ok"),
        Event::RunFailed { run_id, code } => mk(run_id, format!("run:{code}"), SpanKind::Run, "error"),
        Event::ToolStarted { run_id, tool } => mk(run_id, format!("tool:{tool}"), SpanKind::Tool, "started"),
        Event::ToolCompleted { run_id, tool } => mk(run_id, format!("tool:{tool}"), SpanKind::Tool, "ok"),
        Event::ModelRequested { run_id, model } => mk(run_id, format!("model:{model}"), SpanKind::Model, "started"),
        Event::ModelCompleted { run_id } => mk(run_id, "model".into(), SpanKind::Model, "ok"),
        Event::AgentSpawned { run_id, agent } => mk(run_id, format!("agent:{agent}"), SpanKind::Agent, "started"),
        Event::AgentCompleted { run_id, agent } => mk(run_id, format!("agent:{agent}"), SpanKind::Agent, "ok"),
        Event::ApprovalRequested { run_id, scope } => mk(run_id, format!("approval:{scope}"), SpanKind::Approval, "waiting"),
        Event::ApprovalGranted { run_id, scope } => mk(run_id, format!("approval:{scope}"), SpanKind::Approval, "granted"),
        Event::MemoryProposed { run_id } => mk(run_id, "memory.propose".into(), SpanKind::Memory, "proposed"),
        Event::RunProgress { .. } | Event::RunRecovered { .. }
        | Event::ModelDelta { .. } | Event::ToolRequested { .. }
        | Event::ToolOutput { .. } | Event::AgentMessage { .. } => None,
    }
}

/// Fold a replay into counters (metrics export).
pub fn metrics_from(events: &[Event]) -> Metrics {
    let mut m = Metrics::default();
    for e in events {
        match e {
            Event::RunStarted { .. } => m.runs_started += 1,
            Event::RunCompleted { .. } => m.runs_completed += 1,
            Event::RunFailed { .. } => m.runs_failed += 1,
            Event::ToolStarted { .. } => m.tool_calls += 1,
            Event::ModelCompleted { .. } => m.model_turns += 1,
            Event::ApprovalRequested { .. } => m.approvals_requested += 1,
            Event::AgentSpawned { .. } => m.agents_spawned += 1,
            _ => {}
        }
    }
    m
}

/// Offline explain: works with no collector attached.
pub fn explain(events: &[Event]) -> String {
    let m = metrics_from(events);
    format!(
        "runs {} started / {} completed / {} failed; {} tool calls; {} model turns; {} approvals; {} sub-agents",
        m.runs_started, m.runs_completed, m.runs_failed, m.tool_calls,
        m.model_turns, m.approvals_requested, m.agents_spawned)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tool_events_become_tool_spans() {
        let s = span_for(&Event::ToolStarted { run_id: "r".into(), tool: "shell".into() })
            .unwrap();
        assert_eq!(s.kind, SpanKind::Tool);
        assert_eq!(s.name, "tool:shell");
    }
    #[test]
    fn deltas_are_not_spans() {
        assert!(span_for(&Event::ModelDelta { run_id: "r".into(), delta: "x".into() }).is_none());
    }
    #[test]
    fn metrics_fold_over_replay() {
        let evs = vec![
            Event::RunStarted { run_id: "r".into() },
            Event::ToolStarted { run_id: "r".into(), tool: "a".into() },
            Event::ModelCompleted { run_id: "r".into() },
            Event::RunCompleted { run_id: "r".into() },
        ];
        let m = metrics_from(&evs);
        assert_eq!((m.runs_started, m.runs_completed, m.tool_calls, m.model_turns), (1, 1, 1, 1));
        assert!(explain(&evs).contains("1 completed"));
    }
}
