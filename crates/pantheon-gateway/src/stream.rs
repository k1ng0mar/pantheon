//! Ledger event -> AG-UI frame mapping (interactive path).
//! One frame per UI-meaningful transition, keyed by run_id + thread_id.
//! Approval frames park the run: client answers grant/deny.
use pantheon_api::events::Event;
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
    /// Structured UI component (`Event::UiContent`/`UiUpdated`). The
    /// document rides `component` as JSON; `text` carries a one-line
    /// summary for clients that only read text.
    Component,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<serde_json::Value>,
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
        component: None,
    };
    let mk_component = |name: &str,
                        ui_id: &str,
                        revision: u64,
                        document: &pantheon_api::ui::UiDocument,
                        interrupt: bool| UiFrame {
        id: entry_id,
        kind: UiFrameKind::Component,
        run_id: pantheon_storage::ledger::run_id_of(event).to_string(),
        thread_id: thread_id.to_string(),
        name: name.to_string(),
        text: format!(
            "ui:{ui_id} rev {revision} ({})",
            document.root.component.tag()
        ),
        interrupt,
        genui: None,
        component: serde_json::to_value(document).ok(),
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
        // Approval scopes are `call_id:tool:args` and ride out to
        // gateway clients verbatim here; the dashboard redacts them
        // (runs.rs timeline/detail), so do the same on this surface.
        Event::ApprovalRequested { scope, .. } => vec![mk(
            UiFrameKind::Approval,
            "requested",
            pantheon_api::logging::redact(scope),
            true,
        )],
        Event::ApprovalGranted { scope, .. } => vec![mk(
            UiFrameKind::Approval,
            "granted",
            pantheon_api::logging::redact(scope),
            false,
        )],
        Event::ApprovalDenied { scope, .. } => vec![mk(
            UiFrameKind::Approval,
            "denied",
            pantheon_api::logging::redact(scope),
            false,
        )],
        Event::UiContent {
            ui_id, document, ..
        } => vec![mk_component("ui_content", ui_id, 0, document, false)],
        Event::UiUpdated {
            ui_id,
            revision,
            document,
            ..
        } => vec![mk_component(
            "ui_updated",
            ui_id,
            *revision,
            document,
            false,
        )],
        Event::UiActionRequested { ui_id, .. } => vec![mk(
            UiFrameKind::State,
            "ui_action_requested",
            ui_id.clone(),
            true,
        )],
        Event::UiActionSubmitted {
            ui_id, action_id, ..
        } => vec![mk(
            UiFrameKind::State,
            "ui_action_submitted",
            format!("{ui_id}#{action_id}"),
            false,
        )],
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
            component: None,
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
    use pantheon_api::ui::{Markdown, UiDocument, UiNode};

    fn doc() -> UiDocument {
        UiDocument {
            version: pantheon_api::ui::UI_PROTOCOL_VERSION,
            id: "t-ui_0".to_string(),
            root: UiNode {
                id: "root".to_string(),
                component: pantheon_api::ui::Component::Markdown(Markdown {
                    title: None,
                    text: "hi".to_string(),
                }),
            },
        }
    }

    #[test]
    fn ui_content_produces_component_frame() {
        let frames = frame_for_event(
            7,
            "thread1",
            &Event::UiContent {
                run_id: "r1".to_string(),
                turn_id: "t".to_string(),
                ui_id: "t-ui_0".to_string(),
                document: doc(),
            },
            None,
        );
        assert_eq!(frames.len(), 1);
        let f = &frames[0];
        assert_eq!(f.kind, UiFrameKind::Component);
        assert_eq!(f.name, "ui_content");
        assert_eq!(f.id, 7);
        assert!(f.text.contains("t-ui_0"), "{}", f.text);
        let comp = f
            .component
            .as_ref()
            .expect("component frame carries component payload");
        assert_eq!(comp["id"], "t-ui_0");
        assert_eq!(comp["root"]["type"], "markdown");
    }

    #[test]
    fn ui_updated_produces_component_frame_with_revision() {
        let frames = frame_for_event(
            8,
            "thread1",
            &Event::UiUpdated {
                run_id: "r1".to_string(),
                ui_id: "t-ui_0".to_string(),
                revision: 3,
                document: doc(),
            },
            None,
        );
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].kind, UiFrameKind::Component);
        assert_eq!(frames[0].name, "ui_updated");
        assert!(frames[0].text.contains("rev 3"), "{}", frames[0].text);
    }

    #[test]
    fn ui_action_events_produce_interrupt_state_frames() {
        let req = frame_for_event(
            9,
            "thread1",
            &Event::UiActionRequested {
                run_id: "r1".to_string(),
                ui_id: "t-ui_0".to_string(),
                call_id: "c1".to_string(),
            },
            None,
        );
        assert_eq!(req.len(), 1);
        assert_eq!(req[0].kind, UiFrameKind::State);
        assert!(req[0].interrupt, "action requests interrupt the stream");
        assert!(req[0].component.is_none());

        let sub = frame_for_event(
            10,
            "thread1",
            &Event::UiActionSubmitted {
                run_id: "r1".to_string(),
                ui_id: "t-ui_0".to_string(),
                revision: 0,
                node_id: "root".to_string(),
                action_id: "go".to_string(),
                nonce: "n1".to_string(),
            },
            None,
        );
        assert_eq!(sub.len(), 1);
        assert!(!sub[0].interrupt);
        // Params are never on the wire frame (they were never in the event).
        assert!(!sub[0].text.contains("token"));
    }

    #[test]
    fn component_frame_serializes_for_sse() {
        let frames = frame_for_event(
            11,
            "thread1",
            &Event::UiContent {
                run_id: "r1".to_string(),
                turn_id: "t".to_string(),
                ui_id: "t-ui_0".to_string(),
                document: doc(),
            },
            None,
        );
        let encoded = crate::sse::encode_frame(&frames[0]);
        assert!(encoded.contains("event: component"), "{encoded}");
        assert!(encoded.contains("t-ui_0"), "{encoded}");
    }
}
