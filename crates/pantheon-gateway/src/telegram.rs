//! Telegram Channel adapter.

use crate::channel::{
    chunk_text, format_text, Channel, ChannelEnvelope, ChannelError, ChannelEvent,
};
use crate::stream::{UiFrame, UiFrameKind};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

pub const TELEGRAM_MESSAGE_LIMIT: usize = 4_096;

/// Bot API boundary. A live implementation can use any HTTP client; the
/// adapter only owns Telegram's inline keyboard and message limit semantics.
pub trait TelegramTransport: Send + Sync {
    fn send_message(&self, chat_id: &str, payload: &Value) -> Result<(), ChannelError>;
    /// Long-poll `getUpdates`. Returns raw update objects; normalization is
    /// shared with the webhook path via `parse_event`.
    fn get_updates(&self, offset: i64, timeout_secs: u64) -> Result<Vec<Value>, ChannelError> {
        let _ = (offset, timeout_secs);
        Ok(Vec::new())
    }
    fn poll_events(&self) -> Vec<ChannelEvent> {
        Vec::new()
    }
}

/// Live Telegram Bot API transport. Updates are normalized by a webhook or
/// long-poll bridge and fed to `TelegramChannel::push_inbound`.
pub struct TelegramRestTransport {
    agent: ureq::Agent,
    api_base: String,
    bot_token: String,
}

impl TelegramRestTransport {
    pub fn new(bot_token: impl Into<String>) -> Self {
        Self {
            agent: ureq::Agent::new(),
            api_base: "https://api.telegram.org".into(),
            bot_token: bot_token.into(),
        }
    }
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into().trim_end_matches('/').to_string();
        self
    }
}

impl TelegramTransport for TelegramRestTransport {
    fn send_message(&self, chat_id: &str, payload: &Value) -> Result<(), ChannelError> {
        let url = format!("{}/bot{}/sendMessage", self.api_base, self.bot_token);
        let mut body = payload.clone();
        if let Some(object) = body.as_object_mut() {
            object.insert("chat_id".into(), json!(chat_id));
        }
        self.agent
            .post(&url)
            .send_json(body)
            .map_err(|e| ChannelError::from_ureq("TELEGRAM_HTTP", e))?;
        Ok(())
    }
    fn get_updates(&self, offset: i64, timeout_secs: u64) -> Result<Vec<Value>, ChannelError> {
        let url = format!("{}/bot{}/getUpdates", self.api_base, self.bot_token);
        let body: Value = self
            .agent
            .post(&url)
            .timeout(std::time::Duration::from_secs(timeout_secs + 5))
            .send_json(json!({
                "offset": offset,
                "timeout": timeout_secs,
                "allowed_updates": ["message", "callback_query"],
            }))
            .map_err(|e| ChannelError::from_ureq("TELEGRAM_HTTP", e))?
            .into_json()
            .map_err(|e| ChannelError::new("TELEGRAM_HTTP", e.to_string()))?;
        if body.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(ChannelError::new(
                "TELEGRAM_HTTP",
                format!("getUpdates not ok: {body}"),
            ));
        }
        Ok(body
            .get("result")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }
}

/// Normalize a Telegram Bot API update (message or callback query) into the
/// shared channel event shape.
pub fn parse_event(payload: &Value) -> Result<Option<ChannelEvent>, ChannelError> {
    if let Some(message) = payload.get("message") {
        let chat_id = message
            .get("chat")
            .and_then(|chat| chat.get("id"))
            .map(chat_id_string)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| ChannelError::new("TELEGRAM_EVENT", "message has no chat id"))?;
        let text = message.get("text").and_then(Value::as_str).unwrap_or("");
        let sender = message
            .get("from")
            .and_then(|from| from.get("id"))
            .and_then(Value::as_i64)
            .map(|id| id.to_string());
        return Ok(Some(ChannelEvent {
            thread_id: chat_id,
            run_id: None,
            text: text.to_string(),
            approval: None,
            scope: None,
            sender,
        }));
    }
    if let Some(callback) = payload.get("callback_query") {
        let data = callback
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| ChannelError::new("TELEGRAM_EVENT", "callback has no data"))?;
        let (answer, scope) = data
            .split_once(':')
            .ok_or_else(|| ChannelError::new("TELEGRAM_EVENT", "invalid approval callback_data"))?;
        let approval = match answer {
            "grant" => Some(crate::channel::ApprovalAnswer::Grant),
            "deny" => Some(crate::channel::ApprovalAnswer::Deny),
            _ => {
                return Err(ChannelError::new(
                    "TELEGRAM_EVENT",
                    "unknown approval answer",
                ))
            }
        };
        if scope.is_empty() {
            return Err(ChannelError::new(
                "TELEGRAM_EVENT",
                "approval scope is empty",
            ));
        }
        let chat_id = callback
            .get("message")
            .and_then(|message| message.get("chat"))
            .and_then(|chat| chat.get("id"))
            .map(chat_id_string)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| ChannelError::new("TELEGRAM_EVENT", "callback has no chat id"))?;
        let sender = callback
            .get("from")
            .and_then(|from| from.get("id"))
            .and_then(Value::as_i64)
            .map(|id| id.to_string());
        return Ok(Some(ChannelEvent {
            thread_id: chat_id,
            run_id: None,
            text: String::new(),
            approval,
            scope: Some(scope.to_string()),
            sender,
        }));
    }
    Ok(None)
}

