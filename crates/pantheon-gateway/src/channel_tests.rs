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

// A panic while the inbox lock is held poisons it. Before, every later poll
// panicked too, so one bad handler permanently killed the channel and only a
// restart recovered it. Recovering the guard keeps queued events flowing.
#[test]
fn a_poisoned_inbox_does_not_permanently_kill_the_channel() {
    let ch = MemoryChannel::new("mem");
    ch.push_inbound(ChannelEvent {
        thread_id: "t1".into(),
        run_id: None,
        text: "first".into(),
        approval: None,
        scope: None,
        sender: None,
    });

    // Poison the inbox by panicking while the lock is held. A raw pointer
    // keeps the test inside the same crate without needing an Arc clone.
    let m: *const std::sync::Mutex<Vec<ChannelEvent>> = &ch.inbox;
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // SAFETY: `m` points at `ch.inbox`, which outlives this closure, and
        // the guard is dropped before the panic unwinds past the borrow.
        let m = unsafe { &*m };
        let _guard = m.lock().unwrap();
        panic!("handler blew up");
    }));
    assert!(
        ch.inbox.is_poisoned(),
        "the lock should be poisoned for this test to mean anything"
    );

    // The channel must still work.
    ch.push_inbound(ChannelEvent {
        thread_id: "t2".into(),
        run_id: None,
        text: "second".into(),
        approval: None,
        scope: None,
        sender: None,
    });
    let got = ch.poll();
    let texts: Vec<&str> = got.iter().map(|e| e.text.as_str()).collect();
    assert!(
        texts.contains(&"first") && texts.contains(&"second"),
        "queued events must survive a poisoned lock, got {texts:?}"
    );
}
