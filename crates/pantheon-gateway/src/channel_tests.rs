//! Tests for `pantheon_gateway::channel::tests` — sibling file so sources stay test-free.
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
        sender: None,
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

#[test]
fn retry_after_parses_delta_seconds_and_caps() {
    use super::*;
    assert_eq!(parse_retry_after(Some("3")), Some(3));
    assert_eq!(parse_retry_after(Some("  30 ")), Some(30));
    // HTTP-date form is not a delta: fall back to our own backoff.
    assert_eq!(
        parse_retry_after(Some("Wed, 21 Oct 2015 07:28:00 GMT")),
        None
    );
    assert_eq!(parse_retry_after(Some("0")), None);
    assert_eq!(parse_retry_after(Some("never")), None);
    assert_eq!(parse_retry_after(None), None);
    // A hostile header cannot park the daemon.
    assert_eq!(parse_retry_after(Some("99999")), Some(600));
}
