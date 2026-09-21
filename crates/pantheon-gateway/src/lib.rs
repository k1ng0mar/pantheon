//! Gateways (spec section 16): Telegram/Discord/Custom normalized into
//! canonical messages. The agent never knows which surface sent it.
use serde::{Deserialize, Serialize};

pub mod allowlist;
pub mod canonical;
pub mod dedup;
pub mod delivery;

pub use allowlist::{Admission, Allowlist, Pairing};
pub use canonical::{Canonical, Command, Conversation, Reaction};
pub use dedup::{dedup_key, DedupWindow};
pub use delivery::{backoff_ms, plan_delivery, DeliveryOutcome, Outbox};

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
mod tests {
    use super::*;
    #[test]
    fn canonical_shape() {
        let m = InboundMessage {
            id: "m1".into(),
            from: Identity {
                gateway: "telegram".into(),
                user: "u1".into(),
            },
            conversation: Conversation::new("telegram", "c1"),
            text: "build this".into(),
            attachments: vec![],
        };
        assert_eq!(m.text, "build this");
        assert_eq!(m.from.gateway, "telegram");
        assert_eq!(m.conversation.key(), "telegram:c1");
    }
}
