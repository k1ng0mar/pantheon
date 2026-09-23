//! CAS run leases.
//!
//! Leases are deliberately separate from process liveness.  A supervisor
//! owns a lease while it is doing work, renews it from its activity loop, and
//! must stop mutating the run as soon as the lease can no longer be renewed.
//! This makes recovery safe even when a process is paused, partitioned, or
//! has lost its network connection.

use pantheon_core::error::{Layer, PantheonError};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

/// A lease as observed by a worker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunLease {
    pub run_id: String,
    #[serde(rename = "leaseId")]
    pub lease_id: String,
    #[serde(rename = "leaseUntil")]
    pub lease_until_ms: i64,
    #[serde(rename = "heartbeatMs")]
    pub heartbeat_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LostLeaseError {
    pub run_id: String,
    pub lease_id: String,
    pub lease_until_ms: Option<i64>,
}

impl LostLeaseError {
    pub fn new(run_id: impl Into<String>, lease_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
            lease_id: lease_id.into(),
            lease_until_ms: None,
        }
    }
}

impl std::fmt::Display for LostLeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "lost lease {} for run {}", self.lease_id, self.run_id)
    }
}
impl std::error::Error for LostLeaseError {}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn err(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Storage,
        false,
        cause,
        "check the run lease database and retry",
        "",
    )
}

/// SQLite-backed lease store.  The conditional UPDATE is the CAS primitive:
/// renewal only succeeds for the exact lease owner and only before expiry.
pub struct RunLeaseStore {
    conn: Mutex<Connection>,
}