fn chat_id_string(value: &Value) -> String {
    value
        .as_i64()
        .map(|id| id.to_string())
        .or_else(|| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

pub struct TelegramChannel {
    pub bot_token: String,
    transport: Arc<dyn TelegramTransport>,
    inbox: Mutex<Vec<ChannelEvent>>,
}

impl TelegramChannel {
    pub fn new(bot_token: impl Into<String>, transport: Arc<dyn TelegramTransport>) -> Self {
        Self {
            bot_token: bot_token.into(),
            transport,
            inbox: Mutex::new(Vec::new()),
        }
    }
    pub fn rest(bot_token: impl Into<String>) -> Self {
        let bot_token = bot_token.into();
        let transport = Arc::new(TelegramRestTransport::new(bot_token.clone()));
        Self::new(bot_token, transport)
    }
    pub fn push_inbound(&self, event: ChannelEvent) {
        self.inbox.lock().unwrap().push(event);
    }

    fn payload(frame: &UiFrame, text: String) -> Value {
        if frame.kind == UiFrameKind::Approval && frame.name == "requested" {
            json!({
                "text": text,
                "reply_markup": {"inline_keyboard": [[
                    {"text": "Grant", "callback_data": format!("grant:{}", frame.text)},
                    {"text": "Deny", "callback_data": format!("deny:{}", frame.text)}
                ]]}
            })
        } else {
            json!({ "text": text })
        }
    }

    pub fn payloads(&self, envelope: &ChannelEnvelope) -> Vec<Value> {
        let mut payloads = chunk_text(&format_text(&envelope.frame), TELEGRAM_MESSAGE_LIMIT)
            .into_iter()
            .map(|part| Self::payload(&envelope.frame, part))
            .collect::<Vec<_>>();
        if payloads.is_empty() {
            payloads.push(json!({"text": ""}));
        }
        payloads
    }
}

impl Channel for TelegramChannel {
    fn name(&self) -> &str {
        "telegram"
    }
    fn send(&self, envelope: ChannelEnvelope) -> Result<(), ChannelError> {
        for payload in self.payloads(&envelope) {
            self.transport.send_message(&envelope.thread_id, &payload)?;
        }
        Ok(())
    }
    fn poll(&self) -> Vec<ChannelEvent> {
        let mut events = std::mem::take(&mut *self.inbox.lock().unwrap());
        events.extend(self.transport.poll_events());
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{UiFrame, UiFrameKind};
    use std::sync::Mutex;
    struct Recorder(Mutex<Vec<(String, Value)>>);
    impl TelegramTransport for Recorder {
        fn send_message(&self, chat: &str, payload: &Value) -> Result<(), ChannelError> {
            self.0.lock().unwrap().push((chat.into(), payload.clone()));
            Ok(())
        }
    }
    #[test]
    fn parses_telegram_approval_callbacks() {
        let event = parse_event(&json!({
            "callback_query": {
                "data": "grant:call_2_0",
                "message": {"chat": {"id": 42}}
            }
        }))
        .unwrap()
        .unwrap();
        assert_eq!(event.thread_id, "42");
        assert_eq!(event.approval, Some(crate::channel::ApprovalAnswer::Grant));
        assert_eq!(event.scope.as_deref(), Some("call_2_0"));
        assert!(parse_event(&json!({"callback_query": {"data": "no-scope"}})).is_err());
    }

    #[test]
    fn approval_uses_inline_keyboard() {
        let api = Arc::new(Recorder(Mutex::new(vec![])));
        let c = TelegramChannel::new("token", api.clone());
        c.send(ChannelEnvelope {
            thread_id: "42".into(),
            frame: UiFrame {
                id: 1,
                kind: UiFrameKind::Approval,
                run_id: "r".into(),
                thread_id: "42".into(),
                name: "requested".into(),
                text: "scope".into(),
                interrupt: true,
                genui: None,
            },
        })
        .unwrap();
        let rows = api.0.lock().unwrap();
        assert_eq!(
            rows[0].1["reply_markup"]["inline_keyboard"][0][0]["callback_data"],
            "grant:scope"
        );
    }
}
