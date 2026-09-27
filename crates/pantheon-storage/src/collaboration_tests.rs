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