impl RunLeaseStore {
    pub fn open(path: &Path) -> Result<Self, PantheonError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| err("LEASE_MKDIR", e.to_string()))?;
            }
        }
        let conn = Connection::open(path).map_err(|e| err("LEASE_OPEN", e.to_string()))?;
        Self::from_connection(conn)
    }

    pub fn open_in_memory() -> Result<Self, PantheonError> {
        Self::from_connection(
            Connection::open_in_memory().map_err(|e| err("LEASE_OPEN", e.to_string()))?,
        )
    }

    fn from_connection(conn: Connection) -> Result<Self, PantheonError> {
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(|e| err("LEASE_BUSY_TIMEOUT", e.to_string()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS run_leases (
               run_id TEXT PRIMARY KEY,
               lease_id TEXT NOT NULL,
               lease_until_ms INTEGER NOT NULL,
               heartbeat_ms INTEGER NOT NULL
             );",
        )
        .map_err(|e| err("LEASE_SCHEMA", e.to_string()))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Acquire a lease, or take over an expired lease.  Returns the lease
    /// only when this caller won the CAS.
    pub fn acquire(
        &self,
        run_id: &str,
        lease_id: &str,
        lease_ttl_ms: i64,
    ) -> Result<Option<RunLease>, PantheonError> {
        if run_id.is_empty() || lease_id.is_empty() || lease_ttl_ms <= 0 {
            return Err(err(
                "LEASE_INVALID",
                "run, lease, and positive TTL are required".into(),
            ));
        }
        let now = now_ms();
        let until = now.saturating_add(lease_ttl_ms);
        let mut conn = self
            .conn
            .lock()
            .map_err(|e| err("LEASE_LOCK", e.to_string()))?;
        // BEGIN IMMEDIATE is the cross-connection CAS boundary. A plain
        // SELECT followed by an upsert lets two supervisors both observe an
        // absent/expired row and both believe they won.
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| err("LEASE_ACQUIRE", e.to_string()))?;
        let current: Option<(String, i64)> = tx
            .query_row(
                "SELECT lease_id, lease_until_ms FROM run_leases WHERE run_id=?1",
                params![run_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| err("LEASE_READ", e.to_string()))?;
        let won = match current {
            None => true,
            Some((owner, expiry)) => owner == lease_id || expiry <= now,
        };
        if !won {
            tx.rollback()
                .map_err(|e| err("LEASE_ACQUIRE", e.to_string()))?;
            return Ok(None);
        }
        tx.execute(
            "INSERT INTO run_leases (run_id, lease_id, lease_until_ms, heartbeat_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(run_id) DO UPDATE SET lease_id=excluded.lease_id,
               lease_until_ms=excluded.lease_until_ms, heartbeat_ms=excluded.heartbeat_ms",
            params![run_id, lease_id, until, now],
        )
        .map_err(|e| err("LEASE_ACQUIRE", e.to_string()))?;
        tx.commit()
            .map_err(|e| err("LEASE_ACQUIRE", e.to_string()))?;
        Ok(Some(RunLease {
            run_id: run_id.to_string(),
            lease_id: lease_id.to_string(),
            lease_until_ms: until,
            heartbeat_ms: now,
        }))
    }

    /// Alias with an explicit name that reads well at call sites.
    pub fn acquire_run(
        &self,
        run_id: &str,
        lease_id: &str,
        ttl_ms: i64,
    ) -> Result<Option<RunLease>, PantheonError> {
        self.acquire(run_id, lease_id, ttl_ms)
    }

    /// Heartbeat / renew.  A failed renewal means the caller must abandon the
    /// run; it must not continue executing tools under a stale ownership
    /// assumption.
    pub fn renew(
        &self,
        run_id: &str,
        lease_id: &str,
        lease_ttl_ms: i64,
    ) -> Result<RunLease, PantheonError> {
        if lease_ttl_ms <= 0 {
            return Err(err("LEASE_INVALID", "positive TTL is required".into()));
        }
        let now = now_ms();
        let until = now.saturating_add(lease_ttl_ms);
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEASE_LOCK", e.to_string()))?;
        let changed = conn
            .execute(
                "UPDATE run_leases SET lease_until_ms=?3, heartbeat_ms=?4
                 WHERE run_id=?1 AND lease_id=?2 AND lease_until_ms > ?4",
                params![run_id, lease_id, until, now],
            )
            .map_err(|e| err("LEASE_RENEW", e.to_string()))?;
        if changed != 1 {
            let current: Option<i64> = conn
                .query_row(
                    "SELECT lease_until_ms FROM run_leases WHERE run_id=?1",
                    params![run_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| err("LEASE_READ", e.to_string()))?;
            let cause = match current {
                Some(until) => format!(
                    "run {run_id} lease {lease_id} is no longer owned; current expiry is {until}"
                ),
                None => format!("run {run_id} lease {lease_id} is no longer present"),
            };
            return Err(PantheonError::new(
                "LOST_LEASE",
                Layer::Runtime,
                true,
                cause,
                "stop work and reacquire the run lease",
                "",
            ));
        }
        Ok(RunLease {
            run_id: run_id.into(),
            lease_id: lease_id.into(),
            lease_until_ms: until,
            heartbeat_ms: now,
        })
    }

    pub fn renew_run(
        &self,
        run_id: &str,
        lease_id: &str,
        ttl_ms: i64,
    ) -> Result<RunLease, PantheonError> {
        self.renew(run_id, lease_id, ttl_ms)
    }

    /// Release only the caller's lease.  Releasing someone else's lease is a
    /// no-op, which makes cleanup safe after a lease handoff.
    pub fn release(&self, run_id: &str, lease_id: &str) -> Result<bool, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEASE_LOCK", e.to_string()))?;
        let n = conn
            .execute(
                "DELETE FROM run_leases WHERE run_id=?1 AND lease_id=?2",
                params![run_id, lease_id],
            )
            .map_err(|e| err("LEASE_RELEASE", e.to_string()))?;
        Ok(n == 1)
    }

    pub fn get(&self, run_id: &str) -> Result<Option<RunLease>, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEASE_LOCK", e.to_string()))?;
        conn.query_row(
            "SELECT run_id, lease_id, lease_until_ms, heartbeat_ms FROM run_leases WHERE run_id=?1",
            params![run_id],
            |r| {
                Ok(RunLease {
                    run_id: r.get(0)?,
                    lease_id: r.get(1)?,
                    lease_until_ms: r.get(2)?,
                    heartbeat_ms: r.get(3)?,
                })
            },
        )
        .optional()
        .map_err(|e| err("LEASE_READ", e.to_string()))
    }

    /// Check ownership without mutating the lease.
    pub fn assert_owned(&self, run_id: &str, lease_id: &str) -> Result<RunLease, LostLeaseError> {
        let lease = self
            .get(run_id)
            .ok()
            .flatten()
            .ok_or_else(|| LostLeaseError::new(run_id, lease_id))?;
        if lease.lease_id != lease_id || lease.lease_until_ms <= now_ms() {
            return Err(LostLeaseError {
                lease_until_ms: Some(lease.lease_until_ms),
                ..LostLeaseError::new(run_id, lease_id)
            });
        }
        Ok(lease)
    }

    /// Every unexpired lease. Used by destructive maintenance (reset) to
    /// refuse deleting state under a live session.
    pub fn list_active(&self) -> Result<Vec<RunLease>, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEASE_LOCK", e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT run_id, lease_id, lease_until_ms, heartbeat_ms
                 FROM run_leases WHERE lease_until_ms > ?1",
            )
            .map_err(|e| err("LEASE_LIST", e.to_string()))?;
        let rows = stmt
            .query_map(params![now_ms()], |r| {
                Ok(RunLease {
                    run_id: r.get(0)?,
                    lease_id: r.get(1)?,
                    lease_until_ms: r.get(2)?,
                    heartbeat_ms: r.get(3)?,
                })
            })
            .map_err(|e| err("LEASE_LIST", e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(|e| err("LEASE_LIST", e.to_string()))?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_live_owner_and_expiry_takeover() {
        let s = RunLeaseStore::open_in_memory().unwrap();
        assert!(s.acquire("r", "a", 60_000).unwrap().is_some());
        assert!(s.acquire("r", "b", 60_000).unwrap().is_none());
        assert!(s.renew("r", "a", 60_000).is_ok());
        assert!(s.renew("r", "b", 60_000).is_err());
        assert!(s.release("r", "b").unwrap() == false);
        assert!(s.release("r", "a").unwrap());
        assert!(s.acquire("r", "b", 60_000).unwrap().is_some());
    }

    #[test]
    fn cross_connection_acquire_has_one_winner() {
        use std::sync::{Arc, Barrier};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("leases.db");
        let left = Arc::new(RunLeaseStore::open(&path).unwrap());
        let right = Arc::new(RunLeaseStore::open(&path).unwrap());
        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for (store, owner) in [(Arc::clone(&left), "left"), (Arc::clone(&right), "right")] {
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                store.acquire("run", owner, 60_000).unwrap()
            }));
        }
        barrier.wait();
        let winners = handles
            .into_iter()
            .map(|h| h.join().unwrap().is_some())
            .filter(|won| *won)
            .count();
        assert_eq!(winners, 1);
    }

    #[test]
    fn assert_owned_reports_lost_lease() {
        let s = RunLeaseStore::open_in_memory().unwrap();
        s.acquire("r", "a", 60_000).unwrap();
        let e = s.assert_owned("r", "b").unwrap_err();
        assert_eq!(e.run_id, "r");
    }
}
