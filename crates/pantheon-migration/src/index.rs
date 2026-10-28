//! Making the session quarantine live.
//!
//! `migrate apply` copies a source's transcripts into
//! `<data_dir>/imported-sessions/<source>/` and stops there - a file nobody
//! reads. This module closes that: it turns those quarantined transcripts into
//! [`SessionChunk`]s and indexes them into the same `session_search` store the
//! live runtime writes, so a migrated history is actually searchable.
//!
//! Two rules keep the import from lying to the operator:
//!
//! - **A migrated chunk is namespaced.** `run_id` is
//!   `migrated:<source>:<session>`, which cannot collide with a real `run_*`
//!   id, and `seq` is the record's line number. A hit points at the quarantined
//!   file, not at a ledger event that does not exist.
//! - **Indexing is idempotent.** `chunk_id` is derived from
//!   (source, file, line), and `index()` is `INSERT OR REPLACE`, so
//!   re-indexing converges instead of duplicating rows.
//!
//! The chunk text is only what a search would match on. It is never replayed
//! into a session and never executed.

use pantheon_api::error::{Layer, PantheonError};
use pantheon_storage::search::{SessionChunk, SessionSearch as SessionIndex};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One chunk pulled out of a quarantined transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportedChunk {
    pub chunk_id: String,
    pub run_id: String,
    pub seq: i64,
    /// `message`, `title`, or `tool` - the same vocabulary the live path uses.
    pub kind: String,
    pub text: String,
    pub ts_ms: i64,
}

fn serr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Storage,
        false,
        cause,
        "check the quarantine dir and that the ledger is not locked",
        "see `pantheon migrate apply` output",
    )
}

/// A JSONL transcript record, tolerating the shapes real agents write.
#[derive(Debug, Deserialize)]
struct Record {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    timestamp: Option<serde_json::Value>,
    /// `content` may be a string or an OpenAI-style parts array.
    #[serde(default)]
    content: Option<serde_json::Value>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    message: Option<serde_json::Value>,
}

/// Flatten a `content` value to searchable text. Handles a bare string, an
/// array of `{type, text}` parts, and an object with a `text` field.
fn content_to_text(v: &serde_json::Value, out: &mut String) {
    match v {
        serde_json::Value::String(s) => {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(s);
        }
        serde_json::Value::Array(items) => {
            for it in items {
                content_to_text(it, out);
            }
        }
        serde_json::Value::Object(map) => {
            if let Some(serde_json::Value::String(t)) = map.get("text") {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(t);
            } else if let Some(inner) = map.get("content") {
                // A nested envelope: `{"message":{"role":..,"content":..}}`
                // and `{"content":{"text":..}}` both land here.
                content_to_text(inner, out);
            }
        }
        _ => {}
    }
}

fn timestamp_ms(v: Option<&serde_json::Value>) -> i64 {
    match v {
        Some(serde_json::Value::Number(n)) => n.as_i64().unwrap_or(0),
        // An ISO-8601 string is left to the caller's ordering; 0 sorts oldest,
        // which is the safe default for a recency-weighted search.
        _ => 0,
    }
}

/// The longest text a single chunk will index. A tool result can be enormous
/// and would otherwise dominate the FTS table.
const MAX_CHUNK_CHARS: usize = 4000;

/// One parsed record as a chunk, or `None` when it carries no text.
/// Shared by the JSONL line path and the whole-file `.json` path so
/// both shapes index identically.
fn record_to_chunk(rec: &Record, run_id: &str, stem: &str, seq: i64) -> Option<ImportedChunk> {
    // Text, in the order the shapes appear in practice.
    let mut text = String::new();
    if let Some(s) = &rec.text {
        text.push_str(s);
    }
    if let Some(c) = &rec.content {
        content_to_text(c, &mut text);
    }
    if let Some(m) = &rec.message {
        content_to_text(m, &mut text);
    }
    let text = text.trim().to_string();
    if text.is_empty() {
        return None;
    }
    let text = if text.chars().count() > MAX_CHUNK_CHARS {
        let cut: String = text.chars().take(MAX_CHUNK_CHARS).collect();
        format!("{cut}...")
    } else {
        text
    };

    let kind = match rec.kind.as_deref() {
        Some("tool") | Some("tool_call") | Some("ToolResult") => "tool",
        Some("title") => "title",
        _ => match rec.role.as_deref() {
            Some("title") => "title",
            Some("tool") | Some("tool_call") => "tool",
            _ => "message",
        },
    };

    Some(ImportedChunk {
        // Stable across re-runs, so indexing is idempotent.
        chunk_id: format!("{run_id}#{stem}:{seq}"),
        run_id: run_id.to_string(),
        seq,
        kind: kind.to_string(),
        text,
        ts_ms: timestamp_ms(rec.timestamp.as_ref()),
    })
}

