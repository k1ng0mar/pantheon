//! Behavioral tests for the gateway daemon: real `ChannelDaemon::run`
//! ticks, cursor persistence on disk, and dead-letter routing. Moved here
//! from `pantheon-gateway/src/daemon_tests.rs`; runs under
//! `cargo test -p pantheon-eval`, not beside the code.
use pantheon_gateway::channel::{
    Channel, ChannelEnvelope, ChannelError, ChannelEvent, MemoryChannel,
};
use pantheon_gateway::daemon::{ChannelDaemon, EventSink, UpdateCursor, MAX_SEND_ATTEMPTS};
use pantheon_gateway::OutboundMessage;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct RecordingSink {
    messages: Mutex<Vec<(String, String)>>,
    approvals: Mutex<Vec<(String, String, bool)>>,
}
impl EventSink for RecordingSink {
    fn on_message(&self, thread: &str, _sender: Option<&str>, text: &str) {
        self.messages
            .lock()
            .unwrap()
            .push((thread.into(), text.into()));
    }
    fn on_approval(&self, thread: &str, _sender: Option<&str>, scope: &str, grant: bool) {
        self.approvals
            .lock()
            .unwrap()
            .push((thread.into(), scope.into(), grant));
    }
}

#[test]
fn cursor_advances_and_persists() {
    let dir = std::env::temp_dir().join(format!("pantheon-cursor-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("cursor");
    let c = UpdateCursor::new(path.clone());
    assert_eq!(c.get(), 0);
    c.advance(41);
    assert_eq!(c.get(), 41);
    // Reload from disk.
    let c2 = UpdateCursor::new(path);
    assert_eq!(c2.get(), 41);
    // Never goes backwards.
    c2.advance(7);
    assert_eq!(c2.get(), 41);
}

#[test]
fn daemon_stops_when_asked() {
    let dir = std::env::temp_dir().join(format!("pantheon-daemon-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let daemon = ChannelDaemon::new(dir.join("cursor"));
    let sink = RecordingSink {
        messages: Mutex::new(vec![]),
        approvals: Mutex::new(vec![]),
    };
    let outbound = Mutex::new(vec![]);
    let ticks = AtomicUsize::new(0);
    let stop_fn = || {
        let _ = ticks.fetch_add(1, Ordering::SeqCst) > 0;
        true
    };
    daemon.run(vec![], None, &sink, &outbound, &stop_fn);
    assert!(sink.messages.lock().unwrap().is_empty());
}

fn outbound_msg(to: &str, gateway: &str) -> OutboundMessage {
    OutboundMessage {
        to_conversation: to.into(),
        text: "hello".into(),
        gateway: gateway.into(),
        attempts: 0,
    }
}

fn named_channel(name: &str) -> Arc<MemoryChannel> {
    Arc::new(MemoryChannel::new(name))
}

struct FailChannel {
    name: String,
    sends: AtomicUsize,
}
impl Channel for FailChannel {
    fn name(&self) -> &str {
        &self.name
    }
    fn send(&self, _envelope: ChannelEnvelope) -> Result<(), ChannelError> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        Err(ChannelError::new("TEST_FAIL", "boom"))
    }
    fn poll(&self) -> Vec<ChannelEvent> {
        Vec::new()
    }
}

#[test]
fn failed_sends_dead_letter_through_the_daemon() {
    let dir = std::env::temp_dir().join(format!("pantheon-deadletter-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut daemon = ChannelDaemon::new(dir.join("cursor"));
    daemon.poll_interval = Duration::from_millis(1);
    let fail = Arc::new(FailChannel {
        name: "telegram".into(),
        sends: AtomicUsize::new(0),
    });
    let channels: Vec<Arc<dyn Channel>> = vec![fail.clone()];
    let sink = RecordingSink {
        messages: Mutex::new(vec![]),
        approvals: Mutex::new(vec![]),
    };
    // One attempt left before the bound: a single tick must dead-letter it.
    let outbound = Mutex::new(vec![OutboundMessage {
        to_conversation: "t".into(),
        text: "hi".into(),
        gateway: "telegram".into(),
        attempts: MAX_SEND_ATTEMPTS - 1,
    }]);
    let ticks = AtomicUsize::new(0);
    let stop = || ticks.fetch_add(1, Ordering::SeqCst) >= 1;
    daemon.run(channels, None, &sink, &outbound, &stop);
    assert_eq!(fail.sends.load(Ordering::SeqCst), 1);
    assert!(
        outbound.lock().unwrap().is_empty(),
        "dead-lettered, not requeued forever"
    );
}

#[test]
fn daemon_routes_claimed_thread_to_owning_channel() {
    let dir = std::env::temp_dir().join(format!("pantheon-route-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut daemon = ChannelDaemon::new(dir.join("cursor"));
    daemon.poll_interval = Duration::from_millis(1);
    let telegram = named_channel("telegram");
    let discord = named_channel("discord");
    // A Discord inbound event claims thread T9 for discord.
    discord.push_inbound(ChannelEvent {
        thread_id: "T9".into(),
        run_id: None,
        text: "hi".into(),
        approval: None,
        scope: None,
        sender: None,
    });
    let channels: Vec<Arc<dyn Channel>> = vec![telegram.clone(), discord.clone()];
    let sink = RecordingSink {
        messages: Mutex::new(vec![]),
        approvals: Mutex::new(vec![]),
    };
    let outbound = Mutex::new(vec![outbound_msg("T9", "")]);
    let ticks = AtomicUsize::new(0);
    let stop = || ticks.fetch_add(1, Ordering::SeqCst) >= 1;
    daemon.run(channels, None, &sink, &outbound, &stop);
    assert_eq!(discord.drain_outbound().len(), 1, "reply went to discord");
    assert!(
        telegram.drain_outbound().is_empty(),
        "telegram never saw the discord reply"
    );
    assert_eq!(sink.messages.lock().unwrap().len(), 1, "event routed");
}
