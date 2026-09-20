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
    PantheonError::new(code, Layer::Storage, false, cause,
        "check the storage path permissions and disk space", "")
}

impl ClaimStore {
    pub fn open(path: &Path) -> Result<Self, PantheonError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| err("CLAIM_MKDIR", e.to_string()))?;
            }
        }
        let conn = Connection::open(path).map_err(|e| err("CLAIM_OPEN", e.to_string()))?;
        conn.execute_batch(SCHEMA).map_err(|e| err("CLAIM_SCHEMA", e.to_string()))?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    pub fn open_in_memory() -> Result<Self, PantheonError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| err("CLAIM_OPEN", e.to_string()))?;
        conn.execute_batch(SCHEMA).map_err(|e| err("CLAIM_SCHEMA", e.to_string()))?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Claim an occurrence. `true` means this call won the claim and the run
    /// should start; `false` means the key was already claimed (a replay)
    /// and no second run may start.
    pub fn claim(&self, key: &str) -> Result<bool, PantheonError> {
        let conn = self.conn.lock().map_err(|e| err("CLAIM_LOCK", e.to_string()))?;
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO occurrence_claims (key, claimed_ms) VALUES (?1, ?2)",
            params![key, now_ms()],
        ).map_err(|e| err("CLAIM_INSERT", e.to_string()))?;
        Ok(inserted == 1)
    }

    /// Release a claim after its run ends. `true` means a claim existed and
    /// was removed; `false` means there was nothing to release.
    pub fn release(&self, key: &str) -> Result<bool, PantheonError> {
        let conn = self.conn.lock().map_err(|e| err("CLAIM_LOCK", e.to_string()))?;
        let removed = conn.execute(
            "DELETE FROM occurrence_claims WHERE key = ?1",
            params![key],
        ).map_err(|e| err("CLAIM_DELETE", e.to_string()))?;
        Ok(removed == 1)
    }

    pub fn is_claimed(&self, key: &str) -> Result<bool, PantheonError> {
        let conn = self.conn.lock().map_err(|e| err("CLAIM_LOCK", e.to_string()))?;
        conn.query_row(
            "SELECT 1 FROM occurrence_claims WHERE key = ?1", params![key],
            |_| Ok(()),
        ).optional()
            .map(|row| row.is_some())
            .map_err(|e| err("CLAIM_QUERY", e.to_string()))
    }

    pub fn len(&self) -> Result<usize, PantheonError> {
        let conn = self.conn.lock().map_err(|e| err("CLAIM_LOCK", e.to_string()))?;
        conn.query_row("SELECT COUNT(*) FROM occurrence_claims", [], |r| r.get(0))
            .map_err(|e| err("CLAIM_QUERY", e.to_string()))
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
        let conn = self.conn.lock().map_err(|e| err("CLAIM_LOCK", e.to_string()))?;
        let pruned = conn.execute(
            "DELETE FROM occurrence_claims WHERE claimed_ms < ?1",
            params![cutoff_ms],
        ).map_err(|e| err("CLAIM_DELETE", e.to_string()))?;
        Ok(pruned)
    }

    /// All currently claimed keys, sorted. Used to rebuild the in-memory
    /// ledger of a recovered process.
    pub fn names(&self) -> Result<Vec<String>, PantheonError> {
        let conn = self.conn.lock().map_err(|e| err("CLAIM_LOCK", e.to_string()))?;
        let mut stmt = conn.prepare("SELECT key FROM occurrence_claims ORDER BY key")
            .map_err(|e| err("CLAIM_QUERY", e.to_string()))?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| err("CLAIM_QUERY", e.to_string()))?;
        let mut keys = Vec::new();
        for row in rows {
            keys.push(row.map_err(|e| err("CLAIM_QUERY", e.to_string()))?);
        }
        Ok(keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn first_claim_wins_and_replay_is_rejected() {
        let store = ClaimStore::open_in_memory().unwrap();
        assert!(store.claim("job:nightly:1789914600000").unwrap());
        assert!(!store.claim("job:nightly:1789914600000").unwrap());
        assert!(store.is_claimed("job:nightly:1789914600000").unwrap());
        assert_eq!(store.len().unwrap(), 1);
    }

    #[test]
    fn distinct_occurrences_are_distinct_claims() {
        let store = ClaimStore::open_in_memory().unwrap();
        assert!(store.claim("job:a:1").unwrap());
        assert!(store.claim("job:a:2").unwrap());
        assert!(store.claim("job:b:1").unwrap());
        assert_eq!(store.len().unwrap(), 3);
    }

    #[test]
    fn release_makes_a_key_claimable_again() {
        let store = ClaimStore::open_in_memory().unwrap();
        assert!(store.claim("k").unwrap());
        assert!(store.release("k").unwrap());
        assert!(!store.release("k").unwrap(), "second release is a no-op");
        assert!(store.claim("k").unwrap(), "released keys are claimable again");
        assert_eq!(store.len().unwrap(), 1);
    }

    #[test]
    fn claims_survive_a_reopen_like_a_crash_restart() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("claims.sqlite");
        {
            let store = ClaimStore::open(&path).unwrap();
            assert!(store.claim("job:nightly:100").unwrap());
        }
        // New handle over the same file: what was claimed stays claimed.
        let store = ClaimStore::open(&path).unwrap();
        assert!(store.is_claimed("job:nightly:100").unwrap());
        assert!(!store.claim("job:nightly:100").unwrap());
        assert_eq!(store.names().unwrap(), vec!["job:nightly:100"]);
    }

    #[test]
    fn names_are_sorted() {
        let store = ClaimStore::open_in_memory().unwrap();
        store.claim("b").unwrap();
        store.claim("a").unwrap();
        store.claim("c").unwrap();
        assert_eq!(store.names().unwrap(), vec!["a", "b", "c"]);
    }

    #[test]
    fn prune_before_bounds_growth_and_keeps_recent() {
        let store = ClaimStore::open_in_memory().unwrap();
        let now = now_ms();
        store.claim("fresh").unwrap();
        store.claim("newer").unwrap();
        // A cutoff in the past keeps everything: recent claims survive.
        assert_eq!(store.prune_before(now - 1000).unwrap(), 0);
        assert_eq!(store.len().unwrap(), 2);
        // A cutoff ahead of now evicts everything claimed before it.
        assert_eq!(store.prune_before(now + 1).unwrap(), 2);
        assert_eq!(store.len().unwrap(), 0);
        assert_eq!(store.names().unwrap(), Vec::<String>::new());
    }
}