//! Tests for `pantheon_runtime::tests` — sibling file so sources stay test-free.
use super::*;
/// Session search indexes message events as they are emitted, and the
/// FTS5 query finds the exact chunk back. This is the tool\'s contract.
#[test]
fn session_search_indexes_and_finds() {
    let dir = std::env::temp_dir().join(format!("pantheon-rt-search-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sup = Supervisor::open(dir).unwrap();
    sup.start_run("run_s1").unwrap();
    sup.emit(Event::AssistantMessage {
        run_id: "run_s1".into(),
        message: pantheon_core::message::Message::user(
            "debugging the Figma MCP server connection timeout",
        ),
    })
    .unwrap();
    sup.emit(Event::AssistantMessage {
        run_id: "run_s1".into(),
        message: pantheon_core::message::Message::assistant(
            "the TUI session creation block was the culprit",
        ),
    })
    .unwrap();
    sup.emit(Event::SessionTitled {
        run_id: "run_s1".into(),
        title: "figma-mcp-timeout".into(),
        model: "opus".into(),
        source: "auto".into(),
    })
    .unwrap();

    // Exact identifier: the model id in the title.
    let hits = sup.inner.search.search("figma-mcp-timeout", 8).unwrap();
    assert!(!hits.is_empty(), "title chunk matches");
    assert_eq!(hits[0].chunk.kind, "title");

    // Plain terms across a message chunk.
    let hits = sup.inner.search.search("TUI session creation", 8).unwrap();
    assert!(!hits.is_empty(), "message chunk matches");
    assert!(hits.iter().any(|h| h.chunk.text.contains("culprit")));

    // Non-matching query returns nothing.
    let hits = sup.inner.search.search("quantum-teleport", 8).unwrap();
    assert!(hits.is_empty(), "no false positives");
}

#[test]
fn start_complete_render_log() {
    let dir = std::env::temp_dir().join(format!("pantheon-rt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sup = Supervisor::open(dir).unwrap();
    let id = "run_test_1";
    assert!(!sup.start_run(id).unwrap());
    sup.emit(Event::ToolStarted {
        run_id: id.into(),
        call_id: "t".into(),
        tool: "shell".into(),
        args: String::new(),
        provenance: pantheon_core::provenance::Provenance::system("test"),
    })
    .unwrap();
    sup.complete(id).unwrap();
    assert!(sup.render_run_log(id).unwrap().contains("completed"));
}
#[test]
fn crash_recovery_flag() {
    let dir = std::env::temp_dir().join(format!("pantheon-rt2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sup = Supervisor::open(dir.clone()).unwrap();
    sup.start_run("run_crash").unwrap();
    drop(sup);
    let sup2 = Supervisor::open(dir).unwrap();
    assert!(sup2.start_run("run_crash").unwrap());
    assert!(sup2
        .render_run_log("run_crash")
        .unwrap()
        .contains("recovered"));
}

#[test]
fn observers_receive_events_and_guard_unregisters() {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    let dir = std::env::temp_dir().join(format!("pantheon-obs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sup = Supervisor::open(dir).unwrap();
    sup.start_run("run_obs").unwrap();

    let hits = std::sync::Arc::new(AtomicUsize::new(0));
    let h2 = hits.clone();
    let guard = sup.register_observer(std::sync::Arc::new(move |_ev: &Event| {
        h2.fetch_add(1, AtomicOrdering::SeqCst);
    }));

    sup.emit(Event::RunProgress {
        run_id: "run_obs".into(),
        detail: "hello observer".into(),
    })
    .unwrap();
    assert_eq!(hits.load(AtomicOrdering::SeqCst), 1, "observer fired");

    // Dropping the guard unregisters: no further deliveries.
    drop(guard);
    sup.emit(Event::RunProgress {
        run_id: "run_obs".into(),
        detail: "should not deliver".into(),
    })
    .unwrap();
    assert_eq!(
        hits.load(AtomicOrdering::SeqCst),
        1,
        "guard removed observer"
    );
}

#[test]
fn run_lifecycle_transitions_reach_observers() {
    // `start_run` / `complete` / `fail` / `cancel` used to append straight to
    // the ledger, so a run could finish durably while every live subscriber
    // (TUI, the hook bridge) still believed it was running. They must fan out
    // like every other event — `on_session_end` depends on it.
    let dir = std::env::temp_dir().join(format!("pantheon-obs-life-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sup = Supervisor::open(dir).unwrap();

    let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let s2 = seen.clone();
    let _guard = sup.register_observer(std::sync::Arc::new(move |ev: &Event| {
        let tag = match ev {
            Event::RunStarted { .. } => "started",
            Event::RunCompleted { .. } => "completed",
            Event::RunFailed { .. } => "failed",
            Event::RunCanceled { .. } => "canceled",
            _ => return,
        };
        s2.lock().unwrap().push(tag.to_string());
    }));

    sup.start_run("life_ok").unwrap();
    sup.complete("life_ok").unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["started".to_string(), "completed".to_string()],
        "run start/complete must both be observed"
    );

    seen.lock().unwrap().clear();
    sup.start_run("life_bad").unwrap();
    sup.fail("life_bad", "BOOM").unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["started".to_string(), "failed".to_string()],
        "run start/fail must both be observed"
    );

    seen.lock().unwrap().clear();
    sup.start_run("life_cancel").unwrap();
    sup.cancel_run_intent("life_cancel", "user").unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        vec!["started".to_string(), "canceled".to_string()],
        "run start/cancel must both be observed"
    );
}

#[test]
fn grant_flips_parked_run_back_to_running() {
    let dir = std::env::temp_dir().join(format!("pantheon-rt3-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sup = Supervisor::open(dir).unwrap();
    sup.start_run("run_park").unwrap();
    sup.emit(Event::ApprovalRequested {
        run_id: "run_park".into(),
        scope: "call_0_0".into(),
    })
    .unwrap();
    assert_eq!(
        sup.ledger_status("run_park").unwrap().as_deref(),
        Some("awaiting_approval")
    );
    sup.grant("run_park", "call_0_0").unwrap();
    assert_eq!(
        sup.ledger_status("run_park").unwrap().as_deref(),
        Some("running")
    );
}

#[test]
fn replay_rebuilds_transcript_and_unfinished_calls() {
    use pantheon_core::message::Message;
    let dir = std::env::temp_dir().join(format!("pantheon-rt4-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sup = Supervisor::open(dir).unwrap();
    sup.start_run("run_replay").unwrap();
    sup.emit(Event::AssistantMessage {
        run_id: "run_replay".into(),
        message: Message::user("do the thing"),
    })
    .unwrap();
    sup.emit(Event::ToolStarted {
        run_id: "run_replay".into(),
        call_id: "call_0_0".into(),
        tool: "shell".into(),
        args: "{\"cmd\":\"ls\"}".into(),
        provenance: pantheon_core::provenance::Provenance::system("test"),
    })
    .unwrap();
    let entries = sup.replay("run_replay").unwrap();
    let msgs = crate::session::rebuild_messages(entries.clone());
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].content, "do the thing");
    assert_eq!(
        crate::session::unfinished_calls(&entries),
        vec!["call_0_0".to_string()]
    );
}

#[test]
fn grant_rejects_unknown_scope() {
    let dir = std::env::temp_dir().join(format!("pantheon-rt5-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sup = Supervisor::open(dir).unwrap();
    sup.start_run("run_unknown").unwrap();
    // Park the run on approval so grant() reaches the scope check.
    sup.emit(Event::ApprovalRequested {
        run_id: "run_unknown".into(),
        scope: "real_scope".into(),
    })
    .unwrap();
    assert_eq!(
        sup.ledger_status("run_unknown").unwrap().as_deref(),
        Some("awaiting_approval")
    );
    // Grant a different scope: must refuse as unknown.
    let err = sup.grant("run_unknown", "other_scope").unwrap_err();
    assert_eq!(err.code, "RT_APPROVAL_UNKNOWN");
}

#[test]
fn lease_guard_blocks_a_second_supervisor_until_release() {
    let dir = std::env::temp_dir().join(format!("pantheon-lease-guard-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let first = Supervisor::open(dir.clone()).unwrap();
    let second = Supervisor::open(dir).unwrap();
    first.start_run_with_lease("run-guarded").unwrap();
    let guard = RunLeaseGuard::try_new(first.clone(), "run-guarded").unwrap();
    assert!(guard.is_healthy());
    assert!(second.acquire_lease("run-guarded").is_err());
    drop(guard);
    assert!(second.acquire_lease("run-guarded").is_ok());
}

#[test]
fn canceling_a_run_moves_linked_operations_to_canceled() {
    let dir = std::env::temp_dir().join(format!("pantheon-op-cancel-run-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sup = Supervisor::open(dir).unwrap();
    sup.start_run_with_lease("run-op").unwrap();
    sup.create_operation(
        "run-op:call_0_0",
        "tool.execute",
        serde_json::json!({
            "phase": "execute",
            "request": {"run_id": "run-op", "name": "shell", "args": ""},
            "translated": {"name": "shell", "args": ""}
        }),
    )
    .unwrap();
    sup.cancel_run("run-op", "user stop").unwrap();
    assert_eq!(
        sup.operations()
            .get("run-op:call_0_0")
            .unwrap()
            .unwrap()
            .status,
        OperationStatus::Canceled
    );
}

#[test]
fn operation_cannot_be_canceled_for_another_run() {
    let dir = std::env::temp_dir().join(format!("pantheon-op-mismatch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sup = Supervisor::open(dir).unwrap();
    sup.create_operation(
        "op",
        "tool.execute",
        serde_json::json!({
            "phase": "execute", "request": {"run_id": "run-a"}
        }),
    )
    .unwrap();
    let error = sup
        .cancel_operation_with_groups("op", "run-b", "stop")
        .unwrap_err();
    assert_eq!(error.code, "OPERATION_RUN_MISMATCH");
    assert_eq!(
        sup.operations().get("op").unwrap().unwrap().status,
        OperationStatus::Ready
    );
}

#[test]
fn grant_rejects_duplicate_scope() {
    let dir = std::env::temp_dir().join(format!("pantheon-rt6-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sup = Supervisor::open(dir).unwrap();
    sup.start_run("run_dup").unwrap();
    sup.emit(Event::ApprovalRequested {
        run_id: "run_dup".into(),
        scope: "call_0_0".into(),
    })
    .unwrap();
    sup.grant("run_dup", "call_0_0").unwrap();
    // First grant unparked the run. Second grant must refuse because
    // the run is no longer parked — the duplicate is caught by the
    // status gate, not the scope check.
    let err = sup.grant("run_dup", "call_0_0").unwrap_err();
    assert_eq!(err.code, "RT_NOT_PARKED");
}

#[test]
fn chat_on_parked_run_is_refused() {
    let dir = std::env::temp_dir().join(format!("pantheon-rt7-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sup = Supervisor::open(dir.clone()).unwrap();
    // Park the run synthetically.
    sup.start_run("run_park").unwrap();
    sup.emit(Event::ApprovalRequested {
        run_id: "run_park".into(),
        scope: "call_0_0".into(),
    })
    .unwrap();
    assert_eq!(
        sup.ledger_status("run_park").unwrap().as_deref(),
        Some("awaiting_approval")
    );
    // Build a session whose chat() must refuse before talking to any model.
    // Use an unreachable endpoint to prove the refusal happens pre-flight.
    let session = crate::session::Session::new(
        dir,
        pantheon_core::capability::Policy::coder(),
        pantheon_core::model::ModelPolicy {
            default: pantheon_core::model::DefaultModel {
                provider: "unreachable.test".into(),
                model: "x".into(),
            },
            fallbacks: pantheon_core::model::FallbackChain::default(),
            auxiliaries: vec![],
        },
        pantheon_secrets::SecretsBroker::new(),
    )
    .unwrap();
    let err = session.chat("run_park", "again").unwrap_err();
    assert_eq!(err.code, "RUN_PARKED");
}

/// The park message names the scope so nobody has to read the ledger to
/// answer an approval. That is only true if `pending_approvals` returns
/// exactly the unresolved scopes, so assert both halves: it lists what is
/// open, and it drops what has been decided.
#[test]
fn pending_approvals_lists_open_scopes_and_drops_resolved_ones() {
    let dir = std::env::temp_dir().join(format!("pantheon-pending-appr-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sup = Supervisor::open(dir).unwrap();
    let run = "run_pending";
    sup.start_run(run).unwrap();
    for scope in ["call_0_0:shell:{\"cmd\":\"ls\"}", "call_0_1:write_file"] {
        sup.emit(Event::ApprovalRequested {
            run_id: run.into(),
            scope: scope.into(),
        })
        .unwrap();
    }
    // Order is request order, and a scope with embedded JSON must survive
    // verbatim: the operator is going to paste it into a shell.
    assert_eq!(
        sup.pending_approvals(run).unwrap(),
        vec![
            "call_0_0:shell:{\"cmd\":\"ls\"}".to_string(),
            "call_0_1:write_file".to_string(),
        ]
    );

    // A denial is a decision: it must leave the pending set, and a denial is
    // scoped, not terminal, so the other scope stays open.
    sup.deny(run, "call_0_0:shell:{\"cmd\":\"ls\"}").unwrap();
    assert_eq!(
        sup.pending_approvals(run).unwrap(),
        vec!["call_0_1:write_file".to_string()],
        "a denied scope is still listed as pending"
    );

    sup.grant(run, "call_0_1:write_file").unwrap();
    assert!(
        sup.pending_approvals(run).unwrap().is_empty(),
        "a granted scope is still listed as pending"
    );
}

/// A run that was never parked has nothing pending, and asking must not
/// invent a scope. The park message falls back to pointing at `explain` when
/// this returns empty, so an empty result is a real branch, not a curiosity.
#[test]
fn pending_approvals_is_empty_for_a_run_that_never_parked() {
    let dir = std::env::temp_dir().join(format!("pantheon-pending-none-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let sup = Supervisor::open(dir).unwrap();
    sup.start_run("run_never_parked").unwrap();
    sup.emit(Event::RunProgress {
        run_id: "run_never_parked".into(),
        detail: "just working".into(),
    })
    .unwrap();
    assert!(sup
        .pending_approvals("run_never_parked")
        .unwrap()
        .is_empty());
}
