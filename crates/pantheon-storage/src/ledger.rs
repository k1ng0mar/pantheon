//! Event-sourced execution ledger. Every run persists its events to SQLite;
//! ``pantheon logs run_X`` replays them. History is append-only.
use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::events::Event;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

/// Per-run counters, folded from the ledger's event log.
///
/// `ContextTrimmed`/`ContextCompressed` are here because an operator watching
/// those numbers is the one who needs to know a run has been quietly losing
/// context, and the raw event stream is far too noisy to notice it in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMetrics {
    pub runs_started: u64,
    pub runs_completed: u64,
    pub runs_failed: u64,
    pub runs_canceled: u64,
    pub tool_calls: u64,
    pub model_turns: u64,
    pub approvals_requested: u64,
    pub approvals_granted: u64,
    pub approvals_denied: u64,
    pub agents_spawned: u64,
    pub context_trims: u64,
    pub context_compressions: u64,
}

impl std::fmt::Display for RunMetrics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} started / {} completed / {} failed / {} canceled; {} tool calls; {} model turns; {} approvals ({} granted, {} denied); {} sub-agents; {} context trims ({} compressed)",
            self.runs_started,
            self.runs_completed,
            self.runs_failed,
            self.runs_canceled,
            self.tool_calls,
            self.model_turns,
            self.approvals_requested,
            self.approvals_granted,
            self.approvals_denied,
            self.agents_spawned,
            self.context_trims,
            self.context_compressions,
        )
    }
}

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

