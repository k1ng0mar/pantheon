//! Agent-to-agent inbox drain: messages from other agents reach the running
//! turn as marked, untrusted data.
//! Run with `cargo test -p pantheon-eval`.

use pantheon_agent::agent_profile::{AgentProfile, ProfileRegistry};
use pantheon_api::events::Event;
use pantheon_api::message::Role;
use pantheon_api::provenance::TrustTier;
use pantheon_runtime::agent_runtime::AgentRuntime;
use pantheon_runtime::session::{agent_message_content, rebuild_messages, Session};
use pantheon_storage::collaboration::{AgentMessage, MessageKind};

fn test_session(tag: &str) -> (Session, std::path::PathBuf) {
    let dir =
        std::env::temp_dir().join(format!("pantheon-agent-inbox-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let policy = pantheon_api::capability::Policy::coder();
    let model_policy = pantheon_api::model::ModelPolicy {
        reasoning_budget: Default::default(),
        reasoning: Default::default(),
        default: pantheon_api::model::DefaultModel {
            provider: "test".into(),
            model: "test".into(),
        },
        fallbacks: pantheon_api::model::FallbackChain {
            fallbacks: Vec::new(),
        },
        auxiliaries: Vec::new(),
    };
    let secrets = pantheon_secrets::SecretsBroker::from_system_env();
    let s = Session::new(dir.clone(), policy, model_policy, secrets).unwrap();
    (s, dir)
}

/// Attach a `parent` agent profile to the session and return a `child` peer
/// sharing its supervisor and data dir, the way `for_profile` does.
fn agent_and_peer(session: &Session, dir: &std::path::Path) -> (AgentRuntime, AgentRuntime) {
    let mut registry = ProfileRegistry::new();
    registry
        .insert("parent", AgentProfile::default())
        .expect("insert parent");
    registry
        .insert("child", AgentProfile::default())
        .expect("insert child");
    let effective = registry.resolve("parent", "coder").expect("resolve parent");
    let agent = AgentRuntime::new(
        session.supervisor.clone(),
        registry,
        effective,
        dir.to_path_buf(),
    )
    .expect("agent runtime");
    *session.agent.lock().expect("agent lock") = Some(agent.clone());
    let peer = agent.for_profile("child").expect("peer runtime");
    (agent, peer)
}

/// The message the replay path rebuilds from, so a delivery and a resume
/// produce byte-identical rows.
fn sample_message() -> AgentMessage {
    AgentMessage {
        message_id: "msg_1".into(),
        collaboration_id: None,
        task_id: None,
        correlation_id: None,
        sender: "child".into(),
        recipient: "parent".into(),
        kind: MessageKind::Note,
        content: "the build is red".into(),
        created_ms: 0,
        settled_ms: None,
    }
}

/// The turn-boundary drain injects one marked user message per queued agent
/// message, oldest first, at Untrusted tier with the sender named.
#[test]
fn deliver_agent_messages_injects_marked_untrusted_messages() {
    let (s, dir) = test_session("deliver");
    let run = "run_agent_deliver";
    s.supervisor.start_run(run).unwrap();
    let (_agent, peer) = agent_and_peer(&s, &dir);

    peer.send(
        "msg_1",
        "parent",
        MessageKind::Note,
        "the build is red",
        None,
        None,
    )
    .expect("send");

    let mut messages = Vec::new();
    s.deliver_agent_messages(&mut messages, run).unwrap();

    assert_eq!(messages.len(), 1, "one queued message delivers once");
    let m = &messages[0];
    assert_eq!(m.role, Role::User);
    assert!(
        m.content.contains("the build is red"),
        "payload must survive verbatim"
    );
    assert_ne!(
        m.content, "the build is red",
        "agent content must be framed, never a bare echo"
    );
    assert_eq!(
        m.provenance.as_ref().map(|p| p.trust),
        Some(TrustTier::Untrusted),
        "agent content is never User or System tier"
    );
    assert_eq!(
        m.provenance.as_ref().map(|p| p.source.as_str()),
        Some("agent:child"),
        "provenance names the sender"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Delivery settles the mailbox: a second drain at the same turn boundary
/// must not redeliver, or a resumed run would replay the same instruction.
#[test]
fn drained_messages_are_settled_and_never_delivered_twice() {
    let (s, dir) = test_session("settle");
    let run = "run_agent_settle";
    s.supervisor.start_run(run).unwrap();
    let (_agent, peer) = agent_and_peer(&s, &dir);
    peer.send("msg_1", "parent", MessageKind::Note, "once", None, None)
        .expect("send");

    let mut first = Vec::new();
    s.deliver_agent_messages(&mut first, run).unwrap();
    assert_eq!(first.len(), 1);

    let mut second = Vec::new();
    s.deliver_agent_messages(&mut second, run).unwrap();
    assert!(
        second.is_empty(),
        "a settled message must not redeliver at the next boundary"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A session with no agent profile has no mailbox: nothing is delivered and
/// no delivery row is written.
#[test]
fn session_without_an_agent_delivers_nothing() {
    let (s, dir) = test_session("noagent");
    let run = "run_agent_noagent";
    s.supervisor.start_run(run).unwrap();

    let mut messages = Vec::new();
    s.deliver_agent_messages(&mut messages, run).unwrap();

    assert!(messages.is_empty());
    assert!(
        s.supervisor
            .replay(run)
            .unwrap()
            .iter()
            .all(|e| !matches!(e.event, Event::AgentMessageDelivered { .. })),
        "no delivery rows without an agent"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Resume fidelity: an `AgentMessageDelivered` row rebuilds into exactly the
/// framed message the model saw mid-turn, in position.
#[test]
fn rebuild_messages_restores_agent_message_in_order() {
    let (s, dir) = test_session("rebuild");
    let run = "run_agent_rebuild";
    s.supervisor.start_run(run).unwrap();
    s.supervisor
        .emit(Event::AssistantMessage {
            run_id: run.into(),
            message: pantheon_api::message::Message::user("check the build"),
        })
        .unwrap();
    s.supervisor
        .emit(Event::AgentMessageDelivered {
            run_id: run.into(),
            message_id: "msg_1".into(),
            sender: "child".into(),
            kind: "note".into(),
            task_id: None,
            correlation_id: None,
            content: "the build is red".into(),
        })
        .unwrap();
    s.supervisor
        .emit(Event::AssistantMessage {
            run_id: run.into(),
            message: pantheon_api::message::Message::assistant("on it"),
        })
        .unwrap();

    let msgs = rebuild_messages(s.supervisor.replay(run).unwrap());
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0].content, "check the build");
    assert_eq!(msgs[1].content, agent_message_content(&sample_message()));
    assert_eq!(msgs[1].role, Role::User);
    assert_eq!(
        msgs[1].provenance.as_ref().map(|p| p.trust),
        Some(TrustTier::Untrusted)
    );
    assert_eq!(msgs[2].content, "on it");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The framing itself is the trust contract: it must tell the model this is
/// data to verify, not a command to follow.
#[test]
fn framing_marks_agent_content_as_data_not_instruction() {
    let framed = agent_message_content(&sample_message());
    assert!(
        framed.contains("not an instruction"),
        "framing must deny instructional force: {framed}"
    );
    assert!(framed.contains("child"), "sender is named");
    assert!(framed.contains("note"), "kind is named");
    assert!(framed.contains("the build is red"), "payload is carried");
    assert_ne!(framed, sample_message().content);
}
