//! Durable occurrence claims (scheduler idempotency, §21).
//!
//! A fired job must not run twice even after a crash, so occurrence keys
//! claimed by the scheduler live in storage, not just in process memory.
//! First claim wins: `claim` is an INSERT OR IGNORE, so a recovered process
//! replaying the same occurrence gets `false` and does not start a second
//! run.
//!
//! Additive by design: this table and API sit next to the event ledger and
//! change nothing about run/event handling.

use pantheon_core::error::{Layer, PantheonError};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS occurrence_claims (
  key TEXT PRIMARY KEY,
  claimed_ms INTEGER NOT NULL
);";

/// SQLite-persisted occurrence claims.
///
/// One row per claimed occurrence key. `claim` is idempotent and atomic:
/// concurrent replicas racing on the same key agree on exactly one winner.
#[derive(Debug)]
pub struct ClaimStore {
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
    PantheonError::new(
        code,
        Layer::Storage,
        false,
        cause,
        "check the storage path permissions and disk space",
        "",
    )
}

impl ClaimStore {
    pub fn open(path: &Path) -> Result<Self, PantheonError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| err("CLAIM_MKDIR", e.to_string()))?;
            }
        }
        let conn = Connection::open(path).map_err(|e| err("CLAIM_OPEN", e.to_string()))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| err("CLAIM_SCHEMA", e.to_string()))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> Result<Self, PantheonError> {
        let conn = Connection::open_in_memory().map_err(|e| err("CLAIM_OPEN", e.to_string()))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| err("CLAIM_SCHEMA", e.to_string()))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Claim an occurrence. `true` means this call won the claim and the run
    /// should start; `false` means the key was already claimed (a replay)
    /// and no second run may start.
    pub fn claim(&self, key: &str) -> Result<bool, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("CLAIM_LOCK", e.to_string()))?;
        let inserted = conn
            .execute(
                "INSERT OR IGNORE INTO occurrence_claims (key, claimed_ms) VALUES (?1, ?2)",
                params![key, now_ms()],
            )
            .map_err(|e| err("CLAIM_INSERT", e.to_string()))?;
        Ok(inserted == 1)
    }

    /// Release a claim after its run ends. `true` means a claim existed and
    /// was removed; `false` means there was nothing to release.
    pub fn release(&self, key: &str) -> Result<bool, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("CLAIM_LOCK", e.to_string()))?;
        let removed = conn
            .execute("DELETE FROM occurrence_claims WHERE key = ?1", params![key])
            .map_err(|e| err("CLAIM_DELETE", e.to_string()))?;
        Ok(removed == 1)
    }

    pub fn is_claimed(&self, key: &str) -> Result<bool, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("CLAIM_LOCK", e.to_string()))?;
        conn.query_row(
            "SELECT 1 FROM occurrence_claims WHERE key = ?1",
            params![key],
            |_| Ok(()),
        )
        .optional()
        .map(|row| row.is_some())
        .map_err(|e| err("CLAIM_QUERY", e.to_string()))
    }

    pub fn len(&self) -> Result<usize, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("CLAIM_LOCK", e.to_string()))?;
        conn.query_row("SELECT COUNT(*) FROM occurrence_claims", [], |r| r.get(0))
            .map_err(|e| err("CLAIM_QUERY", e.to_string()))
    }

    pub fn is_empty(&self) -> Result<bool, PantheonError> {
        Ok(self.len()? == 0)
    }

    /// Forget claims older than `cutoff_ms` (retention policy).
    ///
    /// Occurrence keys are never released when a run finishes — a late
    /// redelivery must still collapse onto the run it already fired — so the
    /// table only grows. Retention prunes keys nobody can fire again: an
    /// occurrence older than the window is indistinguishable from a new one
    /// for replay purposes, exactly the bounded trade-off the gateway dedup
    /// window makes. Set the window comfortably above the longest redelivery
    /// backlog a sender can replay.
    pub fn prune_before(&self, cutoff_ms: i64) -> Result<usize, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("CLAIM_LOCK", e.to_string()))?;
        let pruned = conn
            .execute(
                "DELETE FROM occurrence_claims WHERE claimed_ms < ?1",
                params![cutoff_ms],
            )
            .map_err(|e| err("CLAIM_DELETE", e.to_string()))?;
        Ok(pruned)
    }

    /// All currently claimed keys, sorted. Used to rebuild the in-memory
    /// ledger of a recovered process.
    pub fn names(&self) -> Result<Vec<String>, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("CLAIM_LOCK", e.to_string()))?;
        let mut stmt = conn
            .prepare("SELECT key FROM occurrence_claims ORDER BY key")
            .map_err(|e| err("CLAIM_QUERY", e.to_string()))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| err("CLAIM_QUERY", e.to_string()))?;
        let mut keys = Vec::new();
        for row in rows {
            keys.push(row.map_err(|e| err("CLAIM_QUERY", e.to_string()))?);
        }
        Ok(keys)
    }
}

#[cfg(test)]
#[path = "claims_tests.rs"]
mod tests;