/// Parse one quarantined transcript into chunks: JSONL (one record per
/// line) or whole-file `.json` (an array of records, a `{"messages":
/// [...]}` object, or a single pretty-printed record object).
///
/// `carry` copies `.json` transcripts into quarantine too, so the
/// indexer must read them - filtering to `.jsonl` only left every
/// `.json` transcript unindexed and invisible (not even listed in
/// `empty_files`). Malformed lines are skipped rather than failing the
/// import: a truncated final line in a live transcript is normal, not
/// corruption.
pub fn parse_transcript(path: &Path, source: &str, session: &str) -> Vec<ImportedChunk> {
    let Ok(body) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let run_id = format!("migrated:{source}:{session}");
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "t".into());
    let trimmed = body.trim();
    // Whole-file JSON first: a pretty-printed `.json` transcript spans
    // many lines, so line parsing would see only fragments of one
    // record and yield nothing.
    if trimmed.starts_with('[') || trimmed.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            let records: Vec<serde_json::Value> = match &v {
                serde_json::Value::Array(items) => items.clone(),
                serde_json::Value::Object(map) => match map.get("messages") {
                    Some(serde_json::Value::Array(items)) => items.clone(),
                    _ => vec![v.clone()],
                },
                _ => Vec::new(),
            };
            let mut out = Vec::new();
            for (i, item) in records.iter().enumerate() {
                if let Ok(rec) = serde_json::from_value::<Record>(item.clone()) {
                    if let Some(chunk) = record_to_chunk(&rec, &run_id, &stem, i as i64) {
                        out.push(chunk);
                    }
                }
            }
            if !out.is_empty() {
                return out;
            }
        }
    }
    let mut out = Vec::new();
    for (i, line) in body.lines().enumerate() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<Record>(t) else {
            continue;
        };
        if let Some(chunk) = record_to_chunk(&rec, &run_id, &stem, i as i64) {
            out.push(chunk);
        }
    }
    out
}

impl From<ImportedChunk> for SessionChunk {
    fn from(c: ImportedChunk) -> Self {
        SessionChunk {
            chunk_id: c.chunk_id,
            run_id: c.run_id,
            seq: c.seq,
            kind: c.kind,
            text: c.text,
            ts_ms: c.ts_ms,
        }
    }
}

/// What one indexing run did.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexReport {
    pub source: String,
    pub files: usize,
    pub chunks: usize,
    /// Files that produced nothing, named so an unreadable transcript is
    /// visible rather than silently contributing zero rows.
    pub empty_files: Vec<String>,
}

/// Parse every transcript under a quarantine dir and index it.
///
/// `index` is the live store, so a migrated history becomes searchable through
/// the same `session_search` tool the agent already has. Pass a fresh
/// `SessionIndex::open_in_memory()` to dry-run the parse without writing.
pub fn index_quarantine(
    quarantine_dir: &Path,
    source: &str,
    index: &SessionIndex,
) -> Result<IndexReport, PantheonError> {
    let mut report = IndexReport {
        source: source.to_string(),
        ..Default::default()
    };
    let Ok(rd) = std::fs::read_dir(quarantine_dir) else {
        return Ok(report);
    };
    let mut paths: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p
                    .extension()
                    .map(|e| e == "jsonl" || e == "json")
                    .unwrap_or(false)
                && p.file_name().map(|n| n != "manifest.json").unwrap_or(false)
        })
        .collect();
    // Stable order, so a report is comparable run to run.
    paths.sort();

    for p in paths {
        let session = p
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "t".into());
        let chunks = parse_transcript(&p, source, &session);
        if chunks.is_empty() {
            report.empty_files.push(p.to_string_lossy().to_string());
            continue;
        }
        report.files += 1;
        for c in chunks {
            index
                .index(&c.into())
                .map_err(|e| serr("MIGRATE_SESS_INDEX", format!("{}: {e}", p.display())))?;
            report.chunks += 1;
        }
    }
    Ok(report)
}

