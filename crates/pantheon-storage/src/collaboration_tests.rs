//! Collaboration store tests: task lifecycle, concurrency, persistence, and
//! the two security invariants (an agent never gains authority, and a
//! message is data rather than instruction).
//!
//! These go through the production store against a real SQLite file — not a
//! hand-rolled fake — because the property under test is the SQL compare-and-
//! swap, and a fake would only re-implement it.

use super::*;
use std::path::PathBuf;

fn tmp_db(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "pantheon-collab-{tag}-{}-{:?}.db",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_file(&p);
    p
}

fn store(tag: &str) -> (CollaborationStore, PathBuf) {
    let p = tmp_db(tag);
    (CollaborationStore::open(&p).expect("store opens"), p)
}

#[test]
fn a_task_records_its_origin_and_creator() {
    let (s, p) = store("origin");
    let t = s
        .create_task(
            "task_1",
            Some("collab_1"),
            "nyx",
            Some("zeus"),
            None,
            "write the spec",
        )
        .unwrap();
    assert_eq!(t.origin_agent, "nyx", "who asked must be answerable");
    assert_eq!(t.assigned_agent.as_deref(), Some("zeus"));
    assert_eq!(
        t.status,
        TaskStatus::Assigned,
        "assigned at creation, not pending"
    );
    assert_eq!(t.version, 1);
    assert!(t.result.is_none());
    drop(s);
    let _ = std::fs::remove_file(p);
}

#[test]
fn an_unassigned_task_starts_pending() {
    let (s, p) = store("pending");
    let t = s
        .create_task("task_1", None, "nyx", None, None, "scope it")
        .unwrap();
    assert_eq!(t.status, TaskStatus::Pending);
    drop(s);
    let _ = std::fs::remove_file(p);
}

#[test]
fn the_full_delegation_lifecycle_runs() {
    let (s, p) = store("lifecycle");
    s.create_task("t1", Some("c1"), "nyx", Some("zeus"), None, "build it")
        .unwrap();
    s.transition("t1", None, TaskStatus::Running, None, None)
        .unwrap();
    s.transition(
        "t1",
        None,
        TaskStatus::Completed,
        Some("built the thing"),
        None,
    )
    .unwrap();
    let t = s.task("t1").unwrap().unwrap();
    assert_eq!(t.status, TaskStatus::Completed);
    assert_eq!(t.result.as_deref(), Some("built the thing"));
    assert!(
        t.settled_ms.is_some(),
        "a terminal task records when it settled"
    );
    assert!(!t.is_orphaned());
    drop(s);
    let _ = std::fs::remove_file(p);
}

#[test]
fn completion_without_a_result_is_refused() {
    let (s, p) = store("noresult");
    s.create_task("t1", None, "nyx", Some("zeus"), None, "build it")
        .unwrap();
    // "completed" with nothing to show for it is the exact lie the audit
    // question "what was the result" would otherwise have to accept.
    let err = s
        .transition("t1", None, TaskStatus::Completed, Some("   "), None)
        .unwrap_err();
    assert!(matches!(
        err,
        TaskMutationError::Conflict(TaskConflict::Transition { .. })
    ));
    assert_eq!(
        s.task("t1").unwrap().unwrap().status,
        TaskStatus::Assigned,
        "a refused settle must leave the task untouched"
    );
    drop(s);
    let _ = std::fs::remove_file(p);
}

#[test]
fn failure_requires_a_cause() {
    let (s, p) = store("nocause");
    s.create_task("t1", None, "nyx", Some("zeus"), None, "build it")
        .unwrap();
    s.transition("t1", None, TaskStatus::Running, None, None)
        .unwrap();
    let err = s
        .transition("t1", None, TaskStatus::Failed, None, None)
        .unwrap_err();
    assert!(matches!(err, TaskMutationError::Conflict(_)));
    drop(s);
    let _ = std::fs::remove_file(p);
}

#[test]
fn a_completed_task_cannot_be_reopened() {
    let (s, p) = store("reopen");
    s.create_task("t1", None, "nyx", Some("zeus"), None, "build it")
        .unwrap();
    s.transition("t1", None, TaskStatus::Completed, Some("done"), None)
        .unwrap();
    // Reopening would retract a result the user or a downstream agent has
    // already seen. Retry is modeled as a *new* task (or Failed -> Pending).
    for next in [
        TaskStatus::Running,
        TaskStatus::Assigned,
        TaskStatus::Pending,
    ] {
        let err = s.transition("t1", None, next, None, None).unwrap_err();
        assert!(
            matches!(
                err,
                TaskMutationError::Conflict(TaskConflict::Transition { .. })
            ),
            "completed -> {next} must be refused, got {err:?}"
        );
    }
    drop(s);
    let _ = std::fs::remove_file(p);
}

