//! Event-sourced execution ledger. Every run persists its events to SQLite;
//! `/explain run_X` replays them. History is append-only.
use pantheon_core::error::{Layer, PantheonError};
use pantheon_core::events::Event;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

/// One persisted row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub id: i64,
    pub run_id: String,
    pub seq: i64,
    pub ts_ms: i64,
    pub event: Event,
}

/// An artifact stored in the ledger database and served through a signed
/// generative-UI URL. Bytes never need to be reconstructed from event JSON.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub task_id: String,
    pub mime: String,
    pub bytes: Vec<u8>,
    pub created_ms: i64,
}

pub struct Ledger {
    conn: Mutex<Connection>,
}

/// Artifact size cap: generative-UI blobs are small by design; the ledger
/// is not a blob store. 8 MiB covers SVG/PNG/JSON artifacts comfortably.
const MAX_ARTIFACT_BYTES: usize = 8 * 1024 * 1024;

fn valid_artifact_id(task_id: &str) -> bool {
    !task_id.is_empty()
        && task_id.len() <= 128
        && task_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

fn valid_mime(mime: &str) -> bool {
    !mime.is_empty() && mime.len() <= 256 && mime.bytes().all(|b| b >= 0x20 && b != 0x7f)
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn err(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Storage,
        false,
        cause,
        "check ledger path permissions and disk space",
        "",
    )
}

/// Extract the run id from any event.
pub fn run_id_of(event: &Event) -> &str {
    match event {
        Event::RunStarted { run_id }
        | Event::RunProgress { run_id, .. }
        | Event::RunCompleted { run_id }
        | Event::RunFailed { run_id, .. }
        | Event::RunCanceled { run_id, .. }
        | Event::RunRecovered { run_id }
        | Event::ModelRequested { run_id, .. }
        | Event::ModelDelta { run_id, .. }
        | Event::ModelCompleted { run_id }
        | Event::ToolRequested { run_id, .. }
        | Event::ToolStarted { run_id, .. }
        | Event::ToolOutput { run_id, .. }
        | Event::ToolCompleted { run_id, .. }
        | Event::AgentSpawned { run_id, .. }
        | Event::AgentMessage { run_id, .. }
        | Event::AgentCompleted { run_id, .. }
        | Event::MemoryProposed { run_id }
        | Event::ApprovalRequested { run_id, .. }
        | Event::ApprovalGranted { run_id, .. }
        | Event::ApprovalDenied { run_id, .. }
        | Event::AssistantMessage { run_id, .. }
        | Event::ToolMessage { run_id, .. } => run_id,
    }
}

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS runs (
  run_id TEXT PRIMARY KEY,
  created_ms INTEGER NOT NULL,
  status TEXT NOT NULL DEFAULT 'running'
);
CREATE TABLE IF NOT EXISTS events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  run_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  ts_ms INTEGER NOT NULL,
  event_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_events_run ON events(run_id, seq);
CREATE TABLE IF NOT EXISTS claims (
  key TEXT PRIMARY KEY,
  ts_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS artifacts (
  task_id TEXT PRIMARY KEY,
  mime TEXT NOT NULL,
  bytes BLOB NOT NULL,
  created_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS run_process_groups (
  run_id TEXT NOT NULL,
  pgid INTEGER NOT NULL,
  lease_id TEXT NOT NULL,
  created_ms INTEGER NOT NULL,
  PRIMARY KEY (run_id, pgid)
);";

fn has_pending_approval(
    conn: &Connection,
    run_id: &str,
    resolved_scope: Option<&str>,
) -> Result<bool, PantheonError> {
    let mut stmt = conn
        .prepare("SELECT event_json FROM events WHERE run_id=?1 ORDER BY id")
        .map_err(|e| err("LEDGER_APPROVAL", e.to_string()))?;
    let rows = stmt
        .query_map(params![run_id], |r| r.get::<_, String>(0))
        .map_err(|e| err("LEDGER_APPROVAL", e.to_string()))?;
    let mut requested = std::collections::HashSet::new();
    let mut resolved = std::collections::HashSet::new();
    for row in rows {
        let raw = row.map_err(|e| err("LEDGER_APPROVAL", e.to_string()))?;
        let event: Event =
            serde_json::from_str(&raw).map_err(|e| err("LEDGER_APPROVAL", e.to_string()))?;
        match event {
            Event::ApprovalRequested { scope, .. } => {
                requested.insert(scope);
            }
            Event::ApprovalGranted { scope, .. } | Event::ApprovalDenied { scope, .. } => {
                resolved.insert(scope);
            }
            _ => {}
        }
    }
    Ok(requested
        .iter()
        .any(|scope| Some(scope.as_str()) != resolved_scope && !resolved.contains(scope)))
}

impl Ledger {
    pub fn open(path: &Path) -> Result<Self, PantheonError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| err("LEDGER_MKDIR", e.to_string()))?;
            }
        }
        let conn = Connection::open(path).map_err(|e| err("LEDGER_OPEN", e.to_string()))?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(|e| err("LEDGER_BUSY_TIMEOUT", e.to_string()))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| err("LEDGER_SCHEMA", e.to_string()))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
    pub fn open_in_memory() -> Result<Self, PantheonError> {
        let conn = Connection::open_in_memory().map_err(|e| err("LEDGER_OPEN", e.to_string()))?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(|e| err("LEDGER_BUSY_TIMEOUT", e.to_string()))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| err("LEDGER_SCHEMA", e.to_string()))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
    pub fn append(&self, event: &Event) -> Result<LedgerEntry, PantheonError> {
        let run_id = run_id_of(event).to_string();
        let json = serde_json::to_string(event).map_err(|e| err("LEDGER_SER", e.to_string()))?;
        let ts = now_ms();
        let mut raw_conn = self
            .conn
            .lock()
            .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        let conn = raw_conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| err("LEDGER_APPEND", e.to_string()))?;
        if matches!(event, Event::RunStarted { .. }) {
            conn.execute(
                "INSERT OR IGNORE INTO runs (run_id, created_ms, status) VALUES (?1, ?2, 'running')",
                params![run_id, ts],
            ).map_err(|e| err("LEDGER_RUN", e.to_string()))?;
        }
        if matches!(event, Event::RunFailed { .. }) {
            conn.execute(
                "UPDATE runs SET status = 'failed' WHERE run_id = ?1 AND status NOT IN ('completed','failed','canceled')",
                params![run_id],
            )
            .map_err(|e| err("LEDGER_UPDATE", e.to_string()))?;
        }
        if matches!(event, Event::RunCanceled { .. }) {
            conn.execute(
                "UPDATE runs SET status = 'canceled' WHERE run_id = ?1 AND status NOT IN ('completed','failed','canceled')",
                params![run_id],
            )
            .map_err(|e| err("LEDGER_UPDATE", e.to_string()))?;
        }
        if matches!(event, Event::ApprovalRequested { .. }) {
            conn.execute(
                "UPDATE runs SET status = 'awaiting_approval' WHERE run_id = ?1 AND status NOT IN ('completed','failed','canceled')",
                params![run_id],
            )
            .map_err(|e| err("LEDGER_UPDATE", e.to_string()))?;
        }
        if matches!(
            event,
            Event::ApprovalGranted { .. } | Event::ApprovalDenied { .. }
        ) {
            let resolved_scope = match event {
                Event::ApprovalGranted { scope, .. } | Event::ApprovalDenied { scope, .. } => {
                    Some(scope.as_str())
                }
                _ => None,
            };
            let status = if has_pending_approval(&conn, &run_id, resolved_scope)? {
                "awaiting_approval"
            } else {
                "running"
            };
            conn.execute(
                "UPDATE runs SET status = ?2 WHERE run_id = ?1 AND status NOT IN ('completed','failed','canceled')",
                params![run_id, status],
            )
            .map_err(|e| err("LEDGER_UPDATE", e.to_string()))?;
        }
        if matches!(event, Event::RunCompleted { .. }) {
            conn.execute(
                "UPDATE runs SET status = 'completed' WHERE run_id = ?1 AND status NOT IN ('completed','failed','canceled')",
                params![run_id],
            )
            .map_err(|e| err("LEDGER_UPDATE", e.to_string()))?;
        }
        conn.execute(
            "INSERT INTO events (run_id, seq, ts_ms, event_json) VALUES (?1, (SELECT COALESCE(MAX(seq),0)+1 FROM events), ?2, ?3)",
            params![run_id, ts, json],
        )
        .map_err(|e| err("LEDGER_APPEND", e.to_string()))?;
        let id: i64 = conn.last_insert_rowid();
        conn.commit()
            .map_err(|e| err("LEDGER_APPEND", e.to_string()))?;
        Ok(LedgerEntry {
            id,
            run_id,
            seq: id,
            ts_ms: ts,
            event: event.clone(),
        })
    }

    pub fn replay(&self, run_id: &str) -> Result<Vec<LedgerEntry>, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, run_id, seq, ts_ms, event_json FROM events WHERE run_id = ?1 ORDER BY id",
            )
            .map_err(|e| err("LEDGER_REPLAY", e.to_string()))?;
        let rows = stmt
            .query_map(params![run_id], |row| {
                let json: String = row.get(4)?;
                let event: Event = serde_json::from_str(&json).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        4,
                        rusqlite::types::Type::Text,
                        e.into(),
                    )
                })?;
                Ok(LedgerEntry {
                    id: row.get(0)?,
                    run_id: row.get(1)?,
                    seq: row.get(2)?,
                    ts_ms: row.get(3)?,
                    event,
                })
            })
            .map_err(|e| err("LEDGER_RETRY", e.to_string()))?;
        let mut out = vec![];
        for r in rows {
            out.push(r.map_err(|e| err("LEDGER_RECON", e.to_string()))?);
        }
        Ok(out)
    }

    /// Idempotency claim for the scheduler (spec section 21): occurrence key,
    /// replay-safe. Returns true if this claimer was the first.
    pub fn claim(&self, key: &str) -> Result<bool, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        let n = conn
            .execute(
                "INSERT OR IGNORE INTO claims (key, ts_ms) VALUES (?1, ?2)",
                params![key, now_ms()],
            )
            .map_err(|e| err("LEDGER_CLAIM", e.to_string()))? as usize;
        Ok(n == 1)
    }

    /// Store generative-UI bytes by task id.  `INSERT OR REPLACE` makes
    /// retries of the same task deterministic while the signed URL remains
    /// the only client-visible locator.
    pub fn put_artifact(
        &self,
        task_id: &str,
        mime: &str,
        bytes: &[u8],
    ) -> Result<Artifact, PantheonError> {
        if !valid_artifact_id(task_id) {
            return Err(err(
                "ARTIFACT_ID",
                "artifact task id is not a safe path component".into(),
            ));
        }
        if !valid_mime(mime) {
            return Err(err(
                "ARTIFACT_MIME",
                "artifact mime type contains invalid header characters".into(),
            ));
        }
        if bytes.len() > MAX_ARTIFACT_BYTES {
            return Err(err(
                "ARTIFACT_TOO_LARGE",
                format!(
                    "artifact is {} bytes; the cap is {MAX_ARTIFACT_BYTES}",
                    bytes.len()
                ),
            ));
        }
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        let ts = now_ms();
        conn.execute(
            "INSERT INTO artifacts (task_id, mime, bytes, created_ms) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(task_id) DO UPDATE SET mime=excluded.mime, bytes=excluded.bytes, created_ms=excluded.created_ms",
            params![task_id, mime, bytes, ts],
        )
        .map_err(|e| err("LEDGER_ARTIFACT", e.to_string()))?;
        Ok(Artifact {
            task_id: task_id.to_string(),
            mime: mime.to_string(),
            bytes: bytes.to_vec(),
            created_ms: ts,
        })
    }

    pub fn artifact(&self, task_id: &str) -> Result<Option<Artifact>, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        conn.query_row(
            "SELECT task_id, mime, bytes, created_ms FROM artifacts WHERE task_id=?1",
            params![task_id],
            |r| {
                Ok(Artifact {
                    task_id: r.get(0)?,
                    mime: r.get(1)?,
                    bytes: r.get(2)?,
                    created_ms: r.get(3)?,
                })
            },
        )
        .optional()
        .map_err(|e| err("LEDGER_ARTIFACT", e.to_string()))
    }

    /// Associate a process group with a run and the lease that owns it.
    pub fn register_process_group(
        &self,
        run_id: &str,
        pgid: i32,
        lease_id: &str,
    ) -> Result<(), PantheonError> {
        if pgid <= 1 {
            return Err(err("LEDGER_PGID", "invalid process group id".into()));
        }
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        conn.execute(
            "INSERT OR REPLACE INTO run_process_groups (run_id, pgid, lease_id, created_ms) VALUES (?1, ?2, ?3, ?4)",
            params![run_id, pgid, lease_id, now_ms()],
        )
        .map_err(|e| err("LEDGER_PGID", e.to_string()))?;
        Ok(())
    }

    pub fn process_groups(&self, run_id: &str, lease_id: &str) -> Result<Vec<i32>, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT pgid FROM run_process_groups WHERE run_id=?1 AND lease_id=?2 ORDER BY pgid",
            )
            .map_err(|e| err("LEDGER_PGID", e.to_string()))?;
        let rows = stmt
            .query_map(params![run_id, lease_id], |r| r.get(0))
            .map_err(|e| err("LEDGER_PGID", e.to_string()))?;
        let mut out = vec![];
        for row in rows {
            out.push(row.map_err(|e| err("LEDGER_PGID", e.to_string()))?);
        }
        Ok(out)
    }

    pub fn unregister_process_group(&self, run_id: &str, pgid: i32) -> Result<(), PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        conn.execute(
            "DELETE FROM run_process_groups WHERE run_id=?1 AND pgid=?2",
            params![run_id, pgid],
        )
        .map_err(|e| err("LEDGER_PGID", e.to_string()))?;
        Ok(())
    }

    pub fn unregister_process_group_owned(
        &self,
        run_id: &str,
        pgid: i32,
        lease_id: &str,
    ) -> Result<(), PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        conn.execute(
            "DELETE FROM run_process_groups WHERE run_id=?1 AND pgid=?2 AND lease_id=?3",
            params![run_id, pgid, lease_id],
        )
        .map_err(|e| err("LEDGER_PGID", e.to_string()))?;
        Ok(())
    }

    /// Does a durable claim already exist for this key?
    pub fn is_claimed(&self, key: &str) -> Result<bool, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        Ok(conn
            .query_row(
                "SELECT COUNT(*) FROM claims WHERE key = ?1",
                params![key],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .optional()
            .map_err(|e| err("LEDGER_CLAIM", e.to_string()))?
            .unwrap_or(false))
    }

    pub fn status(&self, run_id: &str) -> Result<Option<String>, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        conn.query_row(
            "SELECT status FROM runs WHERE run_id=?1",
            params![run_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| err("LEDGER_STATUS", e.to_string()))
    }

    /// List recent runs, newest first. Powers the REPL `/runs` picker and
    /// auto-resume. `limit` bounds the row count.
    pub fn list_runs(&self, limit: usize) -> Result<Vec<(String, String, i64)>, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT run_id, status, created_ms FROM runs ORDER BY created_ms DESC, rowid DESC LIMIT ?1",
            )
            .map_err(|e| err("LEDGER_QUERY", e.to_string()))?;
        let rows = stmt
            .query_map(params![limit as i64], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map_err(|e| err("LEDGER_QUERY", e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| err("LEDGER_QUERY", e.to_string()))?);
        }
        Ok(out)
    }

    /// Reopen a terminal run for continued conversation. Only terminal
    /// statuses flip back to running; a running/awaiting run is untouched
    /// (the caller then follows the normal path). Returns whether a
    /// reopen happened. The event trail keeps its original shape; the
    /// status flip is the continuation marker.
    pub fn reopen_run(&self, run_id: &str) -> Result<bool, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        let n = conn
            .execute(
                "UPDATE runs SET status='running' WHERE run_id=?1
                 AND status IN ('completed','failed','canceled')",
                params![run_id],
            )
            .map_err(|e| err("LEDGER_UPDATE", e.to_string()))?;
        Ok(n > 0)
    }
    /// Highest global ledger sequence (== highest event id). Checkpoints
    /// anchor to this, so `rollback --seq N` maps to a real position.
    pub fn max_seq(&self) -> Result<i64, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        conn.query_row("SELECT COALESCE(MAX(seq),0) FROM events", [], |r| r.get(0))
            .map_err(|e| err("LEDGER_STATUS", e.to_string()))
    }
    pub fn explain(&self, run_id: &str) -> Result<String, PantheonError> {
        let entries = self.replay(run_id)?;
        if entries.is_empty() {
            return Ok(format!("run {run_id}: no events recorded"));
        }
        let mut lines = vec![format!("run {run_id}: {} events", entries.len())];
        for e in &entries {
            lines.push(format!("  #{} {}", e.seq, describe(&e.event)));
        }
        Ok(lines.join("\n"))
    }
}

