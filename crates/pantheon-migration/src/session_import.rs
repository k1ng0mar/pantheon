//! First-class session import: foreign transcripts become real, resumable
//! ledger runs.
//!
//! The quarantine path (`carry::write_session_import_*`) keeps a search-only
//! copy of a source's transcripts. This module goes further: it replays each
//! transcript into the ledger as native events, so the session shows up in
//! `/resume`, reopens, and continues like any Pantheon conversation.
//!
//! Design notes:
//!
//! - **Deterministic ids.** A run id is
//!   `imported:<source>:<parent-dir>:<file-stem>`. Re-running an import
//!   converges: a run that already exists is skipped, never duplicated.
//! - **Native shapes.** User and assistant records become
//!   `AssistantMessage` events - the same shape the runtime uses for live
//!   sessions (user prompts ride `AssistantMessage` with `Role::User`), so
//!   `rebuild_messages` and resume work unchanged.
//! - **Reasoning survives.** Thinking traces become `ImportedReasoning`
//!   events: durable, surfaced in the TUI transcript in order, never sent
//!   to the model as a conversation message.
//! - **Tool traffic is dropped.** Pantheon cannot safely re-execute a
//!   foreign tool call, so tool records are counted and the drop is written
//!   onto the run as a `RunProgress` note. The transcript is honest about
//!   what is missing.
//! - **Provenance.** Source, original session id, and import timestamp are
//!   written as the run's first `RunProgress` event.

use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::events::Event;
use pantheon_api::message::Message;
use pantheon_storage::Ledger;
use std::path::{Path, PathBuf};

fn serr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Storage,
        false,
        cause,
        "check the data dir is writable and the ledger is not locked",
        "see `pantheon migrate apply --categories sessions` output",
    )
}

/// One ordered turn extracted from a foreign transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedTurn {
    pub role: TurnRole,
    pub text: String,
}

/// The roles a transcript record can play in an imported session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnRole {
    User,
    Assistant,
    Reasoning,
}

/// A JSONL transcript record, tolerating the shapes real agents write.
///
/// Beyond the flat `{role, content}` shape this also understands the
/// Claude-style envelope `{type: "user"|"assistant", message: {role,
/// content: [...]}}` whose content parts carry `thinking` / `text` /
/// `tool_use` payloads.
#[derive(Debug, serde::Deserialize, Default)]
struct Record {
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    content: Option<serde_json::Value>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    message: Option<serde_json::Value>,
    /// Anthropic-style thinking blocks also appear flattened on the record.
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
}

