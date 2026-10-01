//! Durable occurrence claims (scheduler idempotency, §21).
//!
//! A fired job must not run twice even after a crash, so occurrence keys
//! claimed by the scheduler live in storage, not just in process memory.
//! First claim wins: `claim` is an INSERT OR IGNORE, so a recovered process
//! replaying the same occurrence gets `false` and does not start a second
//! run.
//!
//! Claim unification: this store and [`Ledger::claim`] share ONE table — the
//! ledger's `claims` table — when opened on the same database file. A key
//! claimed through either API is visible to the other, so exactly-once
//! semantics no longer depend on which call site claimed first. Older
//! `ClaimStore` databases used a separate `occurrence_claims` table; its rows
//! are migrated into `claims` on open and the legacy table is dropped.

use pantheon_api::error::{Layer, PantheonError};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;

/// The canonical idempotency-claims table. Column names match the ledger's
/// `claims` table exactly so a `ClaimStore` and a `Ledger` opened on the same
/// file read and write the same rows.
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS claims (
  key TEXT PRIMARY KEY,
  ts_ms INTEGER NOT NULL
);";

/// Durable twin of the tick driver's in-memory queue: one row per job id
/// that owes a queued fire. Set semantics (PRIMARY KEY on job_id) mirror
/// the driver's `HashSet` — re-queueing while a fire is already owed just
/// refreshes the timestamp. Rows are deleted when the drain is taken, on
/// abandon, and on claim failure; a row left behind by a crash is
/// re-driven by the next process at `tick_job` entry.
const DRAIN_QUEUE_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS drain_queue (
  job_id TEXT PRIMARY KEY,
  queued_ms INTEGER NOT NULL
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

/// One-time migration for databases created before claim unification, when
/// `ClaimStore` kept its own `occurrence_claims` table. Rows move into the
/// shared `claims` table (INSERT OR IGNORE keeps pre-existing canonical rows)
/// and the legacy table is dropped. Databases without the legacy table take
/// the early exit.
fn migrate_occurrence_claims(conn: &Connection) -> Result<(), PantheonError> {
    let legacy: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'occurrence_claims'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| err("CLAIM_MIGRATE", e.to_string()))?;
    if legacy.is_none() {
        return Ok(());
    }
    conn.execute(
        "INSERT OR IGNORE INTO claims (key, ts_ms) SELECT key, claimed_ms FROM occurrence_claims",
        [],
    )
    .map_err(|e| err("CLAIM_MIGRATE", e.to_string()))?;
    conn.execute("DROP TABLE occurrence_claims", [])
        .map_err(|e| err("CLAIM_MIGRATE", e.to_string()))?;
    Ok(())
}