#[test]
fn a_failed_task_can_be_retried_without_a_new_id() {
    let (s, p) = store("retry");
    s.create_task("t1", None, "nyx", Some("zeus"), None, "build it")
        .unwrap();
    s.transition("t1", None, TaskStatus::Running, None, None)
        .unwrap();
    s.transition("t1", None, TaskStatus::Failed, None, Some("provider down"))
        .unwrap();
    // The retry keeps the id, so the audit trail shows one unit of work
    // attempted twice rather than two unrelated tasks.
    let t = s
        .transition("t1", None, TaskStatus::Pending, None, None)
        .unwrap();
    assert_eq!(t.status, TaskStatus::Pending);
    assert_eq!(t.version, 4, "every accepted transition bumps the version");
    drop(s);
    let _ = std::fs::remove_file(p);
}

#[test]
fn cancellation_is_reachable_from_every_live_state() {
    let (s, p) = store("cancel");
    s.create_task("t1", None, "nyx", Some("zeus"), None, "a")
        .unwrap();
    s.create_task("t2", None, "nyx", Some("zeus"), None, "b")
        .unwrap();
    s.create_task("t3", None, "nyx", Some("zeus"), None, "c")
        .unwrap();
    s.transition("t2", None, TaskStatus::Running, None, None)
        .unwrap();
    s.transition("t3", None, TaskStatus::Running, None, None)
        .unwrap();
    s.transition("t3", None, TaskStatus::Blocked, None, None)
        .unwrap();
    s.transition("t1", None, TaskStatus::Cancelled, None, None)
        .unwrap();
    s.transition("t2", None, TaskStatus::Cancelled, None, None)
        .unwrap();
    s.transition("t3", None, TaskStatus::Running, None, None)
        .unwrap();
    s.transition("t3", None, TaskStatus::Cancelled, None, None)
        .unwrap();
    for id in ["t1", "t2", "t3"] {
        assert_eq!(s.task(id).unwrap().unwrap().status, TaskStatus::Cancelled);
    }
    drop(s);
    let _ = std::fs::remove_file(p);
}

#[test]
fn a_blocked_task_can_resume() {
    let (s, p) = store("blocked");
    s.create_task("t1", None, "nyx", Some("athena"), None, "research X")
        .unwrap();
    s.transition("t1", None, TaskStatus::Running, None, None)
        .unwrap();
    s.transition("t1", None, TaskStatus::Blocked, None, None)
        .unwrap();
    let t = s
        .transition("t1", None, TaskStatus::Running, None, None)
        .unwrap();
    assert_eq!(t.status, TaskStatus::Running);
    drop(s);
    let _ = std::fs::remove_file(p);
}

#[test]
fn a_stale_version_is_a_conflict_not_a_silent_overwrite() {
    let (s, p) = store("cas");
    s.create_task("t1", None, "nyx", Some("zeus"), None, "build it")
        .unwrap();
    s.transition("t1", None, TaskStatus::Running, None, None)
        .unwrap();
    // Version is 2 now; a coordinator holding 1 must lose.
    let err = s
        .transition("t1", Some(1), TaskStatus::Completed, Some("done"), None)
        .unwrap_err();
    match err {
        TaskMutationError::Conflict(TaskConflict::Version {
            expected, actual, ..
        }) => {
            assert_eq!(expected, 1);
            assert_eq!(actual, Some(2));
        }
        other => panic!("expected a version conflict, got {other:?}"),
    }
    assert_eq!(s.task("t1").unwrap().unwrap().status, TaskStatus::Running);
    drop(s);
    let _ = std::fs::remove_file(p);
}

