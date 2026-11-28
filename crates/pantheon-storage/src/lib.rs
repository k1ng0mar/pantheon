//! sqlite-backed event-sourced ledger. rusqlite, bundled, no server.

use pantheon_api::error::{Layer, PantheonError};
use rusqlite::Connection;
use std::time::Duration;

/// the pragmas every store here applies on open: 5s busy timeout, WAL, and
/// synchronous=NORMAL.
///
/// NORMAL survives a process crash but not a power cut - it may lose recent
/// commits there. if you need the latter, switch to FULL, but you pay a second
/// fsync. we took the cheap option on purpose, this is a ledger not a bank.
///
/// checkpointing is left to sqlite's autocheckpoint (1000 pages, covers normal
/// use). a long write burst should run wal_checkpoint(TRUNCATE) from a
/// maintenance pass. doing it on close makes a disconnect look like a hang.
///
/// `code` prefixes the error codes, e.g. "LEDGER".
pub(crate) fn configure_durability(conn: &Connection, code: &str) -> Result<(), PantheonError> {
    // the db file inherits the process umask. 022 leaves ledger.db at 0644 and
    // the data dir at 0755, so any local user can read every transcript while
    // the secret stores next to them are 0600. this is the one open path all the
    // file-backed stores share, so fix it here instead of at each caller.
    if let Some(path) = conn.path() {
        crate::privacy::tighten_database(std::path::Path::new(path));
    }
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
    // timeout first, the WAL pragma below can block on another process's lock
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|e| err("BUSY_TIMEOUT", e.to_string()))?;
    // journal_mode returns the resulting mode as a row, ignore it. on :memory:
    // connections it just reports "memory".
    conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get::<_, String>(0))
        .map_err(|e| err("WAL", e.to_string()))?;
    conn.execute_batch("PRAGMA synchronous = NORMAL")
        .map_err(|e| err("SYNCHRONOUS", e.to_string()))?;
    Ok(())
}

/// one forward-only ADD COLUMN.
///
/// "duplicate column name" means it's already there (fresh dbs get every column
/// from the SCHEMA constants), so we swallow it and the migration stays
/// idempotent. everything else propagates - if this returns Ok, it really ran.
///
/// matching on the message text because sqlite gives this a plain SQLITE_ERROR
/// with no separate code.
pub(crate) fn add_column_once(conn: &Connection, sql: &str) -> Result<(), rusqlite::Error> {
    match conn.execute(sql, []) {
        Ok(_) => Ok(()),
        Err(e)
            if e.to_string()
                .to_lowercase()
                .contains("duplicate column name") =>
        {
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// A live claim on one run's event stream.
///
/// The holder may append through [`Ledger::append_fenced`]; anyone else's
/// fenced append fails. Claims carry the writer's pid and timestamp so a
/// crashed process can be taken over instead of blocking the run forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterClaim {
    pub run_id: String,
    pub token: String,
    pub pid: i64,
    pub claimed_ms: i64,
}

/// True when `pid` names a live process. `/proc` is the Linux answer; on
/// other platforms we assume the holder is alive, which fails closed (a
/// false "busy" is recoverable by waiting, a false "dead" is not).
pub(crate) fn pid_is_alive(pid: i64) -> bool {
    if pid <= 0 {
        return false;
    }
    let proc = std::path::Path::new("/proc");
    if proc.is_dir() {
        proc.join(pid.to_string()).exists()
    } else {
        true
    }
}

mod core_group;
mod init_lock;
pub use init_lock::{InitLock, lock_path as init_lock_path};
mod misc_group;
mod privacy;
pub use audit::{audit_line, export_jsonl};
pub use claims::ClaimStore;
pub use collaboration::{
    AgentMessage, AgentTask, Collaboration, CollaborationStatus, CollaborationStore, MessageKind,
    TaskConflict, TaskMutationError, TaskStatus,
};
pub use core_group::claims;
pub use core_group::leases;
pub use core_group::ledger;
pub use core_group::ledger::{ImageGeneration, ModelAttempt};
pub use core_group::operations;
pub use ideas::{
    FeedbackSignal, Idea, IdeaKind, IdeaStatus, IdeaStore, NewIdea, ScheduleSpec, TopicSignals,
};
pub use leases::{LostLeaseError, RunLease, RunLeaseStore};
pub use ledger::{Artifact, Ledger, LedgerEntry, RunListing, RunMetrics, HOME_SESSION_ID};
pub use misc_group::audit;
pub use misc_group::backup;
pub use misc_group::collaboration;
pub use misc_group::ideas;
pub use misc_group::search;
pub use operations::{Operation, OperationConflict, OperationStatus, OperationStore};
pub use privacy::tighten_database;
pub use search::{recreate_search_index, search_index_health};
pub use search::{SearchHit, SessionChunk, SessionSearch};
