//! Storage: SQLite-backed event-sourced execution ledger (§12 + §19).
//! Default backend is SQLite (rusqlite, bundled). Postgres later.

use pantheon_api::error::{Layer, PantheonError};
use rusqlite::Connection;
use std::time::Duration;

/// Durability pragmas every store in this crate applies on open, file-backed
/// or in-memory: a 5s busy timeout (second process / thread never hits
/// SQLITE_BUSY immediately), WAL journal mode (concurrent readers don't block
/// writers; crash mid-commit leaves a replayable WAL, not a hot journal), and
/// an explicit `synchronous = NORMAL` (fully durable under WAL without the
/// double-fsync tax rollback-journal `FULL` imposes).
///
/// The `code` prefix names the store for error codes, e.g. `"LEDGER"`.
///
/// Checkpoint policy (deliberately NOT enforced here): SQLite's default
/// `wal_autocheckpoint` (1000 pages) bounds WAL growth in normal use. Long
/// write bursts should issue `PRAGMA wal_checkpoint(TRUNCATE)` from an idle
/// hook or maintenance pass instead — auto-checkpointing on close would turn a
/// cheap disconnect into a latency spike, and doing it here would hide the
/// cost inside every store open. Retention likewise stays an explicit API
/// (e.g. [`ledger::Ledger::prune_events_before`]); nothing in this crate wires
/// automatic pruning into open paths.
pub(crate) fn configure_durability(conn: &Connection, code: &str) -> Result<(), PantheonError> {
    let err = |suffix: &str, cause: String| {
        PantheonError::new(
            format!("{code}_{suffix}"),
            Layer::Storage,
            false,
            cause,
            "check the storage path permissions and disk space",
            "",
        )
    };
    // Busy timeout first: the WAL pragma below can itself block when another
    // process holds the write lock.
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| err("BUSY_TIMEOUT", e.to_string()))?;
    // `PRAGMA journal_mode = WAL` returns the resulting mode as a row;
    // ignore it — on :memory: connections it reports "memory" (a no-op).
    conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get::<_, String>(0))
        .map_err(|e| err("WAL", e.to_string()))?;
    conn.execute_batch("PRAGMA synchronous = NORMAL")
        .map_err(|e| err("SYNCHRONOUS", e.to_string()))?;
    Ok(())
}

pub mod claims;
pub mod leases;
pub mod ledger;
pub mod operations;

pub mod audit;
pub mod collaboration;
pub mod search;
pub use audit::{audit_line, export_jsonl};
pub use claims::ClaimStore;
pub use collaboration::{
    AgentMessage, AgentTask, Collaboration, CollaborationStatus, CollaborationStore, MessageKind,
    TaskConflict, TaskMutationError, TaskStatus,
};
pub use leases::{LostLeaseError, RunLease, RunLeaseStore};
pub use ledger::{Artifact, Ledger, LedgerEntry, RunListing, RunMetrics};
pub use operations::{Operation, OperationConflict, OperationStatus, OperationStore};
pub use search::{recreate_search_index, search_index_health};
pub use search::{SearchHit, SessionChunk, SessionSearch};