#[test]
fn assign_honours_the_version_it_is_given() {
    let (s, p) = store("assign-cas");
    s.create_task("t1", None, "nyx", None, None, "unassigned")
        .unwrap();
    // Version 1 is current: this must succeed.
    s.assign("t1", Some(1), "zeus").unwrap();
    // Version 1 is now stale: this must be refused, proving the parameter
    // reached the WHERE clause instead of being ignored.
    let err = s.assign("t1", Some(1), "athena").unwrap_err();
    assert!(
        matches!(
            err,
            TaskMutationError::Conflict(TaskConflict::Version { .. })
        ),
        "a stale assign must lose, got {err:?}"
    );
    assert_eq!(
        s.task("t1").unwrap().unwrap().assigned_agent.as_deref(),
        Some("zeus")
    );
    drop(s);
    let _ = std::fs::remove_file(p);
}

#[test]
fn reassignment_preserves_a_running_task_in_place() {
    let (s, p) = store("reassign");
    s.create_task("t1", None, "nyx", Some("zeus"), None, "build it")
        .unwrap();
    s.transition("t1", None, TaskStatus::Running, None, None)
        .unwrap();
    let t = s.assign("t1", None, "athena").unwrap();
    assert_eq!(t.assigned_agent.as_deref(), Some("athena"));
    assert_eq!(
        t.status,
        TaskStatus::Running,
        "handing work to a different agent must not reset progress"
    );
    drop(s);
    let _ = std::fs::remove_file(p);
}

#[test]
fn two_threads_racing_to_settle_produce_exactly_one_winner() {
    let (s, p) = store("race");
    s.create_task("t1", None, "nyx", Some("zeus"), None, "build it")
        .unwrap();
    s.transition("t1", None, TaskStatus::Running, None, None)
        .unwrap();
    let snapshot = s.task("t1").unwrap().unwrap();
    let v = snapshot.version;
    let store = std::sync::Arc::new(s);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = ["a", "b"]
        .into_iter()
        .map(|tag| {
            let store = std::sync::Arc::clone(&store);
            let barrier = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                let result = format!("result-{tag}");
                store.transition("t1", Some(v), TaskStatus::Completed, Some(&result), None)
            })
        })
        .collect();
    let outcomes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let winners = outcomes.iter().filter(|o| o.is_ok()).count();
    assert_eq!(winners, 1, "exactly one settler may win, got {outcomes:?}");
    let t = store.task("t1").unwrap().unwrap();
    assert_eq!(t.status, TaskStatus::Completed);
    drop(store);
    let _ = std::fs::remove_file(p);
}

#[test]
fn nested_delegation_records_the_parent_link() {
    let (s, p) = store("nested");
    s.create_task(
        "root",
        Some("c1"),
        "nyx",
        Some("zeus"),
        None,
        "build the thing",
    )
    .unwrap();
    let sub = s
        .create_task(
            "root-1",
            Some("c1"),
            "zeus",
            Some("athena"),
            Some("root"),
            "write tests",
        )
        .unwrap();
    assert_eq!(sub.parent_task_id.as_deref(), Some("root"));
    assert_eq!(
        sub.origin_agent, "zeus",
        "the sub-delegator becomes the origin"
    );
    let root = s.task("root").unwrap().unwrap();
    assert_eq!(root.assigned_agent.as_deref(), Some("zeus"));
    drop(s);
    let _ = std::fs::remove_file(p);
}

#[test]
fn invalid_inputs_are_refused_before_touching_the_database() {
    let (s, p) = store("invalid");
    for (id, why) in [
        ("", "empty task id"),
        ("has space", "space in id"),
        ("has/slash", "slash in id"),
    ] {
        let err = s.create_task(id, None, "nyx", None, None, "x").unwrap_err();
        assert_eq!(err.code, "COLLABORATION_INVALID", "{why}");
    }
    // Agents must be slugs, because an agent name is a profile name and it
    // appears in table names and namespaces.
    let err = s
        .create_task("t1", None, "not a slug", None, None, "x")
        .unwrap_err();
    assert_eq!(err.code, "COLLABORATION_INVALID");
    // An empty objective is not a task.
    let err = s
        .create_task("t1", None, "nyx", None, None, "  ")
        .unwrap_err();
    assert_eq!(err.code, "COLLABORATION_INVALID");
    drop(s);
    let _ = std::fs::remove_file(p);
}

// ---------------------------------------------------------------- messages

