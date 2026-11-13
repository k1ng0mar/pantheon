//! A panicking sub-agent settles as Failed instead of hanging its waiter
//! (audit COR-05), and output compaction never panics on multi-byte text
//! at the byte cap (audit COR-03).

#![allow(clippy::result_large_err)] // the workspace allows it; eval does not inherit lints

use pantheon_agent::subagent::{SubagentCaps, SubagentRegistry, SubagentSpawner};
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn panicking_child_fails_instead_of_hanging_the_waiter() {
    let reg = std::sync::Arc::new(SubagentRegistry::new(SubagentCaps::default()));
    let handle = reg
        .spawn_child("root", "worker", 0, "explode", || panic!("boom in child"))
        .expect("spawn");
    let (tx, rx) = mpsc::channel();
    let r = std::sync::Arc::clone(&reg);
    std::thread::spawn(move || {
        let _ = tx.send(r.subagent_wait(&handle.id));
    });
    let result = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("subagent_wait must return, not hang, when the child panics");
    let err = result.expect_err("a panicked child is a failure");
    assert!(
        err.cause.contains("SUBAGENT_PANIC") || err.code == "SUBAGENT_PANIC",
        "{err:?}"
    );
    assert!(err.cause.contains("boom in child"), "{err:?}");
}

#[test]
fn compaction_cuts_on_a_char_boundary() {
    let policy = pantheon_exec::CompactionPolicy {
        max_lines: 4,
        head_lines: 2,
        max_bytes: 102, // head line 1 is 92 bytes + \n; byte 102 is inside a 3-byte char of line 2
    };
    // Many lines of 3-byte characters: the byte cap lands mid-character.
    let raw: String = (0..50)
        .map(|i| format!("{i} {}\n", "界".repeat(30)))
        .collect();
    let out = pantheon_exec::compact_output(&raw, &policy);
    assert!(out.truncated);
    assert!(out.text.ends_with("[... byte-cap ...]"));
}
