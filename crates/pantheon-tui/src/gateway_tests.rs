//! Tests for `crate::gateway::tests` — sibling file so sources stay test-free.
use super::*;
use pantheon_gateway::EventSink;

fn test_queues() -> HashMap<String, Arc<Mutex<Vec<pantheon_gateway::OutboundMessage>>>> {
    ["telegram", "discord"]
        .into_iter()
        .map(|g| (g.to_string(), Arc::new(Mutex::new(Vec::new()))))
        .collect()
}

fn sink_with(allow: Option<&[&str]>) -> RuntimeSink {
    RuntimeSink::new(
        std::path::PathBuf::from("/tmp"),
        Policy::coder(),
        test_queues(),
        allow.map(|ids| ids.iter().map(|s| s.to_string()).collect()),
    )
}

fn queue_len(sink: &RuntimeSink, gateway: &str) -> usize {
    sink.queues
        .get(gateway)
        .unwrap()
        .lock()
        .unwrap()
        .len()
}

#[test]
fn allowlist_admits_listed_senders_only() {
    let s = sink_with(Some(&["111", "222"]));
    assert!(s.allowed(Some("111")));
    assert!(!s.allowed(Some("333")));
    // No sender identity with an active allowlist: refuse.
    assert!(!s.allowed(None));
}

#[test]
fn unset_allowlist_admits_everyone_local_only() {
    let s = sink_with(None);
    assert!(s.allowed(Some("anyone")));
    assert!(s.allowed(None));
}

#[test]
fn empty_allowlist_denies_everyone() {
    let s = sink_with(Some(&[]));
    assert!(!s.allowed(Some("111")));
}

#[test]
fn denied_sender_never_reaches_the_runtime() {
    // on_message from a non-listed sender must not bind a thread or
    // touch a session; outbound gets the refusal instead.
    let dir = std::env::temp_dir().join(format!("pantheon-gw-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let s = RuntimeSink::new(
        dir,
        Policy::coder(),
        test_queues(),
        Some(["111".to_string()].into_iter().collect()),
    );
    let surface = SurfaceSink {
        inner: &s,
        gateway: "telegram",
    };
    surface.on_message("chat1", Some("999"), "rm -rf /");
    let out = s.queues.get("telegram").unwrap().lock().unwrap();
    assert_eq!(out.len(), 1, "exactly the refusal");
    assert!(out[0].text.contains("not authorized"));
    assert_eq!(out[0].gateway, "telegram");
}

#[test]
fn surface_sink_routes_replies_to_its_own_queue() {
    // The whole point of per-surface queues: a discord-tagged event's
    // replies must never land in the telegram queue.
    let s = sink_with(Some(&["111"]));
    let surface = SurfaceSink {
        inner: &s,
        gateway: "discord",
    };
    surface.on_message("chat1", Some("999"), "hi");
    let dq = s.queues.get("discord").unwrap().lock().unwrap();
    assert_eq!(dq.len(), 1, "refusal queued");
    assert!(dq[0].text.contains("not authorized"));
    assert_eq!(dq[0].gateway, "discord");
    assert_eq!(queue_len(&s, "telegram"), 0);
}

#[test]
fn grant_triggers_resume_hook_with_run_id() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let mut s = sink_with(None);
    // Mock the resume hook: the production one spawns a thread and calls
    // the model, which a unit test must not do.
    s.resume = Arc::new(move |req: ResumeRequest| {
        seen2.lock().unwrap().push(req);
    });
    s.threads
        .lock()
        .unwrap()
        .insert("chat9".into(), "run-42".into());
    s.after_approval("discord", "chat9", "run-42", "call_1_0", true);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "grant triggers exactly one resume");
    assert_eq!(seen[0].run_id, "run-42");
    assert_eq!(seen[0].gateway, "discord");
    assert_eq!(seen[0].thread_id, "chat9");
    // The ack went to the discord queue, not telegram's.
    let dq = s.queues.get("discord").unwrap().lock().unwrap();
    assert_eq!(dq.len(), 1);
    assert_eq!(dq[0].text, "granted call_1_0");
    assert_eq!(dq[0].gateway, "discord");
    assert_eq!(queue_len(&s, "telegram"), 0);
}

#[test]
fn deny_does_not_trigger_resume() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let mut s = sink_with(None);
    s.resume = Arc::new(move |req: ResumeRequest| {
        seen2.lock().unwrap().push(req);
    });
    s.after_approval("telegram", "chat1", "run-7", "call_2", false);
    assert!(seen.lock().unwrap().is_empty(), "deny must not resume");
    let tq = s.queues.get("telegram").unwrap().lock().unwrap();
    assert_eq!(tq.len(), 1);
    assert_eq!(tq[0].text, "denied call_2");
}

#[test]
fn outbox_round_trips_gateway() {
    let dir = std::env::temp_dir().join(format!("pantheon-gw-outbox-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    enqueue_outbound(&dir, "12345", "hello", "telegram").unwrap();
    enqueue_outbound(&dir, "chan-9", "hi", "discord").unwrap();
    let (msgs, bad) = drain_outbound(&dir);
    assert!(bad.is_empty(), "{bad:?}");
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].to_conversation, "12345");
    assert_eq!(msgs[0].text, "hello");
    assert_eq!(msgs[0].gateway, "telegram");
    assert_eq!(msgs[1].gateway, "discord");
    // Drained exactly once: the file is truncated.
    let (msgs2, bad2) = drain_outbound(&dir);
    assert!(msgs2.is_empty());
    assert!(bad2.is_empty());
}
