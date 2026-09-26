//! Gateways (spec section 16): Telegram/Discord/Custom normalized into
//! canonical messages. The agent never knows which surface sent it.
//!
//! The interactive path (AG-UI) lives here too:
//! `stream` maps ledger events to UI frames, `sse` encodes frames as
//! `text/event-stream` bytes, `channel` is the transport seam every
//! surface (discord/slack/web/…) consumes, and `genui` mints
//! task-id + signed-URL references (never embedded payloads).
use serde::{Deserialize, Serialize};

pub mod allowlist;
pub mod canonical;
pub mod channel;
pub mod daemon;
pub mod dedup;
pub mod delivery;
pub mod discord;
pub mod discord_gateway;
pub mod genui;
pub mod sse;
pub mod stream;
pub mod telegram;

pub use allowlist::{Admission, Allowlist, Pairing};
pub use canonical::{Canonical, Command, Conversation, Reaction};
pub use channel::{
    chunk_text, fanout, format_text, ApprovalButtons, Channel, ChannelEnvelope, ChannelError,
    ChannelEvent, MemoryChannel, ThreadRunMap,
};
pub use daemon::{poll_telegram_once, route_event, ChannelDaemon, EventSink, UpdateCursor};
pub use dedup::{dedup_key, DedupWindow};
pub use delivery::{backoff_ms, plan_delivery, DeliveryOutcome, Outbox};
pub use discord::{
    parse_event as parse_discord_event, DiscordChannel, DiscordRestTransport, DiscordTransport,
    DISCORD_CONTENT_LIMIT,
};
pub use genui::{valid_task_id, GenUiRef, GenUiSigner, SignedUrl};
pub use sse::{parse_last_event_id, SseEncoder};
pub use stream::{frame_for_event, frames_for_entries, UiFrame, UiFrameKind};
pub use telegram::{
    parse_event as parse_telegram_event, TelegramChannel, TelegramRestTransport, TelegramTransport,
    TELEGRAM_MESSAGE_LIMIT,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Identity {
    pub gateway: String,
    pub user: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub name: String,
    pub mime: String,
    pub bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundMessage {
    pub id: String,
    pub from: Identity,
    /// The canonical place this happened (§16): gateway + conversation +
    /// optional thread. Store the shape, not a bare id, so every surface
    /// reports the same keys for the same place.
    pub conversation: Conversation,
    pub text: String,
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub to_conversation: String,
    pub text: String,
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
