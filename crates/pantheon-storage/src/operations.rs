//! Durable operations.
//!
//! An operation is a small, versioned state machine persisted in the same
//! SQLite database as the event ledger.  Tool work is stored as an
//! operation before execution so a process can be replaced without losing
//! the operation or accidentally repeating a completed side effect.

use pantheon_core::error::{Layer, PantheonError};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

/// The only states a durable operation may occupy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationStatus {
    Ready,
    Awaiting,
    Canceling,
    Completed,
    Failed,
    Canceled,
}

impl OperationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Awaiting => "awaiting",
            Self::Canceling => "canceling",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }

    fn from_str(value: &str) -> Result<Self, PantheonError> {
        match value {
            "ready" => Ok(Self::Ready),
            "awaiting" => Ok(Self::Awaiting),
            "canceling" => Ok(Self::Canceling),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "canceled" => Ok(Self::Canceled),
            other => Err(operation_err(
                "OPERATION_STATUS",
                format!("unknown operation status {other:?}"),
            )),
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Canceled)
    }

    /// Check a state transition before it reaches SQLite.  The SQL CAS below
    /// remains authoritative; this is useful for callers that want a local
    /// error instead of a compare-and-swap failure.
    pub fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Ready, Self::Ready)
                | (Self::Awaiting, Self::Awaiting)
                | (Self::Canceling, Self::Canceling)
                | (Self::Ready, Self::Awaiting)
                | (Self::Ready, Self::Canceling)
                | (Self::Ready, Self::Completed)
                | (Self::Ready, Self::Failed)
                | (Self::Awaiting, Self::Completed)
                | (Self::Awaiting, Self::Failed)
                | (Self::Awaiting, Self::Canceling)
                | (Self::Canceling, Self::Canceled)
                | (Self::Canceling, Self::Failed)
        )
    }
}

/// A persisted durable operation.
///
/// `operation_type` is serialized as `type` on the wire.  Rust cannot use
/// `type` as a field name, but the public JSON and database schema retain
/// the spelling used by the protocol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Operation {
    pub id: String,
    #[serde(rename = "type")]
    pub operation_type: String,
    pub version: u64,
    pub status: OperationStatus,
    pub state: Value,
}

/// A compare-and-swap failure.  The operation row was changed by another
/// worker between the caller's read and write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationConflict {
    pub id: String,
    pub expected_version: u64,
    pub actual_version: Option<u64>,
    pub actual_status: Option<OperationStatus>,
}

impl std::fmt::Display for OperationConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "operation {} version conflict (expected {})",
            self.id, self.expected_version
        )
    }
}
impl std::error::Error for OperationConflict {}

/// Durable operation repository.  It deliberately owns a small mutex-guarded
/// SQLite connection so compare-and-swap updates are atomic even when called
/// by multiple supervisors in one process.
pub struct OperationStore {
    conn: Mutex<Connection>,
}

