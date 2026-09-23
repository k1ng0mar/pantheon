//! Channel transport seam (interactive path).
//! One trait every surface implements: discord/slack/web/CLI consume the
//! same `UiFrame` stream and answer approvals through the same call.
//! Discord formatting (code fences, chunking) lives in the discord adapter,
//! not here: this seam only moves frames and approval decisions.
use crate::stream::UiFrame;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
/// Approval answer from a user/surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalAnswer {
    Grant,
    Deny,
}
/// What a surface hands the runtime (normalized inbound).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelEvent {
    pub thread_id: String,
    pub run_id: Option<String>,
    pub text: String,
    pub approval: Option<ApprovalAnswer>,
    pub scope: Option<String>,
}
/// What the runtime hands a surface (one frame + routing).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelEnvelope {
    pub thread_id: String,
    pub frame: UiFrame,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelError {
    pub code: String,
    pub message: String,
}
impl ChannelError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}
impl std::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}
impl std::error::Error for ChannelError {}
/// Transport seam. Blocking + object-safe so web/discord/slack/CLI share it.
pub trait Channel: Send + Sync {
    fn name(&self) -> &str;
    fn send(&self, envelope: ChannelEnvelope) -> Result<(), ChannelError>;
    fn poll(&self) -> Vec<ChannelEvent>;
}

/// Fan-out: one frame to every attached surface. A failing surface does not
/// break the others; errors come back per channel for the caller to log.
pub fn fanout(
    channels: &[&dyn Channel],
    envelope: &ChannelEnvelope,
) -> Vec<(String, Result<(), ChannelError>)> {
    channels
        .iter()
        .map(|c| (c.name().to_string(), c.send(envelope.clone())))
        .collect()
}

/// Split text at character boundaries without breaking UTF-8. Adapters use
/// this for their platform limits; it also prevents a large model response
/// from becoming one un-sendable message.
pub fn chunk_text(text: &str, max_chars: usize) -> Vec<String> {
    if max_chars == 0 {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + max_chars).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = (start + 1).min(text.len());
        }
        out.push(text[start..end].to_string());
        start = end;
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Buttons carried by approval frames. The custom IDs are stable protocol
/// values so a transport can map a click back to the original scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalButtons {
    pub scope: String,
}

/// Render one frame as plain text for surfaces without rich UI
/// (discord/slack code fences and chunking live in the adapters; this is
/// the shared fallback shape).
pub fn format_text(frame: &UiFrame) -> String {
    match frame.kind {
        crate::stream::UiFrameKind::Run => {
            if frame.name == "progress" && !frame.text.is_empty() {
                frame.text.clone()
            } else {
                format!("[run {}]", frame.name)
            }
        }
        crate::stream::UiFrameKind::Text => frame.text.clone(),
        crate::stream::UiFrameKind::Tool => format!("`{}` {}", frame.name, frame.text),
        crate::stream::UiFrameKind::State => frame.text.clone(),
        crate::stream::UiFrameKind::Approval => {
            if frame.name == "requested" {
                format!("approval needed: `{}` — reply grant/deny", frame.text)
            } else if frame.name == "denied" {
                format!("approval denied: `{}`", frame.text)
            } else {
                format!("approval granted: `{}`", frame.text)
            }
        }
        crate::stream::UiFrameKind::GenUi => frame
            .genui
            .as_ref()
            .and_then(|g| g.get("url"))
            .and_then(|u| u.as_str())
            .map(|u| u.to_string())
            .unwrap_or_default(),
    }
}
/// Thread -> run affinity so one SSE stream multiplexes many runs.
#[derive(Debug, Default)]
pub struct ThreadRunMap {
    map: HashMap<String, String>,
}
impl ThreadRunMap {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn bind(&mut self, thread_id: &str, run_id: &str) {
        self.map.insert(thread_id.to_string(), run_id.to_string());
    }
    pub fn run_for(&self, thread_id: &str) -> Option<&str> {
        self.map.get(thread_id).map(|s| s.as_str())
    }
    pub fn thread_for_run(&self, run_id: &str) -> Option<&str> {
        self.map
            .iter()
            .find(|(_, r)| r.as_str() == run_id)
            .map(|(t, _)| t.as_str())
    }
    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}
/// In-memory channel: tests, CLI streaming, and the web shim's outbox.
#[derive(Debug, Default)]
pub struct MemoryChannel {
    pub name: String,
    pub outbox: std::sync::Mutex<Vec<ChannelEnvelope>>,
    pub inbox: std::sync::Mutex<Vec<ChannelEvent>>,
}
impl MemoryChannel {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            outbox: Default::default(),
            inbox: Default::default(),
        }
    }
    pub fn push_inbound(&self, ev: ChannelEvent) {
        self.inbox.lock().unwrap().push(ev);
    }
    pub fn drain_outbound(&self) -> Vec<ChannelEnvelope> {
        std::mem::take(&mut *self.outbox.lock().unwrap())
    }
}
impl Channel for MemoryChannel {
    fn name(&self) -> &str {
        &self.name
    }
    fn send(&self, envelope: ChannelEnvelope) -> Result<(), ChannelError> {
        self.outbox.lock().unwrap().push(envelope);
        Ok(())
    }
    fn poll(&self) -> Vec<ChannelEvent> {
        std::mem::take(&mut *self.inbox.lock().unwrap())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{UiFrame, UiFrameKind};
    fn frame() -> UiFrame {
        UiFrame {
            id: 1,
            kind: UiFrameKind::Text,
            run_id: "r".into(),
            thread_id: "t".into(),
            name: "delta".into(),
            text: "hi".into(),
            interrupt: false,
            genui: None,
        }
    }
    #[test]
    fn memory_channel_round_trips() {
        let c = MemoryChannel::new("web");
        c.send(ChannelEnvelope {
            thread_id: "t".into(),
            frame: frame(),
        })
        .unwrap();
        assert_eq!(c.drain_outbound().len(), 1);
        c.push_inbound(ChannelEvent {
            thread_id: "t".into(),
            run_id: None,
            text: "go".into(),
            approval: None,
            scope: None,
        });
        assert_eq!(c.poll().len(), 1);
        assert!(c.poll().is_empty());
    }
    #[test]
    fn thread_run_map_resolves_both_ways() {
        let mut m = ThreadRunMap::new();
        m.bind("discord:c1", "run-1");
        assert_eq!(m.run_for("discord:c1"), Some("run-1"));
        assert_eq!(m.thread_for_run("run-1"), Some("discord:c1"));
    }
}
