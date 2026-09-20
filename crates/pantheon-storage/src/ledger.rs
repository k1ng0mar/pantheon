//! Event-sourced execution ledger. Every run persists its events to SQLite;
//! `/explain run_X` replays them. History is append-only.
use pantheon_core::error::{Layer, PantheonError};
use pantheon_core::events::Event;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

/// One persisted row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub id: i64,
    pub run_id: String,
    pub seq: i64,
    pub ts_ms: i64,
    pub event: Event,
}

pub struct Ledger {
    conn: Mutex<Connection>,
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn err(code: &str, cause: String) -> PantheonError {
    PantheonError::new(code, Layer::Storage, false, cause,
        "check ledger path permissions and disk space", "")
}

/// Extract the run id from any event.
pub fn run_id_of(event: &Event) -> &str {
    match event {
        Event::RunStarted { run_id }
        | Event::RunProgress { run_id, .. }
        | Event::RunCompleted { run_id }
        | Event::RunFailed { run_id, .. }
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
        | Event::ApprovalGranted { run_id, .. } => run_id,
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
);";

impl Ledger {
    pub fn open(path: &Path) -> Result<Self, PantheonError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| err("LEDGER_MKDIR", e.to_string()))?;
            }
        }
        let conn = Connection::open(path).map_err(|e| err("LEDGER_OPEN", e.to_string()))?;
        conn.execute_batch(SCHEMA).map_err(|e| err("LEDGER_SCHEMA", e.to_string()))?;
        Ok(Self { conn: Mutex::new(conn) })
    }
    pub fn open_in_memory() -> Result<Self, PantheonError> {
        let conn = Connection::open_in_memory().map_err(|e| err("LEDGER_OPEN", e.to_string()))?;
        conn.execute_batch(SCHEMA).map_err(|e| err("LEDGER_SCHEMA", e.to_string()))?;
        Ok(Self { conn: Mutex::new(conn) })
    }
    pub fn append(&self, event: &Event) -> Result<LedgerEntry, PantheonError> {
        let run_id = run_id_of(event).to_string();
        let json = serde_json::to_string(event)
            .map_err(|e| err("LEDGER_SER", e.to_string()))?;
        let ts = now_ms();
        let conn = self.conn.lock().map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        if matches!(event, Event::RunStarted { .. }) {
            conn.execute(
                "INSERT OR IGNORE INTO runs (run_id, created_ms, status) VALUES (?1, ?2, 'running')",
                params![run_id, ts],
            ).map_err(|e| err("LEDGER_RUN", e.to_string()))?;
        }
        if matches!(event, Event::RunFailed { .. }) {
            conn.execute(
                "UPDATE runs SET status = 'failed' WHERE run_id = ?1",
                params![run_id],
            ).map_err(|e| err("LEDGER_UPDATE", e.to_string()))?;
        }
        if matches!(event, Event::RunCompleted { .. }) {
            conn.execute(
                "UPDATE runs SET status = 'completed' WHERE run_id = ?1",
                params![run_id],
            ).map_err(|e| err("LEDGER_UPDATE", e.to_string()))?;
        }
        conn.execute(
            "INSERT INTO events (run_id, seq, ts_ms, event_json) VALUES (?1, ?2, ?3, ?4)",
            params![run_id, ts, ts, json],
        ).map_err(|e| err("LEDGER_APPEND", e.to_string()))?;
        Ok(LedgerEntry { id: 0, run_id, seq: ts, ts_ms: ts, event: event.clone() })
    }

    pub fn replay(&self, run_id: &str) -> Result<Vec<LedgerEntry>, PantheonError> {
        let conn = self.conn.lock().map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        let mut stmt = conn.prepare(
            "SELECT id, run_id, ts_ms, event_json FROM events WHERE run_id = ?1 ORDER BY id"
        ).map_err(|e| err("LEDGER_REPLAY", e.to_string()))?;
        let rows = stmt.query_map(params![run_id], |row| {
            let json: String = row.get(3)?;
            let event: Event = serde_json::from_str(&json)
                .map_err(|e| rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, e.into()))?;
            Ok(LedgerEntry {
                id: row.get(0)?,
                run_id: row.get(1)?,
                seq: 0,
                ts_ms: row.get(2)?,
                event,
            })
        }).map_err(|e| err("LEDGER_RETRY", e.to_string()))?;
        let mut out = vec![];
        for r in rows {
            out.push(r.map_err(|e| err("LEDGER_RECON", e.to_string()))?);
        }
        Ok(out)
    }

    /// Idempotency claim for the scheduler (spec section 21): occurrence key,
    /// replay-safe. Returns true if this claimer was the first.
    pub fn claim(&self, key: &str) -> Result<bool, PantheonError> {
        let conn = self.conn.lock().map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        let n = conn.execute(
            "INSERT OR IGNORE INTO claims (key, ts_ms) VALUES (?1, ?2)",
            params![key, now_ms()],
        ).map_err(|e| err("LEDGER_CLAIM", e.to_string()))?
            as usize;
        Ok(n == 1)
    }

    /// Does a durable claim already exist for this key?
    pub fn is_claimed(&self, key: &str) -> Result<bool, PantheonError> {
        let conn = self.conn.lock().map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        Ok(conn.query_row(
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
        let conn = self.conn.lock().map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        conn.query_row("SELECT status FROM runs WHERE run_id=?1", params![run_id], |r| r.get(0))
            .optional().map_err(|e| err("LEDGER_STATUS", e.to_string()))
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
        Event::RunRecovered { .. } => String::from("recovered after restart"),
        Event::ModelRequested { model, .. } => format!("model requested: {model}"),
        Event::ModelDelta { .. } => String::from("model streamed output"),
        Event::ModelCompleted { .. } => String::from("model turn done"),
        Event::ToolRequested { tool, .. } => format!("tool requested: {tool}"),
        Event::ToolStarted { tool, .. } => format!("tool started: {tool}"),
        Event::ToolOutput { tool, truncated, .. } => format!(
            "tool output: {tool}{}", if *truncated { " (compacted)" } else { "" }),
        Event::ToolCompleted { tool, .. } => format!("tool done: {tool}"),
        Event::AgentSpawned { agent, .. } => format!("spawned sub-agent: {agent}"),
        Event::AgentMessage { agent, .. } => format!("sub-agent message: {agent}"),
        Event::AgentCompleted { agent, .. } => format!("sub-agent done: {agent}"),
        Event::MemoryProposed { .. } => String::from("memory write proposed"),
        Event::ApprovalRequested { scope, .. } => format!("approval requested: {scope}"),
        Event::ApprovalGranted { scope, .. } => format!("approval granted: {scope}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn round_trip_and_explain() {
        let ledger = Ledger::open_in_memory().unwrap();
        ledger.append(&Event::RunStarted { run_id: "r1".into() }).unwrap();
        ledger.append(&Event::ToolStarted { run_id: "r1".into(), tool: "shell".into() }).unwrap();
        ledger.append(&Event::RunCompleted { run_id: "r1".into() }).unwrap();
        assert_eq!(ledger.replay("r1").unwrap().len(), 3);
        assert!(ledger.explain("r1").unwrap().contains("completed"));
        assert_eq!(ledger.status("r1").unwrap().as_deref(), Some("completed"));
    }
}
