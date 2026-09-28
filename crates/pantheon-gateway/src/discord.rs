//! Discord Channel adapter.
//!
//! The adapter owns Discord's wire formatting and limit handling, while the
//! transport seam owns the actual HTTP client. This keeps the channel usable
//! with the std-only server, a gateway daemon, or a test recorder without
//! coupling the runtime to a Discord SDK.

use crate::channel::{
    chunk_text, format_text, Channel, ChannelEnvelope, ChannelError, ChannelEvent,
};
use crate::stream::{UiFrame, UiFrameKind};
use serde_json::{json, Value};
use std::fmt;
use std::sync::{Arc, Mutex};

pub const DISCORD_CONTENT_LIMIT: usize = 2_000;

/// Minimal API boundary implemented by a live Discord REST client or a test
/// recorder. `send_message` receives a complete channel message payload.
pub trait DiscordTransport: Send + Sync {
    fn send_message(&self, channel_id: &str, payload: &Value) -> Result<(), ChannelError>;
    fn poll_events(&self) -> Vec<ChannelEvent> {
        Vec::new()
    }
}

/// Live Discord REST transport. Gateway updates can be fed to
/// `DiscordChannel::push_inbound`; REST is used for the outbound path and
/// retry policy remains the caller's concern.
pub struct DiscordRestTransport {
    agent: ureq::Agent,
    api_base: String,
    token: String,
}

impl DiscordRestTransport {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            agent: ureq::Agent::new(),
            api_base: "https://discord.com/api/v10".into(),
            token: token.into(),
        }
    }
    pub fn with_api_base(mut self, base: impl Into<String>) -> Self {
        self.api_base = base.into().trim_end_matches('/').to_string();
        self
    }

    /// Build the POST request for one message: `(url, authorization header
    /// value, body)`. Pure: unit tests assert the wire shape
    /// (`POST {api_base}/channels/{id}/messages`, `Authorization: Bot
    /// <token>`, `{"content": ...}`) without touching the network.
    pub fn build_send_request(&self, channel_id: &str, payload: &Value) -> (String, String, Value) {
        let url = format!("{}/channels/{}/messages", self.api_base, channel_id);
        let auth = format!("Bot {}", self.token);
        (url, auth, payload.clone())
    }
}

impl DiscordTransport for DiscordRestTransport {
    fn send_message(&self, channel_id: &str, payload: &Value) -> Result<(), ChannelError> {
        let (url, auth, body) = self.build_send_request(channel_id, payload);
        self.agent
            .post(&url)
            .set("Authorization", &auth)
            .send_json(body)
            .map_err(|e| ChannelError::from_ureq("DISCORD_HTTP", e))?;
        Ok(())
    }
}

