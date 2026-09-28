//! Behavioral tests for the migration index plane: transcript indexing,
//! quarantine, and sessions import against temp-dir fixture trees. Moved
//! here from `pantheon-migration/src/index_tests.rs`; runs under
//! `cargo test -p pantheon-eval`, not beside the code.
use pantheon_migration::*;
use std::fs;
use std::path::{Path, PathBuf};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pantheon-idx-{}-{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn parses_a_string_content_transcript() {
    let d = tmp("str");
    let p = d.join("s.jsonl");
    fs::write(
        &p,
        "{\"role\":\"user\",\"content\":\"how do I configure the router\"}\n\
         {\"role\":\"assistant\",\"content\":\"set base_url\"}\n",
    )
    .unwrap();
    let c = parse_transcript(&p, "hermes", "s");
    assert_eq!(c.len(), 2);
    assert_eq!(c[0].kind, "message");
    assert_eq!(c[0].seq, 0);
    assert!(c[0].text.contains("configure the router"));
}

#[test]
fn parses_openai_style_content_parts() {
    let d = tmp("parts");
    let p = d.join("s.jsonl");
    fs::write(
        &p,
        "{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"first part\"},{\"type\":\"text\",\"text\":\"second part\"}]}\n",
    )
    .unwrap();
    let c = parse_transcript(&p, "hermes", "s");
    assert_eq!(c.len(), 1);
    assert!(c[0].text.contains("first part"));
    assert!(c[0].text.contains("second part"));
}

#[test]
fn a_nested_message_object_is_flattened() {
    let d = tmp("nested");
    let p = d.join("s.jsonl");
    fs::write(
        &p,
        "{\"message\":{\"role\":\"user\",\"content\":\"nested body text\"}}\n",
    )
    .unwrap();
    let c = parse_transcript(&p, "hermes", "s");
    assert_eq!(c.len(), 1);
    assert!(c[0].text.contains("nested body text"));
}

#[test]
fn a_tool_record_is_classified_as_a_tool() {
    let d = tmp("tool");
    let p = d.join("s.jsonl");
    fs::write(
        &p,
        "{\"role\":\"tool\",\"content\":\"ran the migration\"}\n\
         {\"kind\":\"title\",\"text\":\"Session about migration\"}\n",
    )
    .unwrap();
    let c = parse_transcript(&p, "hermes", "s");
    assert_eq!(c[0].kind, "tool");
    assert_eq!(c[1].kind, "title");
}

#[test]
fn migrated_chunks_are_namespaced_away_from_real_runs() {
    let d = tmp("ns");
    let p = d.join("s.jsonl");
    fs::write(&p, "{\"role\":\"user\",\"content\":\"x\"}\n").unwrap();
    let c = parse_transcript(&p, "hermes", "sess7");
    // A real run id starts with run_/run-; a migrated one must not.
    assert!(c[0].run_id.starts_with("migrated:"), "{}", c[0].run_id);
    assert_eq!(c[0].run_id, "migrated:hermes:sess7");
    assert!(!c[0].run_id.starts_with("run_"));
}

#[test]
fn chunk_ids_are_stable_so_reindexing_is_idempotent() {
    let d = tmp("stable");
    let p = d.join("s.jsonl");
    fs::write(
        &p,
        "{\"role\":\"user\",\"content\":\"a\"}\n{\"role\":\"user\",\"content\":\"b\"}\n",
    )
    .unwrap();
    let first = parse_transcript(&p, "hermes", "s");
    let second = parse_transcript(&p, "hermes", "s");
    assert_eq!(first, second, "the same file must yield the same chunk ids");
    assert_ne!(first[0].chunk_id, first[1].chunk_id);
    assert!(first[0].chunk_id.ends_with(":0"));
    assert!(first[1].chunk_id.ends_with(":1"));
}

#[test]
fn a_malformed_or_truncated_line_is_skipped_not_fatal() {
    let d = tmp("malformed");
    let p = d.join("s.jsonl");
    fs::write(
        &p,
        "{\"role\":\"user\",\"content\":\"good one\"}\n\
         {not json at all\n\
         \n\
         {\"role\":\"assistant\",\"content\":\"good two\"}\n\
         {\"role\":\"user\",\"content\":\"truncated",
    )
    .unwrap();
    let c = parse_transcript(&p, "hermes", "s");
    assert_eq!(c.len(), 2, "the two good lines survive: {c:#?}");
    assert!(c[0].text.contains("good one"));
    assert!(c[1].text.contains("good two"));
}

#[test]
fn an_empty_or_contentless_record_contributes_nothing() {
    let d = tmp("empty");
    let p = d.join("s.jsonl");
    fs::write(
        &p,
        "{\"role\":\"assistant\",\"content\":\"\"}\n{\"role\":\"user\"}\n",
    )
    .unwrap();
    assert!(parse_transcript(&p, "hermes", "s").is_empty());
}

#[test]
fn the_quarantine_path_is_the_one_apply_writes() {
    assert_eq!(
        quarantine_dir(Path::new("/d"), "hermes"),
        PathBuf::from("/d/imported-sessions/hermes")
    );
}

// ---------------------------------------------------------------------------
// ensure_sessions_indexed: index failure is import failure
// ---------------------------------------------------------------------------
