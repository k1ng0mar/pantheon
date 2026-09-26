//! Tests for `pantheon_gateway::canonical::tests` — sibling file so sources stay test-free.
use super::*;
use crate::{Attachment, Identity};

#[test]
fn conversation_key_includes_the_thread() {
    assert_eq!(Conversation::new("discord", "c1").key(), "discord:c1");
    assert_eq!(
        Conversation::new("discord", "c1").in_thread("t9").key(),
        "discord:c1:t9",
        "threads must not share per-conversation state"
    );
}

#[test]
fn commands_split_into_name_and_args() {
    let c = Command::parse("/deploy prod now", Conversation::new("discord", "c1")).unwrap();
    assert_eq!(c.name, "deploy");
    assert_eq!(c.args, vec!["prod", "now"]);
    assert_eq!(c.conversation.key(), "discord:c1");
}

#[test]
fn bot_suffixed_commands_are_normalized() {
    let c = Command::parse("/status@nyxbot", Conversation::new("telegram", "c1")).unwrap();
    assert_eq!(c.name, "status");
    assert!(c.args.is_empty());
}

#[test]
fn plain_text_and_bare_slash_are_not_commands() {
    let conv = Conversation::new("telegram", "c1");
    assert!(Command::parse("hello there", conv.clone()).is_none());
    assert!(Command::parse("/", conv.clone()).is_none());
    assert!(Command::parse("   ", conv).is_none());
}

#[test]
fn canonical_events_report_their_conversation() {
    let conv = Conversation::new("telegram", "c7").in_thread("t1");
    let msg = InboundMessage {
        id: "m1".into(),
        from: Identity {
            gateway: "telegram".into(),
            user: "joe".into(),
        },
        conversation: conv.clone(),
        text: "hi".into(),
        attachments: vec![Attachment {
            name: "a.txt".into(),
            mime: "text/plain".into(),
            bytes: 3,
        }],
    };
    assert_eq!(
        Canonical::Message(msg).conversation_key().as_deref(),
        Some("telegram:c7:t1")
    );
    let reaction = Reaction {
        conversation: conv.clone(),
        message_id: "m1".into(),
        emoji: "👍".into(),
        removed: false,
    };
    assert_eq!(
        Canonical::Reaction(reaction).conversation_key().as_deref(),
        Some("telegram:c7:t1")
    );
    assert_eq!(
        Canonical::Command(Command {
            conversation: conv,
            name: "deploy".into(),
            args: vec![],
        })
        .conversation_key()
        .as_deref(),
        Some("telegram:c7:t1")
    );
}