/// The quarantine directory for a source.
pub fn quarantine_dir(data_dir: &Path, source: &str) -> PathBuf {
    data_dir.join("imported-sessions").join(source)
}

/// Index the quarantined transcripts for `source` into the live session
/// search store, treating index failure as import failure.
///
/// A bare [`index_quarantine`] returns `Err` on an index write failure, but
/// nothing stops a caller from logging it and reporting success anyway
/// which is what `migrate apply` used to do (stderr line, exit 0). This
/// function's contract is explicit: an `Err` here means the session import
/// did not complete and must be reported as failed, never as success.
/// Callers must propagate the error to their exit status.
///
/// A missing quarantine dir is not a failure: there is simply nothing to
/// index, and an empty report is returned.
pub fn ensure_sessions_indexed(
    targets: &crate::Targets,
    source: &str,
    index: &SessionIndex,
) -> Result<IndexReport, PantheonError> {
    let q = quarantine_dir(&targets.data_dir, source);
    if !q.is_dir() {
        return Ok(IndexReport {
            source: source.to_string(),
            ..Default::default()
        });
    }
    index_quarantine(&q, source, index).map_err(|e| {
        PantheonError::new(
            "MIGRATE_SESS_INDEX_FAILED",
            Layer::Storage,
            false,
            format!("session indexing failed, import not complete: {e}"),
            "the transcripts are quarantined on disk; fix the index and re-run indexing deliberately",
            "see `pantheon migrate apply` output",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-index-test-{}-{}-{tag}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_tmp(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    /// A pretty-printed single-object `.json` transcript indexes its
    /// text: before the fix, line parsing saw only fragments of the
    /// object and yielded nothing.
    #[test]
    fn json_single_object_transcript_indexes() {
        let dir = scratch("single");
        let path = write_tmp(
            &dir,
            "chat.json",
            "{\n  \"role\": \"user\",\n  \"content\": \"hello from json\"\n}\n",
        );
        let chunks = parse_transcript(&path, "test", "chat");
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].text.contains("hello from json"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `.json` array of records indexes every record.
    #[test]
    fn json_array_transcript_indexes() {
        let dir = scratch("array");
        let path = write_tmp(
            &dir,
            "chat.json",
            "[{\"role\": \"user\", \"content\": \"first\"}, {\"role\": \"assistant\", \"content\": \"second\"}]",
        );
        let chunks = parse_transcript(&path, "test", "chat");
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].text.contains("first"));
        assert!(chunks[1].text.contains("second"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// JSONL still parses line by line, unchanged.
    #[test]
    fn jsonl_transcript_unchanged() {
        let dir = scratch("jsonl");
        let path = write_tmp(
            &dir,
            "chat.jsonl",
            "{\"role\": \"user\", \"content\": \"one\"}\n{\"role\": \"user\", \"content\": \"two\"}\n",
        );
        let chunks = parse_transcript(&path, "test", "chat");
        assert_eq!(chunks.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `index_quarantine` picks up `.json` files (not just `.jsonl`)
    /// and does not count a real transcript as empty.
    #[test]
    fn index_quarantine_includes_json_files() {
        let dir = scratch("quarantine");
        std::fs::write(
            dir.join("a.json"),
            "{\"role\": \"user\", \"content\": \"json transcript\"}",
        )
        .unwrap();
        std::fs::write(
            dir.join("b.jsonl"),
            "{\"role\": \"user\", \"content\": \"jsonl transcript\"}",
        )
        .unwrap();
        let index = SessionIndex::open_in_memory().unwrap();
        let report = index_quarantine(&dir, "test", &index).unwrap();
        assert_eq!(report.files, 2);
        assert_eq!(report.chunks, 2);
        assert!(report.empty_files.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