fn operation_err(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Storage,
        false,
        cause,
        "check the operation database and retry",
        "",
    )
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl OperationStore {
    pub fn open(path: &Path) -> Result<Self, PantheonError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| operation_err("OPERATION_MKDIR", e.to_string()))?;
            }
        }
        let conn =
            Connection::open(path).map_err(|e| operation_err("OPERATION_OPEN", e.to_string()))?;
        Self::from_connection(conn)
    }

    pub fn open_in_memory() -> Result<Self, PantheonError> {
        Self::from_connection(
            Connection::open_in_memory()
                .map_err(|e| operation_err("OPERATION_OPEN", e.to_string()))?,
        )
    }

    fn from_connection(conn: Connection) -> Result<Self, PantheonError> {
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(|e| operation_err("OPERATION_BUSY_TIMEOUT", e.to_string()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS operations (
               id TEXT PRIMARY KEY,
               operation_type TEXT NOT NULL,
               version INTEGER NOT NULL,
               status TEXT NOT NULL,
               state_json TEXT NOT NULL,
               created_ms INTEGER NOT NULL,
               updated_ms INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_operations_status ON operations(status, updated_ms);",
        )
        .map_err(|e| operation_err("OPERATION_SCHEMA", e.to_string()))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Create an operation in `ready` state.  IDs are caller supplied so an
    /// upstream task can use an idempotency key as the operation ID.
    pub fn create(
        &self,
        id: impl Into<String>,
        operation_type: impl Into<String>,
        state: Value,
    ) -> Result<Operation, PantheonError> {
        let id = id.into();
        if id.is_empty() {
            return Err(operation_err(
                "OPERATION_ID",
                "operation id cannot be empty".into(),
            ));
        }
        let operation_type = operation_type.into();
        if operation_type.is_empty() {
            return Err(operation_err(
                "OPERATION_TYPE",
                "operation type cannot be empty".into(),
            ));
        }
        let state_json = serde_json::to_string(&state)
            .map_err(|e| operation_err("OPERATION_SER", e.to_string()))?;
        let ts = now_ms();
        let conn = self
            .conn
            .lock()
            .map_err(|e| operation_err("OPERATION_LOCK", e.to_string()))?;
        conn.execute(
            "INSERT INTO operations (id, operation_type, version, status, state_json, created_ms, updated_ms)
             VALUES (?1, ?2, 0, 'ready', ?3, ?4, ?4)",
            params![id, operation_type, state_json, ts],
        )
        .map_err(|e| operation_err("OPERATION_CREATE", e.to_string()))?;
        drop(conn);
        self.get(&id)?.ok_or_else(|| {
            operation_err("OPERATION_CREATE", "created operation disappeared".into())
        })
    }

    pub fn get(&self, id: &str) -> Result<Option<Operation>, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| operation_err("OPERATION_LOCK", e.to_string()))?;
        let row = conn
            .query_row(
                "SELECT id, operation_type, version, status, state_json FROM operations WHERE id=?1",
                params![id],
                |r| {
                    let status: String = r.get(3)?;
                    let state: String = r.get(4)?;
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, status, state))
                },
            )
            .optional()
            .map_err(|e| operation_err("OPERATION_GET", e.to_string()))?;
        row.map(decode_row).transpose()
    }

    pub fn list(&self, status: Option<OperationStatus>) -> Result<Vec<Operation>, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| operation_err("OPERATION_LOCK", e.to_string()))?;
        let mut out = Vec::new();
        if let Some(status) = status {
            let mut stmt = conn
                .prepare("SELECT id, operation_type, version, status, state_json FROM operations WHERE status=?1 ORDER BY updated_ms, id")
                .map_err(|e| operation_err("OPERATION_LIST", e.to_string()))?;
            let rows = stmt
                .query_map(params![status.as_str()], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                })
                .map_err(|e| operation_err("OPERATION_LIST", e.to_string()))?;
            for row in rows {
                out.push(
                    decode_row(row.map_err(|e| operation_err("OPERATION_LIST", e.to_string()))?)
                        .map_err(|e| operation_err("OPERATION_LIST", e.to_string()))?,
                );
            }
        } else {
            let mut stmt = conn
                .prepare("SELECT id, operation_type, version, status, state_json FROM operations ORDER BY updated_ms, id")
                .map_err(|e| operation_err("OPERATION_LIST", e.to_string()))?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                })
                .map_err(|e| operation_err("OPERATION_LIST", e.to_string()))?;
            for row in rows {
                out.push(
                    decode_row(row.map_err(|e| operation_err("OPERATION_LIST", e.to_string()))?)
                        .map_err(|e| operation_err("OPERATION_LIST", e.to_string()))?,
                );
            }
        }
        Ok(out)
    }

    /// Atomically transition an operation using its version as a CAS token.
    pub fn transition(
        &self,
        id: &str,
        expected_version: u64,
        next: OperationStatus,
        state: Value,
    ) -> Result<Operation, PantheonError> {
        let state_json = serde_json::to_string(&state)
            .map_err(|e| operation_err("OPERATION_SER", e.to_string()))?;
        let conn = self
            .conn
            .lock()
            .map_err(|e| operation_err("OPERATION_LOCK", e.to_string()))?;
        let current: Option<(i64, String)> = conn
            .query_row(
                "SELECT version, status FROM operations WHERE id=?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| operation_err("OPERATION_CAS", e.to_string()))?;
        let Some((actual, status)) = current else {
            return Err(operation_err(
                "OPERATION_NOT_FOUND",
                format!("operation {id} not found"),
            ));
        };
        let current = OperationStatus::from_str(&status)?;
        if actual as u64 != expected_version || !current.can_transition_to(next) {
            return Err(operation_err(
                "OPERATION_CONFLICT",
                format!("operation {id} is version {actual} / {status}; expected version {expected_version} and transition to {}", next.as_str()),
            ));
        }
        let changed = conn
            .execute(
                "UPDATE operations SET version=version+1, status=?2, state_json=?3, updated_ms=?4
                 WHERE id=?1 AND version=?5",
                params![
                    id,
                    next.as_str(),
                    state_json,
                    now_ms(),
                    expected_version as i64
                ],
            )
            .map_err(|e| operation_err("OPERATION_CAS", e.to_string()))?;
        if changed != 1 {
            return Err(operation_err(
                "OPERATION_CONFLICT",
                format!("operation {id} changed concurrently"),
            ));
        }
        drop(conn);
        self.get(id)?
            .ok_or_else(|| operation_err("OPERATION_GET", "updated operation disappeared".into()))
    }

    pub fn complete(
        &self,
        id: &str,
        expected_version: u64,
        state: Value,
    ) -> Result<Operation, PantheonError> {
        self.transition(id, expected_version, OperationStatus::Completed, state)
    }

    pub fn fail(
        &self,
        id: &str,
        expected_version: u64,
        state: Value,
    ) -> Result<Operation, PantheonError> {
        self.transition(id, expected_version, OperationStatus::Failed, state)
    }

    /// Request cancellation.  The two-step canceling → canceled transition is
    /// intentional: the caller can persist cancellation intent, terminate the
    /// process group, and only then settle the operation.
    pub fn request_cancel(
        &self,
        id: &str,
        expected_version: u64,
        state: Value,
    ) -> Result<Operation, PantheonError> {
        self.transition(id, expected_version, OperationStatus::Canceling, state)
    }

    pub fn cancel(
        &self,
        id: &str,
        expected_version: u64,
        state: Value,
    ) -> Result<Operation, PantheonError> {
        self.transition(id, expected_version, OperationStatus::Canceled, state)
    }
}

fn decode_row(row: (String, String, i64, String, String)) -> Result<Operation, PantheonError> {
    let state =
        serde_json::from_str(&row.4).map_err(|e| operation_err("OPERATION_JSON", e.to_string()))?;
    Ok(Operation {
        id: row.0,
        operation_type: row.1,
        version: row.2 as u64,
        status: OperationStatus::from_str(&row.3)?,
        state,
    })
}

#[cfg(test)]
#[path = "operations_tests.rs"]
mod tests;