/// Normalize a Discord gateway MESSAGE_CREATE or interaction payload into the
/// channel seam. The transport bridge can feed raw gateway JSON here instead
/// of reimplementing approval routing at every call site.
pub fn parse_event(payload: &Value) -> Result<Option<ChannelEvent>, ChannelError> {
    let kind = payload.get("type").and_then(Value::as_u64).or_else(|| {
        match payload.get("t").and_then(Value::as_str) {
            Some("MESSAGE_CREATE") => Some(0),
            Some("INTERACTION_CREATE") => payload
                .get("data")
                .and_then(|data| data.get("type"))
                .and_then(Value::as_u64),
            _ => None,
        }
    });
    let data = payload.get("data").unwrap_or(payload);
    if kind == Some(0) {
        let channel_id = data
            .get("channel_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ChannelError::new("DISCORD_EVENT", "message has no channel_id"))?;
        let text = data.get("content").and_then(Value::as_str).unwrap_or("");
        let sender = data
            .get("author")
            .and_then(|author| author.get("id"))
            .and_then(Value::as_str)
            .map(|id| id.to_string());
        return Ok(Some(ChannelEvent {
            thread_id: channel_id.to_string(),
            run_id: None,
            text: text.to_string(),
            approval: None,
            scope: None,
            sender,
        }));
    }
    if kind == Some(2) {
        let custom_id = data
            .get("custom_id")
            .and_then(Value::as_str)
            .ok_or_else(|| ChannelError::new("DISCORD_EVENT", "interaction has no custom_id"))?;
        let (answer, scope) = custom_id
            .split_once(':')
            .ok_or_else(|| ChannelError::new("DISCORD_EVENT", "invalid approval custom_id"))?;
        let approval = match answer {
            "grant" => Some(crate::channel::ApprovalAnswer::Grant),
            "deny" => Some(crate::channel::ApprovalAnswer::Deny),
            _ => {
                return Err(ChannelError::new(
                    "DISCORD_EVENT",
                    "unknown approval answer",
                ))
            }
        };
        if scope.is_empty() {
            return Err(ChannelError::new(
                "DISCORD_EVENT",
                "approval scope is empty",
            ));
        }
        let channel_id = data
            .get("channel_id")
            .and_then(Value::as_str)
            .or_else(|| payload.get("channel_id").and_then(Value::as_str))
            .ok_or_else(|| ChannelError::new("DISCORD_EVENT", "interaction has no channel_id"))?;
        let sender = payload
            .get("user")
            .and_then(|user| user.get("id"))
            .and_then(Value::as_str)
            .or_else(|| {
                payload
                    .get("member")
                    .and_then(|member| member.get("user"))
                    .and_then(|user| user.get("id"))
                    .and_then(Value::as_str)
            })
            .map(|id| id.to_string());
        return Ok(Some(ChannelEvent {
            thread_id: channel_id.to_string(),
            run_id: None,
            text: String::new(),
            approval,
            scope: Some(scope.to_string()),
            sender,
        }));
    }
    Ok(None)
}

pub struct DiscordChannel {
    pub token: String,
    transport: Arc<dyn DiscordTransport>,
    inbox: Mutex<Vec<ChannelEvent>>,
}

/// Hand-written so the bot token never reaches a log line. A `#[derive(Debug)]`
/// here would print a live credential, and the token is a `pub` field, so
/// any future derive is a leak. Same intent as `SecretValue`'s Debug, kept
/// local to avoid a gateway -> secrets dependency for three fields.
impl fmt::Debug for DiscordChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DiscordChannel {{ token: *** }}")
    }
}

impl DiscordChannel {
    pub fn new(token: impl Into<String>, transport: Arc<dyn DiscordTransport>) -> Self {
        Self {
            token: token.into(),
            transport,
            inbox: Mutex::new(Vec::new()),
        }
    }
    pub fn rest(token: impl Into<String>) -> Self {
        let token = token.into();
        let transport = Arc::new(DiscordRestTransport::new(token.clone()));
        Self::new(token, transport)
    }
    pub fn push_inbound(&self, event: ChannelEvent) {
        crate::channel::lock(&self.inbox).push(event);
    }

    fn payload(frame: &UiFrame, text: String) -> Value {
        if frame.kind == UiFrameKind::Approval && frame.name == "requested" {
            json!({
                "content": text,
                "components": [{
                    "type": 1,
                    "components": [
                        {"type": 2, "style": 2, "label": "Grant", "custom_id": format!("grant:{}", frame.text)},
                        {"type": 2, "style": 2, "label": "Deny", "custom_id": format!("deny:{}", frame.text)}
                    ]
                }]
            })
        } else {
            json!({ "content": text })
        }
    }

    /// Exposed for protocol tests and gateway bridges.
    pub fn payloads(&self, envelope: &ChannelEnvelope) -> Vec<Value> {
        let text = format_text(&envelope.frame);
        let mut payloads = chunk_text(&text, DISCORD_CONTENT_LIMIT)
            .into_iter()
            .map(|part| Self::payload(&envelope.frame, part))
            .collect::<Vec<_>>();
        if payloads.is_empty() {
            payloads.push(json!({"content": ""}));
        }
        payloads
    }
}

impl Channel for DiscordChannel {
    fn name(&self) -> &str {
        "discord"
    }
    fn send(&self, envelope: ChannelEnvelope) -> Result<(), ChannelError> {
        // The thread id is the canonical Discord channel id.  Integrations
        // that use a thread append the thread id before handing it here.
        for payload in self.payloads(&envelope) {
            self.transport.send_message(&envelope.thread_id, &payload)?;
        }
        Ok(())
    }
    fn poll(&self) -> Vec<ChannelEvent> {
        let mut events = std::mem::take(&mut *crate::channel::lock(&self.inbox));
        events.extend(self.transport.poll_events());
        events
    }
}

#[cfg(test)]
#[path = "discord_tests.rs"]
mod tests;
