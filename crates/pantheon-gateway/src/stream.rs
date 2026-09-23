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
        Event::RunRecovered { .. } => vec![mk(UiFrameKind::Run, "recovered", String::new(), false)],
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
mod tests {
    use super::*;
    use pantheon_core::events::Event;
    #[test]
    fn approval_request_parks_with_interrupt_flag() {
        let f = frame_for_event(
            7,
            "discord:c1",
            &Event::ApprovalRequested {
                run_id: "r1".into(),
                scope: "call_0_0".into(),
            },
            None,
        );
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].kind, UiFrameKind::Approval);
        assert!(f[0].interrupt);
        assert_eq!(f[0].thread_id, "discord:c1");
        assert_eq!(f[0].id, 7);
    }
    #[test]
    fn text_and_tool_events_become_frames() {
        let d = frame_for_event(
            1,
            "t",
            &Event::ModelDelta {
                run_id: "r".into(),
                delta: "hi".into(),
            },
            None,
        );
        assert_eq!(d[0].kind, UiFrameKind::Text);
        assert!(!d[0].interrupt);
        let s = frame_for_event(
            2,
            "t",
            &Event::ToolStarted {
                run_id: "r".into(),
                call_id: "call_0_0".into(),
                tool: "shell".into(),
                args: String::new(),
            },
            None,
        );
        assert_eq!(s[0].kind, UiFrameKind::Tool);
    }
    #[test]
    fn provider_internals_stay_off_the_wire() {
        let f = frame_for_event(
            3,
            "t",
            &Event::ModelRequested {
                run_id: "r".into(),
                model: "m".into(),
            },
            None,
        );
        assert!(f.is_empty());
    }
}