#[test]
fn a_message_carries_full_provenance() {
    let (s, p) = store("msg");
    s.create_task("t1", Some("c1"), "nyx", Some("zeus"), None, "build it")
        .unwrap();
    let m = s
        .record_message(
            "m1",
            Some("c1"),
            Some("t1"),
            "nyx",
            "zeus",
            MessageKind::Delegation,
            "do the thing",
        )
        .unwrap();
    assert_eq!(m.sender, "nyx");
    assert_eq!(m.recipient, "zeus");
    assert_eq!(m.kind, MessageKind::Delegation);
    assert_eq!(m.source(), "agent:nyx", "provenance source names the agent");
    assert!(m.settled_ms.is_none());
    drop(s);
    let _ = std::fs::remove_file(p);
}

#[test]
fn an_agent_cannot_message_itself() {
    let (s, p) = store("selfmsg");
    let err = s
        .record_message("m1", None, None, "nyx", "nyx", MessageKind::Note, "hi")
        .unwrap_err();
    assert_eq!(err.code, "COLLABORATION_INVALID");
    assert!(err.cause.contains("cannot message itself"));
    drop(s);
    let _ = std::fs::remove_file(p);
}

#[test]
fn the_inbox_is_unread_only_and_settling_is_idempotent() {
    let (s, p) = store("inbox");
    s.record_message(
        "m1",
        None,
        None,
        "nyx",
        "zeus",
        MessageKind::Query,
        "status?",
    )
    .unwrap();
    s.record_message("m2", None, None, "athena", "zeus", MessageKind::Note, "fyi")
        .unwrap();
    assert_eq!(s.inbox("zeus").unwrap().len(), 2);
    assert!(s.settle_message("m1").unwrap(), "first settle does work");
    // A replayed delivery must not double-count or error.
    assert!(!s.settle_message("m1").unwrap(), "second settle is a no-op");
    let inbox = s.inbox("zeus").unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].message_id, "m2");
    // Zeus's own mailbox is untouched by messages addressed elsewhere.
    assert_eq!(s.inbox("nyx").unwrap().len(), 0);
    drop(s);
    let _ = std::fs::remove_file(p);
}

#[test]
fn messages_on_a_task_form_an_ordered_trail() {
    let (s, p) = store("trail");
    s.create_task("t1", Some("c1"), "nyx", Some("zeus"), None, "build it")
        .unwrap();
    s.record_message(
        "m1",
        Some("c1"),
        Some("t1"),
        "nyx",
        "zeus",
        MessageKind::Delegation,
        "go",
    )
    .unwrap();
    s.record_message(
        "m2",
        Some("c1"),
        Some("t1"),
        "zeus",
        "nyx",
        MessageKind::Query,
        "how far?",
    )
    .unwrap();
    s.record_message(
        "m3",
        Some("c1"),
        Some("t1"),
        "nyx",
        "zeus",
        MessageKind::Answer,
        "half",
    )
    .unwrap();
    let trail = s.messages_for_task("t1").unwrap();
    assert_eq!(trail.len(), 3);
    assert_eq!(
        trail.iter().map(|m| m.kind).collect::<Vec<_>>(),
        vec![
            MessageKind::Delegation,
            MessageKind::Query,
            MessageKind::Answer
        ]
    );
    drop(s);
    let _ = std::fs::remove_file(p);
}

#[test]
fn only_the_result_kind_claims_to_be_work_output() {
    // The kind is a closed set precisely so a reader can distinguish work
    // output from chatter without parsing prose.
    assert_ne!(MessageKind::Result, MessageKind::Note);
    assert_ne!(MessageKind::Result, MessageKind::Broadcast);
    let kinds = [
        MessageKind::Delegation,
        MessageKind::Note,
        MessageKind::Query,
        MessageKind::Answer,
        MessageKind::Result,
        MessageKind::Failure,
        MessageKind::Broadcast,
        MessageKind::Reassign,
    ];
    let mut seen = std::collections::HashSet::new();
    for k in kinds {
        assert!(
            seen.insert(k.as_str()),
            "duplicate kind string {}",
            k.as_str()
        );
        // Every kind must round-trip through the database spelling.
        let back = match k.as_str() {
            "delegation" => MessageKind::Delegation,
            "note" => MessageKind::Note,
            "query" => MessageKind::Query,
            "answer" => MessageKind::Answer,
            "result" => MessageKind::Result,
            "failure" => MessageKind::Failure,
            "broadcast" => MessageKind::Broadcast,
            "reassign" => MessageKind::Reassign,
            other => panic!("unmapped {other}"),
        };
        assert_eq!(back, k);
    }
}

// --------------------------------------------------------- collaborations

