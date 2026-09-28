//! Behavioral tests for first-class session migration: foreign transcripts
//! become real, resumable ledger runs. Runs under `cargo test -p
//! pantheon-eval`, not beside the code.
use pantheon_api::message::Role;
use pantheon_migration::session_import::{
    import_run_id, import_session_dirs, import_session_transcript, parse_session_transcript,
    ImportStatus, TurnRole,
};
use pantheon_runtime::session::{rebuild_messages, rebuild_transcript, TranscriptItem};
use pantheon_storage::Ledger;
use std::fs;
use std::path::{Path, PathBuf};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pantheon-mig-sess-{}-{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn ledger_at(dir: &Path) -> Ledger {
    Ledger::open(&dir.join("ledger.db")).unwrap()
}

/// A Hermes-style transcript: flat roles, one thinking part, one tool record.
fn write_hermes(dir: &Path) -> PathBuf {
    let sessions = dir.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let p = sessions.join("abc123.jsonl");
    fs::write(
        &p,
        concat!(
            "{\"role\":\"user\",\"content\":\"how do I configure the router\"}\n",
            "{\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":\"the user wants router help\"},{\"type\":\"text\",\"text\":\"set base_url\"}]}\n",
            "{\"role\":\"tool\",\"content\":\"tool output nobody replays\"}\n",
            "{\"role\":\"user\",\"content\":\"and the timeout?\"}\n",
            "{\"role\":\"assistant\",\"content\":\"set timeout_ms\"}\n",
        ),
    )
    .unwrap();
    p
}

#[test]
fn parse_extracts_ordered_turns_and_reasoning() {
    let d = tmp("parse");
    let p = write_hermes(&d);
    let (turns, dropped) = parse_session_transcript(&p);
    assert_eq!(dropped, 1, "tool record is counted as dropped");
    let roles: Vec<TurnRole> = turns.iter().map(|t| t.role).collect();
    assert_eq!(
        roles,
        vec![
            TurnRole::User,
            TurnRole::Reasoning,
            TurnRole::Assistant,
            TurnRole::User,
            TurnRole::Assistant,
        ]
    );
    assert!(turns[0].text.contains("configure the router"));
    assert!(turns[1].text.contains("the user wants router help"));
    assert!(turns[2].text.contains("set base_url"));
}

#[test]
fn parse_understands_claude_envelopes() {
    let d = tmp("claude");
    let p = d.join("sess.jsonl");
    fs::write(
        &p,
        concat!(
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"explain ports\"}]}}\n",
            "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":\"ports thinking\"},{\"type\":\"text\",\"text\":\"ports answer\"},{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"read\"}]}}\n",
        ),
    )
    .unwrap();
    let (turns, dropped) = parse_session_transcript(&p);
    assert_eq!(dropped, 1, "envelope tool_use is dropped");
    let roles: Vec<TurnRole> = turns.iter().map(|t| t.role).collect();
    assert_eq!(
        roles,
        vec![TurnRole::User, TurnRole::Reasoning, TurnRole::Assistant]
    );
    assert!(turns[1].text.contains("ports thinking"));
    assert!(turns[2].text.contains("ports answer"));
}

#[test]
fn parse_handles_whole_file_json() {
    let d = tmp("json");
    let p = d.join("s.json");
    fs::write(
        &p,
        "{\"messages\":[{\"role\":\"user\",\"content\":\"hi\"},{\"role\":\"assistant\",\"content\":\"hello\"}]}",
    )
    .unwrap();
    let (turns, dropped) = parse_session_transcript(&p);
    assert_eq!(dropped, 0);
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].role, TurnRole::User);
    // A single-record JSONL line is one record, not an empty whole-file parse.
    let q = d.join("one.jsonl");
    fs::write(&q, "{\"role\":\"user\",\"content\":\"solo\"}\n").unwrap();
    let (turns, _) = parse_session_transcript(&q);
    assert_eq!(turns.len(), 1);
}

#[test]
fn run_id_is_deterministic() {
    let a = import_run_id("hermes", Path::new("/x/sessions/abc123.jsonl"));
    let b = import_run_id("hermes", Path::new("/x/sessions/abc123.jsonl"));
    assert_eq!(a, b);
    assert_eq!(a, "imported:hermes:sessions:abc123");
    // A different parent dir does not collide.
    let c = import_run_id("hermes", Path::new("/x/other/abc123.jsonl"));
    assert_ne!(a, c);
}

