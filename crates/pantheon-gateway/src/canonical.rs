//! Canonical inbound shapes (§16).
//!
//! Every surface normalizes into these before the runtime sees anything, so
//! the agent never learns which gateway, protocol, or client sent the event.
//! A surface that cannot express one of these simply never produces it.

use crate::{InboundMessage, OutboundMessage};
use serde::{Deserialize, Serialize};

/// A place a conversation happens: a channel, DM, or thread on a surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub gateway: String,
    /// Surface-native conversation id.
    pub id: String,
    /// Thread inside the conversation, when the surface has threads.
    pub thread: Option<String>,
}

impl Conversation {
    pub fn new(gateway: &str, id: &str) -> Self {
        Self {
            gateway: gateway.into(),
            id: id.into(),
            thread: None,
        }
    }

    pub fn in_thread(mut self, thread: &str) -> Self {
        self.thread = Some(thread.into());
        self
    }

    /// Stable key for per-conversation state.
    pub fn key(&self) -> String {
        match &self.thread {
            Some(thread) => format!("{}:{}:{}", self.gateway, self.id, thread),
            None => format!("{}:{}", self.gateway, self.id),
        }
    }
}

/// A reaction to a message. `removed` distinguishes add from removal, which
/// surfaces report as separate events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reaction {
    pub conversation: Conversation,
    pub message_id: String,
    pub emoji: String,
    pub removed: bool,
}

/// A slash-style command, already split into name and arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    pub conversation: Conversation,
    pub name: String,
    pub args: Vec<String>,
}

impl Command {
    /// Parse `/name arg1 arg2`. Returns `None` when the text is not a
    /// command. A surface that suffixes the bot (`/name@bot`) is normalized
    /// to `name`, and a bare `/` is not a command.
    pub fn parse(text: &str, conversation: Conversation) -> Option<Self> {
        let rest = text.trim_start().strip_prefix('/')?;
        let mut parts = rest.split_whitespace();
        let name = parts.next()?;
        let name = name.split('@').next().unwrap_or(name);
        if name.is_empty() {
            return None;
        }
        Some(Self {
            conversation,
            name: name.to_string(),
            args: parts.map(|a| a.to_string()).collect(),
        })
    }
}

/// Everything a gateway can hand the runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Canonical {
    Message(InboundMessage),
    Reaction(Reaction),
    Command(Command),
    /// An outbound message re-entering for delivery. Kept in the same enum so
    /// one ledger records both directions.
    Outbound(OutboundMessage),
}

impl Canonical {
    /// Conversation this event belongs to, whichever shape it is.
    pub fn conversation_key(&self) -> Option<String> {
        match self {
            Canonical::Message(m) => Some(m.conversation.key()),
            Canonical::Reaction(r) => Some(r.conversation.key()),
            Canonical::Command(c) => Some(c.conversation.key()),
            Canonical::Outbound(m) => Some(m.to_conversation.clone()),
        }
    }
}

#[cfg(test)]
#[path = "canonical_tests.rs"]
mod tests;