/// Apply `TurnRewound` markers to an ordered event stream. The named turn
/// and everything after it up to the marker is dropped; the marker itself
/// is kept so the audit trail shows a rewind happened. Turns started after
/// the marker replay normally. The `events` table is never rewritten — this
/// is a read-time projection, so raw history survives for forensics while
/// resume and transcript rebuilds see the rewound run as if those turns
/// never happened.
fn apply_rewinds(entries: Vec<LedgerEntry>) -> Vec<LedgerEntry> {
    let mut out: Vec<LedgerEntry> = Vec::with_capacity(entries.len());
    for entry in entries {
        if let Event::TurnRewound { turn_id, .. } = &entry.event {
            if let Some(pos) = out.iter().rposition(
                |e| matches!(&e.event, Event::TurnStarted { turn_id: t, .. } if t == turn_id),
            ) {
                out.truncate(pos);
            }
        }
        out.push(entry);
    }
    out
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
        | Event::AgentBound { run_id, .. }
        | Event::TurnStarted { run_id, .. }
        | Event::TurnParked { run_id, .. }
        | Event::TurnCompleted { run_id, .. }
        | Event::TurnFailed { run_id, .. }
        | Event::TurnRewound { run_id, .. }
        | Event::CheckpointCreated { run_id, .. }
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
        | Event::DecisionRequested { run_id, .. }
        | Event::DecisionMade { run_id, .. }
        | Event::DecisionRecorded { run_id, .. }
        | Event::ContextTrimmed { run_id, .. }
        | Event::ContextCompressed { run_id, .. }
        | Event::SessionTitled { run_id, .. }
        | Event::AssistantMessage { run_id, .. }
        | Event::ToolMessage { run_id, .. }
        | Event::ImportedReasoning { run_id, .. }
        | Event::UsageRecorded { run_id, .. }
        | Event::SteeringProvided { run_id, .. }
        | Event::UserInputRequested { run_id, .. }
        | Event::UserInputProvided { run_id, .. } => run_id,
    }
}

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS runs (
  run_id TEXT PRIMARY KEY,
  created_ms INTEGER NOT NULL,
  status TEXT NOT NULL DEFAULT 'running',
  title TEXT,
  agent_id TEXT
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

/// One row of [`Ledger::list_runs`]: `(run_id, status, created_ms, title)`.
/// `title` is `None` when the run has never been titled.
pub type RunListing = (String, String, i64, Option<String>);

/// Forward-only column migrations for ledgers created before a column
/// existed. Fresh databases already carry every column from SCHEMA, so the
/// ALTER fails harmlessly with "duplicate column name" and is ignored.
fn migrate(conn: &Connection) -> Result<(), PantheonError> {
    let _ = conn.execute("ALTER TABLE runs ADD COLUMN title TEXT", []);
    // Run -> agent binding. A run belongs to exactly one agent profile for
    // its whole life, so this is written once when the run row is created
    // and never updated. NULL means "pre-binding run" (created before
    // profiles existed) and is read back as such rather than guessed.
    let _ = conn.execute("ALTER TABLE runs ADD COLUMN agent_id TEXT", []);
    let _ = conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_runs_agent ON runs(agent_id, created_ms DESC)",
        [],
    );
    Ok(())
}

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
        crate::configure_durability(&conn, "LEDGER")?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| err("LEDGER_SCHEMA", e.to_string()))?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }
    pub fn open_in_memory() -> Result<Self, PantheonError> {
        let conn = Connection::open_in_memory().map_err(|e| err("LEDGER_OPEN", e.to_string()))?;
        crate::configure_durability(&conn, "LEDGER")?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| err("LEDGER_SCHEMA", e.to_string()))?;
        migrate(&conn)?;
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
        // Derived read model: the run's owning agent. Written once and
        // only once — a run that is already bound to a different agent is
        // a hard error, not an overwrite. Identity is immutable for the
        // life of a run; that is what makes "resume Nyx's session" safe.
        if let Event::AgentBound { agent_id, .. } = event {
            let current: Option<String> = conn
                .query_row(
                    "SELECT agent_id FROM runs WHERE run_id = ?1",
                    params![run_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| err("LEDGER_AGENT_BIND", e.to_string()))?
                .flatten();
            match current {
                Some(existing) if existing != *agent_id => {
                    return Err(err(
                        "LEDGER_AGENT_REBOUND",
                        format!(
                            "run {run_id} is already bound to agent {existing:?}; \
                             refusing to rebind to {agent_id:?}"
                        ),
                    ));
                }
                Some(_) => {}
                None => {
                    conn.execute(
                        "UPDATE runs SET agent_id = ?2 WHERE run_id = ?1",
                        params![run_id, agent_id],
                    )
                    .map_err(|e| err("LEDGER_AGENT_BIND", e.to_string()))?;
                }
            }
        }
        // Derived read model: the latest title event is the run's display
        // title. Overwrites unconditionally so a manual rename or a newer
        // model pass wins (last write is authoritative).
        if let Event::SessionTitled { title, .. } = event {
            conn.execute(
                "UPDATE runs SET title = ?2 WHERE run_id = ?1",
                params![run_id, title],
            )
            .map_err(|e| err("LEDGER_UPDATE", e.to_string()))?;
        }
        if matches!(event, Event::RunFailed { .. } | Event::TurnFailed { .. }) {
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
        // The rowid and the global `seq` coincide only while no event row is
        // ever deleted; the stored seq is the authority, so read it back
        // rather than assuming they match.
        let seq: i64 = conn
            .query_row("SELECT seq FROM events WHERE id = ?1", params![id], |r| {
                r.get(0)
            })
            .map_err(|e| err("LEDGER_APPEND", e.to_string()))?;
        conn.commit()
            .map_err(|e| err("LEDGER_APPEND", e.to_string()))?;
        Ok(LedgerEntry {
            id,
            run_id,
            seq,
            ts_ms: ts,
            event: event.clone(),
        })
    }

    /// Replay a run's events in append order, with `TurnRewound` markers
    /// applied: rewound turns are excluded as if they never happened. Raw
    /// history is preserved in the `events` table; this is the effective
    /// projection used by resume, transcript rebuilds, and the run log.
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
        Ok(apply_rewinds(out))
    }

    /// Usage accounting rows in `[from_ms, to_ms)`, for `pantheon stats`.
    /// Filters on the serialized variant tag in SQL so a month of events
    /// doesn't need full deserialization; only `UsageRecorded` rows are
    /// decoded. Rewind truncation is intentionally NOT applied: stats
    /// reports tokens actually consumed, including rewound turns.
    pub fn usage_between(
        &self,
        from_ms: i64,
        to_ms: i64,
    ) -> Result<Vec<LedgerEntry>, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT id, run_id, seq, ts_ms, event_json FROM events \
                 WHERE ts_ms >= ?1 AND ts_ms < ?2 AND event_json LIKE '%\"UsageRecorded\"%' \
                 ORDER BY ts_ms, id",
            )
            .map_err(|e| err("LEDGER_USAGE", e.to_string()))?;
        let rows = stmt
            .query_map(params![from_ms, to_ms], |row| {
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
            .map_err(|e| err("LEDGER_USAGE", e.to_string()))?;
        let mut out = vec![];
        for r in rows {
            out.push(r.map_err(|e| err("LEDGER_USAGE", e.to_string()))?);
        }
        Ok(out)
    }

    /// Idempotency claim for the scheduler (spec section 21): occurrence key,
    /// replay-safe. Returns true if this claimer was the first.
    ///
    /// Shares the `claims` table with `ClaimStore`: a key claimed here is
    /// also claimed as far as `ClaimStore::claim` is concerned, and vice
    /// versa, whenever both stores are opened on the same database file.
    pub fn claim(&self, key: &str) -> Result<bool, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        let inserted = conn
            .execute(
                "INSERT OR IGNORE INTO claims (key, ts_ms) VALUES (?1, ?2)",
                params![key, now_ms()],
            )
            .map_err(|e| err("LEDGER_CLAIM", e.to_string()))?;
        Ok(inserted == 1)
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

    /// Delete a run's process-group row. Takes `lease_id` deliberately:
    /// an unowned variant would let any process with ledger access reap
    /// another lease's process group, so the ownership check is not
    /// optional and there is no ungated twin of this method.
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

    /// The agent profile bound to this run, if any.
    ///
    /// `None` means the run predates profiles (or was never bound); it is
    /// never defaulted to a profile name, because guessing an owner would
    /// make a run look like it belongs to an agent that never ran it.
    pub fn run_agent(&self, run_id: &str) -> Result<Option<String>, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        conn.query_row(
            "SELECT agent_id FROM runs WHERE run_id=?1",
            params![run_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()
        .map(|o| o.flatten())
        .map_err(|e| err("LEDGER_AGENT_BIND", e.to_string()))
    }

    /// List recent runs, newest first. Powers the REPL `/runs` picker and
    /// auto-resume. `limit` bounds the row count. The fourth element is the
    /// run's current display title (`None` = never titled).
    pub fn list_runs(&self, limit: usize) -> Result<Vec<RunListing>, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT run_id, status, created_ms, title FROM runs ORDER BY created_ms DESC, rowid DESC LIMIT ?1",
            )
            .map_err(|e| err("LEDGER_QUERY", e.to_string()))?;
        let rows = stmt
            .query_map(params![limit as i64], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(|e| err("LEDGER_QUERY", e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| err("LEDGER_QUERY", e.to_string()))?);
        }
        Ok(out)
    }

    /// The current display title for one run (latest `SessionTitled`
    /// event), or `None` when the run was never titled.
    pub fn run_title(&self, run_id: &str) -> Result<Option<String>, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        conn.query_row(
            "SELECT title FROM runs WHERE run_id=?1",
            params![run_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(|e| err("LEDGER_STATUS", e.to_string()))
        .map(|o| o.flatten())
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

    /// Delete events with `ts_ms` older than `cutoff_ts_ms` (retention policy).
    ///
    /// The events table is append-only and otherwise grows forever; this is
    /// the explicit retention hook a maintenance cadence (e.g. the scheduler
    /// worker) calls. Nothing automatic runs on open — see
    /// [`configure_durability`]'s checkpoint note. Returns the number of
    /// event rows pruned.
    ///
    /// Derived `runs` rows are left alone: a pruned run keeps its status
    /// row but loses its event history, which is exactly what "keep titles
    /// and outcomes, drop transcripts" retention means. Pair with
    /// `SessionSearch::prune_before` so the FTS sidecar does not retain text
    /// for history the ledger has already pruned.
    pub fn prune_events_before(&self, cutoff_ts_ms: i64) -> Result<usize, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        let pruned = conn
            .execute("DELETE FROM events WHERE ts_ms < ?1", params![cutoff_ts_ms])
            .map_err(|e| err("LEDGER_PRUNE", e.to_string()))?;
        Ok(pruned)
    }

    /// Delete a run and its events outright. Returns `(events_deleted,
    /// run_row_deleted)`. Used by the dashboard's prune action; there is no
    /// soft-delete — the caller confirms first. Deleting a run that does
    /// not exist is `Ok((0, 0))`, not an error.
    pub fn delete_run(&self, run_id: &str) -> Result<(usize, usize), PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        let events = conn
            .execute("DELETE FROM events WHERE run_id = ?1", params![run_id])
            .map_err(|e| err("LEDGER_DELETE", e.to_string()))?;
        let runs = conn
            .execute("DELETE FROM runs WHERE run_id = ?1", params![run_id])
            .map_err(|e| err("LEDGER_DELETE", e.to_string()))?;
        Ok((events, runs))
    }

    /// Retention-safe prune: like [`Self::prune_events_before`], but never
    /// deletes events of runs whose status is not terminal. The active run's
    /// transcript survives even when its oldest events predate the cutoff —
    /// a run that stays open for months must not lose the history a resume
    /// rebuilds from. Only finished runs (`completed`, `failed`,
    /// `canceled`) lose old history. Returns the number of event rows
    /// pruned.
    pub fn prune_events_before_active_safe(
        &self,
        cutoff_ts_ms: i64,
    ) -> Result<usize, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        let pruned = conn
            .execute(
                "DELETE FROM events WHERE ts_ms < ?1 \
                 AND run_id NOT IN (SELECT run_id FROM runs WHERE status NOT IN ('completed','failed','canceled'))",
                params![cutoff_ts_ms],
            )
            .map_err(|e| err("LEDGER_PRUNE", e.to_string()))?;
        Ok(pruned)
    }
    /// A run whose persisted status says it is still live.
    ///
    /// `running` and `awaiting_approval` are the two non-terminal states, and
    /// both are wrong once the process holding them is gone. A crash between
    /// "tool started" and "tool completed" leaves a `running` row forever,
    /// because only the terminal events clear it and they never arrive.
    pub fn stuck_runs(&self) -> Result<Vec<RunListing>, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT run_id, status, created_ms, title FROM runs \
                 WHERE status IN ('running','awaiting_approval') \
                 ORDER BY created_ms ASC",
            )
            .map_err(|e| err("LEDGER_QUERY", e.to_string()))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(|e| err("LEDGER_QUERY", e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            let row = row.map_err(|e| err("LEDGER_QUERY", e.to_string()))?;
            // A run in a non-terminal state is only *stuck* if nobody is
            // driving it. One holding a live lease is a session mid-turn, and
            // reporting it as a corpse would have repair settle live work.
            //
            // The lease test is inlined rather than calling
            // `has_active_lease` because this method already holds `conn`, and
            // a second `lock()` on the same std Mutex would deadlock.
            if lease_is_live(&conn, &row.0)? {
                continue;
            }
            out.push(row);
        }
        Ok(out)
    }

    /// Whether a run lease is held by a process that is actually alive.
    ///
    /// A lease row outlives a `kill -9` — nothing gets to release it and its
    /// TTL keeps counting — so testing `lease_until_ms > now` alone reports a
    /// crashed run as busy for a full TTL, precisely when the operator most
    /// needs `repair` to work. See [`lease_is_live`] for the full argument;
    /// this is the `&self` form of the same predicate.
    pub fn has_active_lease(&self, run_id: &str) -> Result<bool, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        lease_is_live(&conn, run_id)
    }

    /// SQLite's own integrity check, verbatim. Returns the rows it reports,
    /// which is `["ok"]` on a healthy database.
    ///
    /// This is the check nothing in the repo performed. A corrupted ledger
    /// previously surfaced as a confusing downstream read error with no way
    /// to tell "your data is damaged" from "this query has a bug".
    pub fn integrity_check(&self) -> Result<Vec<String>, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
        let mut stmt = conn
            .prepare("PRAGMA integrity_check")
            .map_err(|e| err("LEDGER_QUERY", e.to_string()))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| err("LEDGER_QUERY", e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| err("LEDGER_QUERY", e.to_string()))?);
        }
        Ok(out)
    }

    /// Force a stuck run to a terminal state, refusing while a live lease
    /// exists.
    ///
    /// This appends a real `RunFailed` rather than issuing an `UPDATE`, so the
    /// event trail explains itself: a reader replaying the ledger sees why the
    /// run ended instead of finding a status that contradicts its last event.
    /// A `repair` that rewrites history is a `repair` you cannot audit.
    pub fn settle_stuck_run(&self, run_id: &str, reason: &str) -> Result<(), PantheonError> {
        if self.has_active_lease(run_id)? {
            return Err(err(
                "REPAIR_LEASE_ACTIVE",
                format!("run {run_id} still holds a live lease; stop the session first"),
            ));
        }
        let status = self
            .status(run_id)?
            .ok_or_else(|| err("REPAIR_NO_RUN", format!("no run {run_id} in ledger")))?;
        if !matches!(status.as_str(), "running" | "awaiting_approval") {
            return Err(err(
                "REPAIR_NOT_STUCK",
                format!("run {run_id} is {status}, which is already terminal"),
            ));
        }
        // `RunFailed` carries only a code, so the reason goes out as a
        // progress event first. That is what a reader replaying the ledger
        // actually needs: the terminal event says the run was repaired, and
        // the event before it says what was wrong.
        self.append(&Event::RunProgress {
            run_id: run_id.into(),
            detail: format!("repair: {reason}"),
        })?;
        self.append(&Event::RunFailed {
            run_id: run_id.into(),
            code: "REPAIRED".into(),
        })
        .map(|_| ())
    }

    /// Per-run counters folded from the event log.
    ///
    /// This is where a metrics fold belongs: the ledger is the source of
    /// truth, so a counter that cannot disagree with it is one that does not
    /// need a second source of truth. It used to live in `pantheon-otel`,
    /// where nothing could call it.
    ///
    /// `RunFailed` counts separately from `RunCompleted` rather than being
    /// folded into a single "finished" number: a run that failed is exactly
    /// what an operator scanning these wants to see, and hiding it inside a
    /// success count is how failure rates get misread.
    pub fn metrics(&self, run_id: &str) -> Result<RunMetrics, PantheonError> {
        let mut m = RunMetrics::default();
        for e in self.replay(run_id)? {
            match e.event {
                Event::RunStarted { .. } => m.runs_started += 1,
                Event::RunCompleted { .. } => m.runs_completed += 1,
                Event::RunFailed { .. } => m.runs_failed += 1,
                Event::RunCanceled { .. } => m.runs_canceled += 1,
                Event::ToolStarted { .. } => m.tool_calls += 1,
                Event::ModelCompleted { .. } => m.model_turns += 1,
                Event::ApprovalRequested { .. } => m.approvals_requested += 1,
                Event::ApprovalGranted { .. } => m.approvals_granted += 1,
                Event::ApprovalDenied { .. } => m.approvals_denied += 1,
                Event::AgentSpawned { .. } => m.agents_spawned += 1,
                Event::ContextTrimmed { .. } => m.context_trims += 1,
                Event::ContextCompressed { .. } => m.context_compressions += 1,
                _ => {}
            }
        }
        Ok(m)
    }

    pub fn render_run_log(&self, run_id: &str) -> Result<String, PantheonError> {
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
        Event::AgentBound {
            agent_id, profile, ..
        } => {
            format!("bound to agent {agent_id} (profile {profile})")
        }
        Event::TurnStarted { turn_id, .. } => format!("turn started: {turn_id}"),
        Event::TurnParked {
            turn_id, reason, ..
        } => format!("turn parked: {turn_id} ({reason})"),
        Event::TurnCompleted {
            turn_id, outcome, ..
        } => format!("turn completed: {turn_id} ({outcome})"),
        Event::TurnFailed { turn_id, code, .. } => format!("turn failed: {turn_id} ({code})"),
        Event::TurnRewound { turn_id, .. } => format!("turn rewound: {turn_id}"),
        Event::UsageRecorded {
            model,
            total_tokens,
            cost_usd,
            ..
        } => match cost_usd {
            Some(c) => format!("usage: {model} {total_tokens} tokens ${c:.4}"),
            None => format!("usage: {model} {total_tokens} tokens"),
        },
        Event::CheckpointCreated { name, turn_id, .. } => {
            format!("checkpoint \"{name}\" at turn {turn_id}")
        }
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
        Event::UserInputRequested { question, .. } => format!("user input requested: {question}"),
        Event::UserInputProvided { answer, .. } => format!("user input provided: {answer}"),
        Event::ApprovalGranted { scope, .. } => format!("approval granted: {scope}"),
        Event::ApprovalDenied { scope, .. } => format!("approval denied: {scope}"),
        Event::DecisionRequested { point, .. } => {
            format!("decision requested: {:?}", point)
        }
        Event::DecisionMade { point, .. } => {
            format!("decision made: {:?}", point)
        }
        Event::DecisionRecorded { point, action, .. } => {
            format!("decision recorded: {:?} {:?}", point, action)
        }
        Event::ContextTrimmed {
            estimated,
            window,
            dropped_rows,
            compacted_rows,
            ..
        } => format!(
            "context trimmed: ~{estimated} tokens for a {window} window \
             ({dropped_rows} rows dropped, {compacted_rows} compacted)"
        ),
        Event::ContextCompressed {
            model,
            exchanges,
            chars_before,
            chars_after,
            ..
        } => format!(
            "context compressed by {model}: {exchanges} exchanges \
             ({chars_before} -> {chars_after} chars)"
        ),
        Event::SessionTitled {
            title,
            model,
            source,
            ..
        } => format!("session titled \"{title}\" ({source} by {model})"),
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
        Event::SteeringProvided { text, .. } => {
            format!("steered: {}", text.chars().take(120).collect::<String>())
        }
        Event::ImportedReasoning { turn_id, text, .. } => {
            format!(
                "imported reasoning ({}): {}",
                turn_id,
                text.chars().take(120).collect::<String>()
            )
        }
    }
}