#[test]
fn import_creates_a_resumable_run() {
    let d = tmp("import");
    let sessions = d.join("sessions");
    let p = write_hermes(&d);
    let ledger = ledger_at(&d);

    let r = import_session_transcript(&ledger, "hermes", &p, 1_700_000_000_000).unwrap();
    assert_eq!(r.status, ImportStatus::Imported);
    assert_eq!(r.run_id, "imported:hermes:sessions:abc123");
    assert_eq!(r.user_turns, 2);
    assert_eq!(r.assistant_turns, 2);
    assert_eq!(r.reasoning_traces, 1);
    assert_eq!(r.dropped_tool_records, 1);

    // Resumable: a finished run row with a title, visible to /resume.
    assert_eq!(
        ledger.status(&r.run_id).unwrap().as_deref(),
        Some("completed")
    );
    assert_eq!(
        ledger.run_title(&r.run_id).unwrap().as_deref(),
        Some("how do I configure the router")
    );
    let runs = ledger.list_runs(50).unwrap();
    assert!(runs.iter().any(|l| l.0 == r.run_id), "run is listed");

    // Turn order is preserved through replay.
    let entries = ledger.replay(&r.run_id).unwrap();
    let msgs = rebuild_messages(entries);
    let roles: Vec<Role> = msgs.iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        vec![Role::User, Role::Assistant, Role::User, Role::Assistant]
    );
    assert!(msgs[0].content.contains("configure the router"));
    assert!(msgs[1].content.contains("set base_url"));
    assert!(
        !msgs[1].content.contains("router help"),
        "reasoning is not a message"
    );

    // Reasoning survives replay, in order, as a transcript item.
    let entries = ledger.replay(&r.run_id).unwrap();
    let items = rebuild_transcript(entries);
    assert_eq!(items.len(), 5);
    assert!(matches!(items[0], TranscriptItem::Message(_)));
    assert!(
        matches!(&items[1], TranscriptItem::Reasoning(t) if t.contains("the user wants router help")),
        "reasoning trace preserved in order"
    );

    // Provenance: source, original id, and import timestamp are on the run.
    let entries = ledger.replay(&r.run_id).unwrap();
    let prov = entries.iter().find_map(|e| match &e.event {
        pantheon_api::events::Event::RunProgress { detail, .. }
            if detail.starts_with("imported from") =>
        {
            Some(detail.clone())
        }
        _ => None,
    });
    let prov = prov.expect("provenance RunProgress event");
    assert!(prov.contains("hermes"), "{prov}");
    assert!(prov.contains("original_id=abc123"), "{prov}");
    assert!(prov.contains("imported_at_ms=1700000000000"), "{prov}");

    // The tool drop is disclosed on the run.
    let drop_note = entries.iter().find_map(|e| match &e.event {
        pantheon_api::events::Event::RunProgress { detail, .. }
            if detail.starts_with("dropped") =>
        {
            Some(detail.clone())
        }
        _ => None,
    });
    assert!(
        drop_note
            .map(|s| s.contains("1 tool record(s)"))
            .unwrap_or(false),
        "dropped tool traffic is disclosed"
    );
    let _ = sessions;
}

#[test]
fn import_twice_creates_no_duplicates() {
    let d = tmp("idem");
    let p = write_hermes(&d);
    let ledger = ledger_at(&d);

    let first = import_session_transcript(&ledger, "hermes", &p, 1).unwrap();
    assert_eq!(first.status, ImportStatus::Imported);
    let second = import_session_transcript(&ledger, "hermes", &p, 2).unwrap();
    assert_eq!(second.status, ImportStatus::SkippedExists);
    assert_eq!(first.run_id, second.run_id);

    // No duplicated messages after the second import.
    let entries = ledger.replay(&first.run_id).unwrap();
    let msgs = rebuild_messages(entries);
    assert_eq!(msgs.len(), 4, "re-import must not duplicate turns");
}

#[test]
fn import_session_dirs_batches_a_directory() {
    let d = tmp("batch");
    write_hermes(&d);
    let ledger = ledger_at(&d);
    let batch = import_session_dirs(&ledger, "hermes", &[d.join("sessions")], 7);
    assert!(batch.failures.is_empty());
    assert_eq!(batch.reports.len(), 1);
    assert_eq!(batch.reports[0].status, ImportStatus::Imported);
    // Second batch converges.
    let batch2 = import_session_dirs(&ledger, "hermes", &[d.join("sessions")], 8);
    assert!(batch2.failures.is_empty());
    assert_eq!(batch2.reports[0].status, ImportStatus::SkippedExists);
}

#[test]
fn empty_transcript_is_skipped() {
    let d = tmp("empty");
    let sessions = d.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let p = sessions.join("blank.jsonl");
    fs::write(&p, "\n   \nnot json at all\n").unwrap();
    let ledger = ledger_at(&d);
    let r = import_session_transcript(&ledger, "hermes", &p, 1).unwrap();
    assert_eq!(r.status, ImportStatus::SkippedEmpty);
    assert_eq!(
        ledger.status(&r.run_id).unwrap(),
        None,
        "no run row written"
    );
}
