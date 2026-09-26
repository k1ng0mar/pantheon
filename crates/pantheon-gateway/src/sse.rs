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
#[path = "sse_tests.rs"]
mod tests;