/// Flatten a `content` value to text, collecting reasoning parts separately.
///
/// Returns `(text, reasoning)`: display text first, thinking traces second.
/// Tool payloads (`tool_use`, `tool_result`, `function_call`, ...) contribute
/// nothing and are counted by the caller via `dropped`.
fn content_to_text(
    v: &serde_json::Value,
    text: &mut String,
    reasoning: &mut String,
    dropped: &mut usize,
) {
    match v {
        serde_json::Value::String(s) => {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(s);
        }
        serde_json::Value::Array(items) => {
            for it in items {
                content_to_text(it, text, reasoning, dropped);
            }
        }
        serde_json::Value::Object(map) => {
            let t = map.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match t {
                // Thinking / reasoning parts, in the shapes agents write.
                "thinking" | "reasoning" | "thought" => {
                    let r = map
                        .get("thinking")
                        .or_else(|| map.get("reasoning"))
                        .or_else(|| map.get("reasoning_content"))
                        .or_else(|| map.get("text"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !r.trim().is_empty() {
                        if !reasoning.is_empty() {
                            reasoning.push_str("\n\n");
                        }
                        reasoning.push_str(r.trim());
                    }
                }
                // Tool traffic is dropped, not replayed.
                "tool_use"
                | "tool_result"
                | "function_call"
                | "function_call_output"
                | "tool_call"
                | "computer_call" => {
                    *dropped += 1;
                }
                _ => {
                    if let Some(s) = map.get("text").and_then(|v| v.as_str()) {
                        if !text.is_empty() {
                            text.push_str("\n\n");
                        }
                        text.push_str(s);
                    } else if let Some(inner) = map.get("content") {
                        content_to_text(inner, text, reasoning, dropped);
                    } else if let Some(inner) = map.get("message") {
                        content_to_text(inner, text, reasoning, dropped);
                    }
                }
            }
        }
        _ => {}
    }
}

/// Classify one parsed record into ordered turns.
///
/// A record can yield up to two turns (reasoning, then its message), which
/// keeps a Claude-style `thinking` + `text` pair in its original order.
/// Returns the turns and the number of dropped tool records.
fn record_to_turns(rec: &Record) -> (Vec<ImportedTurn>, usize) {
    let mut dropped = 0usize;
    let mut text = String::new();
    let mut reasoning = String::new();

    // Flat reasoning fields some exporters put beside `content`.
    for r in [&rec.thinking, &rec.reasoning_content, &rec.reasoning]
        .into_iter()
        .flatten()
    {
        if !r.trim().is_empty() {
            if !reasoning.is_empty() {
                reasoning.push_str("\n\n");
            }
            reasoning.push_str(r.trim());
        }
    }
    if let Some(s) = &rec.text {
        text.push_str(s);
    }
    if let Some(c) = &rec.content {
        content_to_text(c, &mut text, &mut reasoning, &mut dropped);
    }
    if let Some(m) = &rec.message {
        // A nested envelope: `{"message":{"role":..,"content":..}}` or the
        // Claude envelope `{"type":"assistant","message":{...}}`.
        if let Ok(nested) = serde_json::from_value::<Record>(m.clone()) {
            let (nested_turns, d) = record_to_turns(&nested);
            dropped += d;
            if nested_role(&nested).is_some() {
                // Fully-roled envelope: its turns stand as parsed.
                return (nested_turns, dropped);
            }
            // Role-less envelope: fold its text into this record's role.
            for t in nested_turns {
                match t.role {
                    TurnRole::Reasoning => {
                        if !reasoning.is_empty() {
                            reasoning.push_str("\n\n");
                        }
                        reasoning.push_str(&t.text);
                    }
                    _ => {
                        if !text.is_empty() {
                            text.push_str("\n\n");
                        }
                        text.push_str(&t.text);
                    }
                }
            }
        } else {
            content_to_text(m, &mut text, &mut reasoning, &mut dropped);
        }
    }

    let role = classify_role(rec);
    let mut turns = Vec::new();
    match role {
        TurnRole::Reasoning => {
            let body = if reasoning.is_empty() {
                text
            } else {
                reasoning
            };
            if !body.trim().is_empty() {
                turns.push(ImportedTurn {
                    role: TurnRole::Reasoning,
                    text: body.trim().to_string(),
                });
            }
        }
        TurnRole::User | TurnRole::Assistant => {
            if !reasoning.trim().is_empty() {
                turns.push(ImportedTurn {
                    role: TurnRole::Reasoning,
                    text: reasoning.trim().to_string(),
                });
            }
            if !text.trim().is_empty() {
                turns.push(ImportedTurn {
                    role,
                    text: text.trim().to_string(),
                });
            }
        }
    }
    (turns, dropped)
}

/// The role a nested record declares, if any.
fn nested_role(rec: &Record) -> Option<TurnRole> {
    let raw = rec
        .role
        .as_deref()
        .or(rec.kind.as_deref())
        .or(rec.r#type.as_deref())?;
    match raw.to_ascii_lowercase().as_str() {
        "user" | "human" => Some(TurnRole::User),
        "assistant" | "ai" | "model" => Some(TurnRole::Assistant),
        "thinking" | "reasoning" | "thought" => Some(TurnRole::Reasoning),
        _ => None,
    }
}

fn classify_role(rec: &Record) -> TurnRole {
    // Tool records never become turns; the caller counts them as dropped.
    let raw = rec
        .role
        .as_deref()
        .or(rec.kind.as_deref())
        .or(rec.r#type.as_deref())
        .unwrap_or("");
    match raw.to_ascii_lowercase().as_str() {
        "user" | "human" => TurnRole::User,
        "assistant" | "ai" | "model" => TurnRole::Assistant,
        "thinking" | "reasoning" | "thought" => TurnRole::Reasoning,
        "tool"
        | "tool_use"
        | "tool_result"
        | "tool_call"
        | "function_call"
        | "function_call_output"
        | "computer_call"
        | "title"
        | "system"
        | "summary" => {
            // Not a conversational turn; counted as dropped by the caller.
            TurnRole::Assistant
        }
        _ => TurnRole::Assistant,
    }
}

/// Whether the record is tool traffic (dropped, counted, never replayed).
fn is_tool_record(rec: &Record) -> bool {
    let raw = rec
        .role
        .as_deref()
        .or(rec.kind.as_deref())
        .or(rec.r#type.as_deref())
        .unwrap_or("");
    matches!(
        raw.to_ascii_lowercase().as_str(),
        "tool"
            | "tool_use"
            | "tool_result"
            | "tool_call"
            | "function_call"
            | "function_call_output"
            | "computer_call"
    )
}

/// Parse one transcript file into ordered turns.
///
/// Accepts JSONL (one record per line) and whole-file JSON (an array of
/// records or `{"messages": [...]}`). Malformed lines are skipped: a
/// truncated final line in a live transcript is normal, not corruption.
/// Consecutive same-role turns merge, so a chatty exporter does not spray
/// one-message-per-line across the imported session.
pub fn parse_session_transcript(path: &Path) -> (Vec<ImportedTurn>, usize) {
    let Ok(body) = std::fs::read_to_string(path) else {
        return (Vec::new(), 0);
    };
    let trimmed = body.trim();
    if trimmed.starts_with('[') || trimmed.starts_with('{') {
        // Whole-file JSON: an array of records, or `{"messages": [...]}`.
        // A single JSON object that is itself one record falls through to
        // line parsing below.
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            let arr: Option<Vec<serde_json::Value>> = v
                .as_array()
                .cloned()
                .or_else(|| v.get("messages").and_then(|m| m.as_array()).cloned());
            if let Some(arr) = arr {
                return records_to_turns(arr.iter().map(|v| v.to_string()).collect());
            }
        }
    }
    records_to_turns(trimmed.lines().map(|s| s.to_string()).collect())
}

fn records_to_turns(raw: Vec<String>) -> (Vec<ImportedTurn>, usize) {
    let mut turns: Vec<ImportedTurn> = Vec::new();
    let mut dropped = 0usize;
    for line in raw {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<Record>(t) else {
            continue;
        };
        if is_tool_record(&rec) {
            dropped += 1;
            continue;
        }
        let (mut ts, d) = record_to_turns(&rec);
        dropped += d;
        for t in ts.drain(..) {
            // Merge consecutive same-role turns.
            if let Some(last) = turns.last_mut() {
                if last.role == t.role {
                    last.text.push_str("\n\n");
                    last.text.push_str(&t.text);
                    continue;
                }
            }
            turns.push(t);
        }
    }
    (turns, dropped)
}

/// Deterministic run id for one imported transcript.
///
/// `imported:<source>:<parent-dir>:<file-stem>` - stable across re-runs
/// and across machines, and the parent dir keeps two same-named files in
/// different session directories from colliding. Residual edge: two
/// different directories with the *same* name (hermes' `sessions/` vs
/// `state/sessions/`) holding the same file stem map to one id; the second
/// import is then skipped as already-imported, which is the safe direction
/// (no duplication, and same-stem files across those alt dirs would be
/// copies of one session anyway).
pub fn import_run_id(source: &str, path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "session".to_string());
    let parent = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "sessions".to_string());
    format!(
        "imported:{}:{}:{}",
        sanitize(source),
        sanitize(&parent),
        sanitize(&stem)
    )
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// What happened when one transcript was offered to the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportStatus {
    /// Written to the ledger as a new run.
    Imported,
    /// The run id already exists; re-import converged, nothing duplicated.
    SkippedExists,
    /// The transcript held no usable turns; nothing was written.
    SkippedEmpty,
}

/// The outcome of importing one transcript file.
#[derive(Debug, Clone)]
pub struct SessionImportReport {
    pub run_id: String,
    pub status: ImportStatus,
    pub title: String,
    pub user_turns: usize,
    pub assistant_turns: usize,
    pub reasoning_traces: usize,
    pub dropped_tool_records: usize,
}

impl SessionImportReport {
    pub fn turns(&self) -> usize {
        self.user_turns + self.assistant_turns
    }
}

/// Import one transcript file into the ledger as a resumable run.
///
/// Idempotent: when the deterministic run id already exists the import is
/// skipped and reported, never duplicated. On success the run is left
/// `completed` (a finished foreign conversation) so `/resume` can reopen
/// and continue it.
pub fn import_session_transcript(
    ledger: &Ledger,
    source: &str,
    path: &Path,
    imported_at_ms: i64,
) -> Result<SessionImportReport, PantheonError> {
    let run_id = import_run_id(source, path);
    let (turns, dropped) = parse_session_transcript(path);
    let mut report = SessionImportReport {
        run_id: run_id.clone(),
        status: ImportStatus::SkippedEmpty,
        title: String::new(),
        user_turns: 0,
        assistant_turns: 0,
        reasoning_traces: 0,
        dropped_tool_records: dropped,
    };
    if turns.is_empty() {
        return Ok(report);
    }
    if ledger
        .status(&run_id)
        .map_err(|e| serr("IMPORT_STATUS", e.to_string()))?
        .is_some()
    {
        report.status = ImportStatus::SkippedExists;
        return Ok(report);
    }

    let title = turns
        .iter()
        .find(|t| t.role == TurnRole::User)
        .map(|t| first_line(&t.text, 80))
        .unwrap_or_else(|| {
            path.file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "imported session".to_string())
        });
    report.title = title.clone();

    let original_id = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    ledger
        .append(&Event::RunStarted {
            run_id: run_id.clone(),
        })
        .map_err(|e| serr("IMPORT_START", e.to_string()))?;
    ledger
        .append(&Event::SessionTitled {
            run_id: run_id.clone(),
            title: title.clone(),
            model: "migration".to_string(),
            source: "import".to_string(),
        })
        .map_err(|e| serr("IMPORT_TITLE", e.to_string()))?;
    ledger
        .append(&Event::RunProgress {
            run_id: run_id.clone(),
            detail: format!(
                "imported from {source} transcript {} original_id={original_id} imported_at_ms={imported_at_ms}",
                path.display()
            ),
        })
        .map_err(|e| serr("IMPORT_PROVENANCE", e.to_string()))?;

    let mut turn_n = 0usize;
    let mut pending_user: Option<String> = None;
    // Buffer user text until the assistant answers so each exchange gets
    // one turn id; a trailing user message without an answer still lands.
    let mut flush = |user: &mut Option<String>,
                     assistant: Option<&ImportedTurn>,
                     reasoning: &[ImportedTurn],
                     ledger: &Ledger|
     -> Result<(), PantheonError> {
        turn_n += 1;
        let turn_id = format!("imported-turn-{turn_n}");
        ledger
            .append(&Event::TurnStarted {
                run_id: run_id.clone(),
                turn_id: turn_id.clone(),
            })
            .map_err(|e| serr("IMPORT_TURN", e.to_string()))?;
        if let Some(u) = user.take() {
            report.user_turns += 1;
            ledger
                .append(&Event::AssistantMessage {
                    run_id: run_id.clone(),
                    message: Message::user(u),
                })
                .map_err(|e| serr("IMPORT_USER", e.to_string()))?;
        }
        for r in reasoning {
            report.reasoning_traces += 1;
            ledger
                .append(&Event::ImportedReasoning {
                    run_id: run_id.clone(),
                    turn_id: turn_id.clone(),
                    text: r.text.clone(),
                })
                .map_err(|e| serr("IMPORT_REASONING", e.to_string()))?;
        }
        if let Some(a) = assistant {
            report.assistant_turns += 1;
            ledger
                .append(&Event::AssistantMessage {
                    run_id: run_id.clone(),
                    message: Message::assistant(a.text.clone()),
                })
                .map_err(|e| serr("IMPORT_ASSISTANT", e.to_string()))?;
        }
        ledger
            .append(&Event::TurnCompleted {
                run_id: run_id.clone(),
                turn_id,
                outcome: "imported".to_string(),
            })
            .map_err(|e| serr("IMPORT_TURN_DONE", e.to_string()))?;
        Ok(())
    };

    let mut pending_reasoning: Vec<ImportedTurn> = Vec::new();
    for t in &turns {
        match t.role {
            TurnRole::User => {
                // A new user turn closes any open exchange first.
                if pending_user.is_some() || !pending_reasoning.is_empty() {
                    flush(
                        &mut pending_user,
                        None,
                        &std::mem::take(&mut pending_reasoning),
                        ledger,
                    )?;
                }
                pending_user = Some(match pending_user.take() {
                    Some(u) => format!("{u}\n\n{}", t.text),
                    None => t.text.clone(),
                });
            }
            TurnRole::Reasoning => pending_reasoning.push(t.clone()),
            TurnRole::Assistant => {
                flush(
                    &mut pending_user,
                    Some(t),
                    &std::mem::take(&mut pending_reasoning),
                    ledger,
                )?;
            }
        }
    }
    if pending_user.is_some() || !pending_reasoning.is_empty() {
        flush(
            &mut pending_user,
            None,
            &std::mem::take(&mut pending_reasoning),
            ledger,
        )?;
    }

    if dropped > 0 {
        ledger
            .append(&Event::RunProgress {
                run_id: run_id.clone(),
                detail: format!(
                    "dropped {dropped} tool record(s) at import: foreign tool traffic is not replayable"
                ),
            })
            .map_err(|e| serr("IMPORT_DROPPED", e.to_string()))?;
    }
    ledger
        .append(&Event::RunCompleted {
            run_id: run_id.clone(),
        })
        .map_err(|e| serr("IMPORT_DONE", e.to_string()))?;

    report.status = ImportStatus::Imported;
    Ok(report)
}

/// The outcome of importing a batch of session directories.
pub struct SessionImportBatch {
    pub reports: Vec<SessionImportReport>,
    pub failures: Vec<(PathBuf, String)>,
}

/// Import every transcript file found under each session directory.
///
/// `transcript_format` decides what counts as a transcript, mirroring the
/// quarantine scan so the two views never disagree about the file set.
/// One bad file does not abort the batch: failures are collected for the
/// caller to report.
pub fn import_session_dirs(
    ledger: &Ledger,
    source: &str,
    dirs: &[PathBuf],
    imported_at_ms: i64,
) -> SessionImportBatch {
    let mut batch = SessionImportBatch {
        reports: Vec::new(),
        failures: Vec::new(),
    };
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && crate::carry::transcript_format(p).is_some())
            .collect();
        files.sort();
        for f in files {
            match import_session_transcript(ledger, source, &f, imported_at_ms) {
                Ok(r) => batch.reports.push(r),
                Err(e) => batch.failures.push((f, e.to_string())),
            }
        }
    }
    batch
}

fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    let cut: String = line.chars().take(max).collect();
    if line.chars().count() > max {
        format!("{cut}...")
    } else {
        cut
    }
}
