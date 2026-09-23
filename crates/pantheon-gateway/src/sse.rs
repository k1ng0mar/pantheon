//! SSE encoding for UI frames (std-only, no HTTP crate).
//! `SseEncoder` turns frames into `text/event-stream` bytes; any TCP/HTTP
//! shim writes them. Resume via `Last-Event-ID` (ledger row id).
use crate::stream::UiFrame;
/// Encode one frame as a single SSE `data:` event with `id:` = ledger row.
/// `event:` carries the kind so clients switch without parsing JSON first.
pub fn encode_frame(frame: &UiFrame) -> String {
    let kind = match frame.kind {
        crate::stream::UiFrameKind::Run => "run",
        crate::stream::UiFrameKind::Text => "text",
        crate::stream::UiFrameKind::Tool => "tool",
        crate::stream::UiFrameKind::State => "state",
        crate::stream::UiFrameKind::Approval => "approval",
        crate::stream::UiFrameKind::GenUi => "genui",
    };
    let data = serde_json::to_string(frame).unwrap_or_else(|_| "{}".into());
    format!("id: {}\nevent: {kind}\ndata: {data}\n\n", frame.id)
}
/// Encode many frames in order.
pub fn encode_frames(frames: &[UiFrame]) -> String {
    frames.iter().map(encode_frame).collect()
}
/// Parse a `Last-Event-ID` header value into a ledger row id.
pub fn parse_last_event_id(value: &str) -> Option<i64> {
    value.trim().parse().ok()
}
/// Minimal SSE response head for a hand-rolled HTTP shim.
pub fn response_head() -> String {
    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\nX-Accel-Buffering: no\r\n\r\n".to_string()
}
/// Stateless encoder handle (keeps the call sites readable).
#[derive(Debug, Default, Clone, Copy)]
pub struct SseEncoder;
impl SseEncoder {
    pub fn frame(&self, frame: &UiFrame) -> String {
        encode_frame(frame)
    }
    pub fn frames(&self, frames: &[UiFrame]) -> String {
        encode_frames(frames)
    }
    pub fn head(&self) -> String {
        response_head()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{UiFrame, UiFrameKind};
    fn f() -> UiFrame {
        UiFrame {
            id: 9,
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
    fn frame_encodes_with_id_and_kind() {
        let s = encode_frame(&f());
        assert!(s.starts_with("id: 9\n"), "{s}");
        assert!(s.contains("event: text\n"), "{s}");
        assert!(s.contains("\"run_id\":\"r\""), "{s}");
        assert!(s.ends_with("\n\n"));
    }
    #[test]
    fn last_event_id_parses() {
        assert_eq!(parse_last_event_id("42"), Some(42));
        assert_eq!(parse_last_event_id("nope"), None);
    }
}