fn describe(ev: &Event) -> String {
    match ev {
        Event::RunStarted { .. } => String::from("started"),
        Event::RunProgress { detail, .. } => format!("progress: {detail}"),
        Event::RunCompleted { .. } => String::from("completed"),
        Event::RunFailed { code, .. } => format!("FAILED ({code})"),
        Event::RunCanceled { reason, .. } => format!("canceled ({reason})"),
        Event::RunRecovered { .. } => String::from("recovered after restart"),
        Event::ModelRequested { model, .. } => format!("model requested: {model}"),
        Event::ModelDelta { .. } => String::from("model streamed output"),
        Event::ModelCompleted { .. } => String::from("model turn done"),
        Event::ToolRequested { tool, .. } => format!("tool requested: {tool}"),
        Event::ToolStarted { tool, .. } => format!("tool started: {tool}"),
        Event::ToolOutput {
            tool, truncated, ..
        } => format!(
            "tool output: {tool}{}",
            if *truncated { " (compacted)" } else { "" }
        ),
        Event::ToolCompleted { tool, .. } => format!("tool done: {tool}"),
        Event::AgentSpawned { agent, .. } => format!("spawned sub-agent: {agent}"),
        Event::AgentMessage { agent, .. } => format!("sub-agent message: {agent}"),
        Event::AgentCompleted { agent, .. } => format!("sub-agent done: {agent}"),
        Event::MemoryProposed { .. } => String::from("memory write proposed"),
        Event::ApprovalRequested { scope, .. } => format!("approval requested: {scope}"),
        Event::ApprovalGranted { scope, .. } => format!("approval granted: {scope}"),
        Event::ApprovalDenied { scope, .. } => format!("approval denied: {scope}"),
        Event::AssistantMessage { message, .. } => {
            format!(
                "assistant: {}",
                message.content.chars().take(120).collect::<String>()
            )
        }
        Event::ToolMessage { message, .. } => {
            format!(
                "tool result: {}",
                message.content.chars().take(120).collect::<String>()
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn artifacts_are_stored_in_the_ledger_database() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger
            .put_artifact("task-1", "image/png", &[1, 2, 3])
            .unwrap();
        let a = ledger.artifact("task-1").unwrap().unwrap();
        assert_eq!(a.mime, "image/png");
        assert_eq!(a.bytes, vec![1, 2, 3]);
        assert!(ledger.artifact("task-2").unwrap().is_none());
        assert_eq!(
            ledger
                .put_artifact("bad", "text/plain\r\nX-Test: yes", b"x")
                .unwrap_err()
                .code,
            "ARTIFACT_MIME"
        );
        assert_eq!(
            ledger
                .put_artifact("bad/id", "text/plain", b"x")
                .unwrap_err()
                .code,
            "ARTIFACT_ID"
        );
    }

    #[test]
    fn terminal_run_status_cannot_be_overwritten() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger
            .append(&Event::RunStarted { run_id: "r".into() })
            .unwrap();
        ledger
            .append(&Event::RunCanceled {
                run_id: "r".into(),
                reason: "stop".into(),
            })
            .unwrap();
        ledger
            .append(&Event::RunFailed {
                run_id: "r".into(),
                code: "late".into(),
            })
            .unwrap();
        assert_eq!(ledger.status("r").unwrap().as_deref(), Some("canceled"));
    }

    #[test]
    fn round_trip_and_explain() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger
            .append(&Event::RunStarted {
                run_id: "r1".into(),
            })
            .unwrap();
        ledger
            .append(&Event::ToolStarted {
                run_id: "r1".into(),
                call_id: "call_0_0".into(),
                tool: "shell".into(),
                args: String::new(),
            })
            .unwrap();
        ledger
            .append(&Event::RunCompleted {
                run_id: "r1".into(),
            })
            .unwrap();
        assert_eq!(ledger.replay("r1").unwrap().len(), 3);
        assert!(ledger.explain("r1").unwrap().contains("completed"));
        assert_eq!(ledger.status("r1").unwrap().as_deref(), Some("completed"));
    }
}