#[test]
fn a_collaboration_refuses_to_settle_with_unfinished_work() {
    let (s, p) = store("settle-guard");
    s.create_collaboration("c1", "nyx", "ship the feature")
        .unwrap();
    s.create_task("t1", Some("c1"), "nyx", Some("zeus"), None, "part one")
        .unwrap();
    let err = s
        .settle_collaboration("c1", CollaborationStatus::Completed, false)
        .unwrap_err();
    assert_eq!(err.code, "COLLABORATION_NOT_SETTLED");
    assert!(err.cause.contains("1 unfinished"));
    // Force is the deliberate "coordinator vanished" path.
    let c = s
        .settle_collaboration("c1", CollaborationStatus::Cancelled, true)
        .unwrap();
    assert_eq!(c.status, CollaborationStatus::Cancelled);
    drop(s);
    let _ = std::fs::remove_file(p);
}

#[test]
fn a_collaboration_settles_once_every_task_is_terminal() {
    let (s, p) = store("settle");
    s.create_collaboration("c1", "nyx", "ship it").unwrap();
    s.create_task("t1", Some("c1"), "nyx", Some("zeus"), None, "a")
        .unwrap();
    s.create_task("t2", Some("c1"), "nyx", Some("athena"), None, "b")
        .unwrap();
    s.transition("t1", None, TaskStatus::Completed, Some("a done"), None)
        .unwrap();
    s.transition("t2", None, TaskStatus::Failed, None, Some("b broke"))
        .unwrap();
    let c = s
        .settle_collaboration("c1", CollaborationStatus::Completed, false)
        .unwrap();
    assert_eq!(c.status, CollaborationStatus::Completed);
    assert!(s.active_collaborations().unwrap().is_empty());
    drop(s);
    let _ = std::fs::remove_file(p);
}

#[test]
fn settling_a_settled_collaboration_is_refused() {
    let (s, p) = store("settle-twice");
    s.create_collaboration("c1", "nyx", "x").unwrap();
    s.settle_collaboration("c1", CollaborationStatus::Completed, true)
        .unwrap();
    let err = s
        .settle_collaboration("c1", CollaborationStatus::Cancelled, true)
        .unwrap_err();
    assert_eq!(err.code, "COLLABORATION_CONFLICT");
    drop(s);
    let _ = std::fs::remove_file(p);
}

// ---------------------------------------------------------------- recovery

#[test]
fn tasks_in_flight_at_a_crash_are_recoverable() {
    let path = tmp_db("recover");
    {
        let s = CollaborationStore::open(&path).unwrap();
        s.create_task("done", None, "nyx", Some("zeus"), None, "finished")
            .unwrap();
        s.transition("done", None, TaskStatus::Completed, Some("ok"), None)
            .unwrap();
        s.create_task("lost", None, "nyx", Some("athena"), None, "interrupted")
            .unwrap();
        s.transition("lost", None, TaskStatus::Running, None, None)
            .unwrap();
        // No settle: the process "died" here.
    }
    // A fresh process must see the interrupted task and offer it again.
    let s = CollaborationStore::open(&path).unwrap();
    let orphans = s.orphaned_tasks().unwrap();
    assert_eq!(orphans.len(), 1, "only the unsettled task is orphaned");
    assert_eq!(orphans[0].task_id, "lost");
    assert!(orphans[0].is_orphaned());
    // The completed task is untouched and not offered for retry.
    assert_eq!(
        s.task("done").unwrap().unwrap().status,
        TaskStatus::Completed
    );
    drop(s);
    let _ = std::fs::remove_file(path);
}

#[test]
fn an_unsettled_message_survives_a_restart() {
    let path = tmp_db("msg-recover");
    {
        let s = CollaborationStore::open(&path).unwrap();
        s.record_message(
            "m1",
            None,
            None,
            "nyx",
            "zeus",
            MessageKind::Query,
            "status?",
        )
        .unwrap();
        s.record_message(
            "m2",
            None,
            None,
            "nyx",
            "zeus",
            MessageKind::Query,
            "still there?",
        )
        .unwrap();
        s.settle_message("m1").unwrap();
    }
    let s = CollaborationStore::open(&path).unwrap();
    let inbox = s.inbox("zeus").unwrap();
    assert_eq!(
        inbox.len(),
        1,
        "the delivered-but-unrecorded message replays"
    );
    assert_eq!(inbox[0].message_id, "m2");
    drop(s);
    let _ = std::fs::remove_file(path);
}

