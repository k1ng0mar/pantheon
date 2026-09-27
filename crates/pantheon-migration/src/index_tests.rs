//! Tests for turning quarantined transcripts into searchable chunks.
use super::*;
use std::fs;
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pantheon-idx-{}-{}", name, std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn write(d: &Path, rel: &str, body: &str) {
    let p = d.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
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
fn a_huge_tool_result_is_truncated() {
    let d = tmp("huge");
    let p = d.join("s.jsonl");
    let big = "x".repeat(MAX_CHUNK_CHARS + 500);
    fs::write(&p, format!("{{\"role\":\"tool\",\"content\":\"{big}\"}}\n")).unwrap();
    let c = parse_transcript(&p, "hermes", "s");
    assert!(c[0].text.chars().count() <= MAX_CHUNK_CHARS + 1, "ellipsis");
    assert!(c[0].text.ends_with('…'));
}

#[test]
fn indexing_writes_rows_and_reports_empty_files() {
    let d = tmp("index");
    let q = d.join("imported-sessions/hermes");
    write(
        &q,
        "a.jsonl",
        "{\"role\":\"user\",\"content\":\"alpha term\"}\n",
    );
    write(
        &q,
        "b.jsonl",
        "{\"role\":\"user\",\"content\":\"beta term\"}\n",
    );
    write(&q, "blank.jsonl", "\n\n");
    write(&q, "manifest.json", "{\"files\":[]}");

    let idx = SessionIndex::open_in_memory().unwrap();
    let r = index_quarantine(&q, "hermes", &idx).unwrap();
    assert_eq!(r.files, 2, "{r:?}");
    assert_eq!(r.chunks, 2);
    assert_eq!(r.empty_files.len(), 1, "the blank file must be named");
    assert!(r.empty_files[0].ends_with("blank.jsonl"));
    // manifest.json is not a transcript and must not be counted.
    assert!(!r.empty_files.iter().any(|f| f.ends_with("manifest.json")));
}

#[test]
fn indexing_twice_does_not_duplicate_rows() {
    let d = tmp("idem");
    let q = d.join("imported-sessions/hermes");
    write(
        &q,
        "a.jsonl",
        "{\"role\":\"user\",\"content\":\"same text here\"}\n",
    );

    let idx = SessionIndex::open_in_memory().unwrap();
    let first = index_quarantine(&q, "hermes", &idx).unwrap();
    let second = index_quarantine(&q, "hermes", &idx).unwrap();
    assert_eq!(first.chunks, second.chunks);

    let hits = idx.search("same text here", 50).unwrap();
    assert_eq!(hits.len(), 1, "re-indexing must converge, not duplicate");
}

#[test]
fn an_indexed_migration_is_actually_searchable() {
    let d = tmp("searchable");
    let q = d.join("imported-sessions/hermes");
    write(
        &q,
        "s.jsonl",
        "{\"role\":\"user\",\"content\":\"how do I rotate the mcp oauth token\"}\n",
    );
    let idx = SessionIndex::open_in_memory().unwrap();
    index_quarantine(&q, "hermes", &idx).unwrap();
    let hits = idx.search("mcp oauth token", 10).unwrap();
    assert!(!hits.is_empty(), "a migrated transcript must be findable");
    assert!(hits[0].chunk.text.contains("rotate the mcp oauth token"));
    assert!(hits[0].chunk.run_id.starts_with("migrated:hermes:"));
}

#[test]
fn a_missing_quarantine_dir_is_empty_not_an_error() {
    let d = tmp("missing");
    let idx = SessionIndex::open_in_memory().unwrap();
    let r = index_quarantine(&d.join("nope"), "hermes", &idx).unwrap();
    assert_eq!(r.files, 0);
    assert_eq!(r.chunks, 0);
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

fn quarantine_with_transcript(d: &Path) -> PathBuf {
    let q = d.join("data").join("imported-sessions").join("hermes");
    fs::create_dir_all(&q).unwrap();
    fs::write(
        q.join("s.jsonl"),
        "{\"role\":\"user\",\"content\":\"how do I configure the router\"}\n",
    )
    .unwrap();
    q
}

#[test]
fn indexed_sessions_land_in_the_report() {
    let d = tmp("idx-ok");
    quarantine_with_transcript(&d);
    let t = crate::Targets::new(d.join("data"), d.join("ext"));
    let index = SessionIndex::open_in_memory().unwrap();
    let r = ensure_sessions_indexed(&t, "hermes", &index).unwrap();
    assert_eq!(r.files, 1);
    assert_eq!(r.chunks, 1);
}

#[test]
fn no_quarantine_dir_is_not_a_failure() {
    let d = tmp("idx-empty");
    let t = crate::Targets::new(d.join("data"), d.join("ext"));
    let index = SessionIndex::open_in_memory().unwrap();
    let r = ensure_sessions_indexed(&t, "hermes", &index).unwrap();
    assert_eq!(r.files, 0);
    assert_eq!(r.chunks, 0);
}

#[test]
fn index_failure_is_reported_as_failure_never_success() {
    // Sabotage the schema out from under the index with a second
    // connection: the next chunk write must fail.
    let d = tmp("idx-fail");
    quarantine_with_transcript(&d);
    let db = d.join("ledger.db");
    let index = SessionIndex::open(&db).unwrap();
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("DROP TABLE session_chunks; DROP TABLE session_fts;")
            .unwrap();
    }
    let t = crate::Targets::new(d.join("data"), d.join("ext"));
    let err =
        ensure_sessions_indexed(&t, "hermes", &index).expect_err("the index write must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("MIGRATE_SESS_INDEX_FAILED"),
        "the failure must be unmistakable: {msg}"
    );
    assert!(
        msg.contains("not complete"),
        "it must never read as success: {msg}"
    );
}
