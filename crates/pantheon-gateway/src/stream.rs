//! Ledger event -> AG-UI frame mapping (interactive path).
//! One frame per UI-meaningful transition, keyed by run_id + thread_id.
//! Approval frames park the run: client answers grant/deny.
use pantheon_core::events::Event;
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiFrameKind {
    Run,
    Text,
    Tool,
    State,
    Approval,
    GenUi,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiFrame {
    pub id: i64,
    pub kind: UiFrameKind,
    pub run_id: String,
    pub thread_id: String,
    pub name: String,
    pub text: String,
    #[serde(default)]
    pub interrupt: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genui: Option<serde_json::Value>,
}
pub fn frame_for_event(
    entry_id: i64,
    thread_id: &str,
    event: &Event,
    genui: Option<serde_json::Value>,
) -> Vec<UiFrame> {
    let mk = |kind: UiFrameKind, name: &str, text: String, interrupt: bool| UiFrame {
        id: entry_id,
        kind,
        run_id: pantheon_storage::ledger::run_id_of(event).to_string(),
        thread_id: thread_id.to_string(),
        name: name.to_string(),
        text,
        interrupt,
        genui: genui.clone(),
    };
    match event {
        Event::RunStarted { .. } => vec![mk(UiFrameKind::Run, "started", String::new(), false)],
        Event::RunProgress { detail, .. } => {
            vec![mk(UiFrameKind::Run, "progress", detail.clone(), false)]
        }
        Event::RunCompleted { .. } => vec![mk(UiFrameKind::Run, "completed", String::new(), false)],
        Event::RunFailed { code, .. } => vec![mk(UiFrameKind::Run, "failed", code.clone(), false)],
        Event::RunCanceled { reason, .. } => {
            vec![mk(UiFrameKind::Run, "canceled", reason.clone(), false)]
        }
        Event::RunRecovered { .. } => vec![mk(UiFrameKind::Run, "recovered", String::new(), false)],
        Event::TurnStarted { turn_id, .. } => vec![mk(
            UiFrameKind::State,
            "turn_started",
            turn_id.clone(),
            false,
        )],
        Event::TurnParked {
            turn_id, reason, ..
        } => vec![mk(
            UiFrameKind::State,
            "turn_parked",
            format!("{turn_id}: {reason}"),
            true,
        )],
        Event::TurnCompleted {
            turn_id, outcome, ..
        } => vec![mk(
            UiFrameKind::State,
            "turn_completed",
            format!("{turn_id}: {outcome}"),
            false,
        )],
        Event::TurnFailed { turn_id, code, .. } => vec![mk(
            UiFrameKind::State,
            "turn_failed",
            format!("{turn_id}: {code}"),
            false,
        )],
        Event::ModelDelta { delta, .. } => {
            vec![mk(UiFrameKind::Text, "delta", delta.clone(), false)]
        }
        Event::AssistantMessage { message, .. } => vec![mk(
            UiFrameKind::Text,
            "message",
            message.content.clone(),
            false,
        )],
        Event::ToolMessage { message, .. } => vec![mk(
            UiFrameKind::Text,
            "result",
            message.content.clone(),
            false,
        )],
        Event::ToolRequested { tool, .. } => {
            vec![mk(UiFrameKind::Tool, "requested", tool.clone(), false)]
        }
        Event::ToolStarted { tool, call_id, .. } => vec![mk(
            UiFrameKind::Tool,
            "started",
            format!("{tool} {call_id}"),
            false,
        )],
        Event::ToolOutput {
            tool, truncated, ..
        } => vec![mk(
            UiFrameKind::Tool,
            "output",
            if *truncated {
                format!("{tool} (compacted)")
            } else {
                tool.clone()
            },
            false,
        )],
        Event::ToolCompleted { tool, call_id, .. } => vec![mk(
            UiFrameKind::Tool,
            "completed",
            format!("{tool} {call_id}"),
            false,
        )],
        Event::ApprovalRequested { scope, .. } => {
            vec![mk(UiFrameKind::Approval, "requested", scope.clone(), true)]
        }
        Event::ApprovalGranted { scope, .. } => {
            vec![mk(UiFrameKind::Approval, "granted", scope.clone(), false)]
        }
        Event::ApprovalDenied { scope, .. } => {
            vec![mk(UiFrameKind::Approval, "denied", scope.clone(), false)]
        }
        _ => vec![],
    }
}
pub fn frames_for_entries(
    entries: &[pantheon_storage::LedgerEntry],
    thread_id: &str,
) -> Vec<UiFrame> {
    let mut out = Vec::new();
    if let Some(first) = entries.first() {
        out.push(UiFrame {
            id: 0,
            kind: UiFrameKind::State,
            run_id: first.run_id.clone(),
            thread_id: thread_id.to_string(),
            name: "snapshot".into(),
            text: format!("{} events", entries.len()),
            interrupt: false,
            genui: None,
        });
    }
    for e in entries {
        out.extend(frame_for_event(e.id, thread_id, &e.event, None));
    }
    out
}
#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