#[cfg(test)]
#[path = "ledger_tests.rs"]
mod tests;

/// Whether `run_id` holds a lease that a live process is renewing.
///
/// Takes `&Connection` rather than `&Ledger` so it can be called from a method
/// that already holds the connection lock; going back through `&self` would
/// deadlock on the same non-re-entrant `Mutex`.
fn lease_is_live(conn: &rusqlite::Connection, run_id: &str) -> Result<bool, PantheonError> {
    let now = now_ms();
    // Fail CLOSED on storage errors: a missing row is `Ok(false)` (no live
    // lease), but a genuine DB error must propagate — treating it as
    // "lease dead" would let `settle_stuck_run` repair a possibly-live run.
    let row: Option<(i64, i64)> = conn
        .query_row(
            "SELECT lease_until_ms, heartbeat_ms FROM run_leases WHERE run_id = ?1",
            params![run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(|e| err("LEDGER_LEASE_CHECK", e.to_string()))?;
    let Some((until, beat)) = row else {
        return Ok(false);
    };
    if until <= now {
        return Ok(false);
    }
    // A heartbeat older than a fifth of the TTL, with a 2s floor, means the
    // holder is not renewing. Well inside the TTL, so a busy-but-alive run is
    // never mistaken for a corpse.
    let window = (until - beat).max(2_000) / 5;
    Ok(now - beat <= window)
}