#[test]
fn collaboration_state_survives_a_restart_intact() {
    let path = tmp_db("persist");
    {
        let s = CollaborationStore::open(&path).unwrap();
        s.create_collaboration("c1", "nyx", "objective").unwrap();
        s.create_task("t1", Some("c1"), "nyx", Some("zeus"), Some("root"), "work")
            .unwrap();
        s.record_message(
            "m1",
            Some("c1"),
            Some("t1"),
            "nyx",
            "zeus",
            MessageKind::Delegation,
            "go",
        )
        .unwrap();
    }
    let s = CollaborationStore::open(&path).unwrap();
    let c = s.collaboration("c1").unwrap().unwrap();
    assert_eq!(c.coordinator, "nyx");
    assert_eq!(c.status, CollaborationStatus::Active);
    let tasks = s.tasks_in_collaboration("c1").unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].parent_task_id.as_deref(), Some("root"));
    assert_eq!(s.messages_for_task("t1").unwrap().len(), 1);
    drop(s);
    let _ = std::fs::remove_file(path);
}

#[test]
fn a_second_store_on_the_same_file_does_not_corrupt_the_first() {
    let path = tmp_db("twostores");
    let a = CollaborationStore::open(&path).unwrap();
    a.create_task("t1", None, "nyx", Some("zeus"), None, "work")
        .unwrap();
    let b = CollaborationStore::open(&path).unwrap();
    // Two writers, one row: the CAS is the only thing keeping the version
    // monotonic, so a write from b must not resurrect a stale state.
    b.transition("t1", None, TaskStatus::Running, None, None)
        .unwrap();
    let from_a = a.task("t1").unwrap().unwrap();
    assert_eq!(from_a.status, TaskStatus::Running);
    assert_eq!(from_a.version, 2);
    drop(a);
    drop(b);
    let _ = std::fs::remove_file(path);
}

#[test]
fn querying_an_unknown_task_or_message_is_none_not_an_error() {
    let (s, p) = store("absent");
    assert!(s.task("nope").unwrap().is_none());
    assert!(s.message("nope").unwrap().is_none());
    assert!(s.collaboration("nope").unwrap().is_none());
    assert!(s.tasks_for_agent("ghost", None).unwrap().is_empty());
    drop(s);
    let _ = std::fs::remove_file(p);
}

#[test]
fn an_agents_task_list_is_scoped_to_that_agent() {
    let (s, p) = store("scoped");
    s.create_task("z1", None, "nyx", Some("zeus"), None, "zeus work")
        .unwrap();
    s.create_task("a1", None, "nyx", Some("athena"), None, "athena work")
        .unwrap();
    s.create_task("both", None, "nyx", Some("zeus"), None, "shared")
        .unwrap();
    s.assign("both", None, "athena").unwrap();
    let zeus = s.tasks_for_agent("zeus", None).unwrap();
    assert_eq!(zeus.len(), 1, "Zeus must not see Athena's work");
    assert_eq!(zeus[0].task_id, "z1");
    let athena = s.tasks_for_agent("athena", None).unwrap();
    assert_eq!(athena.len(), 2);
    let running = s
        .tasks_for_agent("zeus", Some(TaskStatus::Assigned))
        .unwrap();
    assert_eq!(running.len(), 1);
    drop(s);
    let _ = std::fs::remove_file(p);
}

#[test]
fn the_transition_table_is_exhaustive_about_terminal_states() {
    use TaskStatus::*;
    let all = [
        Pending, Assigned, Running, Blocked, Completed, Failed, Cancelled,
    ];
    for from in all {
        for to in all {
            let legal = from.can_transition_to(to);
            if from.is_terminal() {
                // A terminal task has exactly two legal targets: itself
                // (an idempotent re-settle) and, for Failed only, Pending
                // (the explicit retry). Nothing else may follow a result
                // that has already been reported.
                let allowed = to == from || (from == Failed && to == Pending);
                assert_eq!(
                    legal, allowed,
                    "{from} -> {to}: a terminal task must not reach any other status"
                );
            }
        }
    }
    // The live states all reach every terminal state, so an agent working
    // on a task can always finish, fail, or be cancelled out of it.
    for from in [Pending, Assigned, Running, Blocked] {
        for to in [Completed, Failed, Cancelled] {
            assert!(
                from.can_transition_to(to),
                "{from} must be able to reach {to}"
            );
        }
    }
}