impl ClaimStore {
    pub fn open(path: &Path) -> Result<Self, PantheonError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| err("CLAIM_MKDIR", e.to_string()))?;
            }
        }
        let conn = Connection::open(path).map_err(|e| err("CLAIM_OPEN", e.to_string()))?;
        crate::configure_durability(&conn, "CLAIM")?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| err("CLAIM_SCHEMA", e.to_string()))?;
        conn.execute_batch(DRAIN_QUEUE_SCHEMA)
            .map_err(|e| err("CLAIM_SCHEMA", e.to_string()))?;
        migrate_occurrence_claims(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> Result<Self, PantheonError> {
        let conn = Connection::open_in_memory().map_err(|e| err("CLAIM_OPEN", e.to_string()))?;
        crate::configure_durability(&conn, "CLAIM")?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| err("CLAIM_SCHEMA", e.to_string()))?;
        conn.execute_batch(DRAIN_QUEUE_SCHEMA)
            .map_err(|e| err("CLAIM_SCHEMA", e.to_string()))?;
        migrate_occurrence_claims(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Claim an occurrence. `true` means this call won the claim and the run
    /// should start; `false` means the key was already claimed (a replay)
    /// and no second run may start.
    ///
    /// Shares the ledger's `claims` table: a key claimed here is also claimed
    /// as far as `Ledger::claim` is concerned, and vice versa, whenever
    /// both stores are opened on the same database file.
    pub fn claim(&self, key: &str) -> Result<bool, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("CLAIM_LOCK", e.to_string()))?;
        let inserted = conn
            .execute(
                "INSERT OR IGNORE INTO claims (key, ts_ms) VALUES (?1, ?2)",
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
            .execute("DELETE FROM claims WHERE key = ?1", params![key])
            .map_err(|e| err("CLAIM_DELETE", e.to_string()))?;
        Ok(removed == 1)
    }

    pub fn is_claimed(&self, key: &str) -> Result<bool, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("CLAIM_LOCK", e.to_string()))?;
        conn.query_row("SELECT 1 FROM claims WHERE key = ?1", params![key], |_| {
            Ok(())
        })
        .optional()
        .map(|row| row.is_some())
        .map_err(|e| err("CLAIM_QUERY", e.to_string()))
    }

    pub fn len(&self) -> Result<usize, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("CLAIM_LOCK", e.to_string()))?;
        conn.query_row("SELECT COUNT(*) FROM claims", [], |r| r.get(0))
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
            .execute("DELETE FROM claims WHERE ts_ms < ?1", params![cutoff_ms])
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
            .prepare("SELECT key FROM claims ORDER BY key")
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

    /// Record that `job_id` owes one queued fire. Idempotent: re-queueing
    /// while a fire is already owed just refreshes the timestamp.
    pub fn enqueue_drain(&self, job_id: &str) -> Result<(), PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("CLAIM_LOCK", e.to_string()))?;
        conn.execute(
            "INSERT OR REPLACE INTO drain_queue (job_id, queued_ms) VALUES (?1, ?2)",
            params![job_id, now_ms()],
        )
        .map_err(|e| err("CLAIM_DRAIN_INSERT", e.to_string()))?;
        Ok(())
    }

    /// Drop the owed fire for `job_id`, if any. Returns true when a row
    /// was removed.
    pub fn dequeue_drain(&self, job_id: &str) -> Result<bool, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("CLAIM_LOCK", e.to_string()))?;
        let removed = conn
            .execute("DELETE FROM drain_queue WHERE job_id = ?1", params![job_id])
            .map_err(|e| err("CLAIM_DRAIN_DELETE", e.to_string()))?;
        Ok(removed == 1)
    }

    /// Atomically take `job_id`'s owed fire, if any: the SELECT and the
    /// DELETE run in one IMMEDIATE transaction, so two recovering
    /// processes (or a racing tick) agree on exactly one owner.
    ///
    /// Take-before-complete trade-off: the row is gone once taken, so a
    /// crash between the take and the drain firing loses the owed run.
    /// The alternative — deleting only after the drain completes — would
    /// re-fire a drain whose pre-crash run may already have executed the
    /// job, risking a duplicate run. Under-fire is the safer failure.
    pub fn take_pending_drain(&self, job_id: &str) -> Result<bool, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("CLAIM_LOCK", e.to_string()))?;
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| err("CLAIM_DRAIN_TXN", e.to_string()))?;
        let taken: Result<bool, PantheonError> = (|| {
            let owed: bool = conn
                .query_row(
                    "SELECT 1 FROM drain_queue WHERE job_id = ?1",
                    params![job_id],
                    |_| Ok(()),
                )
                .optional()
                .map_err(|e| err("CLAIM_DRAIN_QUERY", e.to_string()))?
                .is_some();
            if owed {
                conn.execute("DELETE FROM drain_queue WHERE job_id = ?1", params![job_id])
                    .map_err(|e| err("CLAIM_DRAIN_DELETE", e.to_string()))?;
            }
            Ok(owed)
        })();
        match taken {
            Ok(owed) => {
                conn.execute_batch("COMMIT")
                    .map_err(|e| err("CLAIM_DRAIN_TXN", e.to_string()))?;
                Ok(owed)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod drain_queue_tests {
    use super::*;

    #[test]
    fn enqueue_dequeue_round_trip() {
        let store = ClaimStore::open_in_memory().unwrap();
        assert!(!store.dequeue_drain("job-a").unwrap());
        store.enqueue_drain("job-a").unwrap();
        assert!(store.dequeue_drain("job-a").unwrap());
        assert!(!store.dequeue_drain("job-a").unwrap());
    }

    #[test]
    fn enqueue_is_idempotent() {
        let store = ClaimStore::open_in_memory().unwrap();
        store.enqueue_drain("job-a").unwrap();
        store.enqueue_drain("job-a").unwrap();
        // Still exactly one owed fire: one take consumes it.
        assert!(store.take_pending_drain("job-a").unwrap());
        assert!(!store.take_pending_drain("job-a").unwrap());
    }

    #[test]
    fn take_is_atomic_first_taker_wins() {
        let store = ClaimStore::open_in_memory().unwrap();
        store.enqueue_drain("job-a").unwrap();
        assert!(store.take_pending_drain("job-a").unwrap());
        // Second take (a racing recovery) finds nothing.
        assert!(!store.take_pending_drain("job-a").unwrap());
    }

    #[test]
    fn take_on_empty_is_false() {
        let store = ClaimStore::open_in_memory().unwrap();
        assert!(!store.take_pending_drain("nope").unwrap());
    }

    #[test]
    fn drain_rows_are_per_job() {
        let store = ClaimStore::open_in_memory().unwrap();
        store.enqueue_drain("job-a").unwrap();
        store.enqueue_drain("job-b").unwrap();
        assert!(store.take_pending_drain("job-a").unwrap());
        assert!(store.take_pending_drain("job-b").unwrap());
    }
}
