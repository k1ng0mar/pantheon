//! core_group: ledger, operations, claims, leases in one file.

pub mod ledger {
    //! Event-sourced execution ledger. Every run persists its events to SQLite;
    //! ``pantheon logs run_X`` replays them. During normal operation history is
    //! append-only (events are never updated in place); retention pruning
    //! ([`Ledger::prune_events_before_active_safe`]) and explicit run deletion
    //! ([`Ledger::delete_run`]) are the only paths that remove rows. Events carry
    //! no hash chain and no format version, so this is a local audit trail, not
    //! tamper-evident evidence against someone with write access to the file.
    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::events::Event;
    use pantheon_api::logging::now_ms;
    use pantheon_api::todo::TodoItem;
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
        pub run_seq: i64,
        pub version: i64,
        pub ts_ms: i64,
        pub event: Event,
    }

    /// Latest browser narration for one browser session: the read model behind
    /// `GET /api/browser/status`'s `last_activity`. Written by
    /// [`Ledger::append`] from `Event::BrowserActivity`; keyed by session so
    /// the dashboard can poll it cheaply without scanning the events table.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct BrowserActivityView {
        pub session: String,
        pub action: String,
        pub detail: String,
        pub ts_ms: i64,
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
    /// the marker replay normally. The `events` table is never rewritten - this
    /// is a read-time projection, so raw history survives for forensics while
    /// resume and transcript rebuilds see the rewound run as if those turns
    /// never happened.
    /// Apply `TurnRewound` markers as a read-time projection: truncating at
    /// the rewound turn's `TurnStarted` also drops any earlier `TurnRewound`
    /// markers inside the truncated range from the replayed stream. That is
    /// inherent to truncation (those markers described rewinds of turns that
    /// no longer exist in the projection); the raw table is intact, and a
    /// consumer needing full rewind history must read raw rows, not replay.
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
            | Event::UserMessage { run_id, .. }
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
            | Event::ToolInterrupted { run_id, .. }
            | Event::AgentSpawned { run_id, .. }
            | Event::AgentMessage { run_id, .. }
            | Event::AgentCompleted { run_id, .. }
            | Event::MemoryProposed { run_id }
            | Event::ApprovalRequested { run_id, .. }
            | Event::ApprovalGranted { run_id, .. }
            | Event::ApprovalDenied { run_id, .. }
            | Event::PreStateRecorded { run_id, .. }
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
            | Event::AgentMessageDelivered { run_id, .. }
            | Event::UserInputRequested { run_id, .. }
            | Event::UserInputProvided { run_id, .. }
            | Event::BrowserActivity { run_id, .. }
            | Event::ScheduledTaskFailed { run_id, .. }
            | Event::ScheduledTaskRecovered { run_id, .. }
            | Event::TodosUpdated { run_id, .. }
            | Event::UiContent { run_id, .. }
            | Event::UiUpdated { run_id, .. }
            | Event::UiActionRequested { run_id, .. }
            | Event::UiActionSubmitted { run_id, .. } => run_id,
        }
    }

    const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS runs (
      run_id TEXT PRIMARY KEY,
      created_ms INTEGER NOT NULL,
      status TEXT NOT NULL DEFAULT 'running',
      title TEXT,
      agent_id TEXT,
      project TEXT,
      cancel_intent INTEGER NOT NULL DEFAULT 0,
      pinned INTEGER NOT NULL DEFAULT 0,
      archived INTEGER NOT NULL DEFAULT 0
    );
    CREATE TABLE IF NOT EXISTS events (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      run_id TEXT NOT NULL,
      seq INTEGER NOT NULL,
      run_seq INTEGER NOT NULL DEFAULT 0,
      version INTEGER NOT NULL DEFAULT 1,
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
    );
    CREATE TABLE IF NOT EXISTS todos (
      run_id TEXT PRIMARY KEY,
      todos_json TEXT NOT NULL,
      updated_ms INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS browser_activity (
      session TEXT PRIMARY KEY,
      action TEXT NOT NULL,
      detail TEXT NOT NULL DEFAULT '',
      ts_ms INTEGER NOT NULL
    );";

    /// One row of [`Ledger::list_runs`]:
    /// `(run_id, status, created_ms, title, project, pinned, archived)`.
    /// `title` is `None` when the run has never been titled; `project` is
    /// `None` when the run was never assigned to a named project; `pinned`
    /// and `archived` are operator flags (see `set_run_pinned` /
    /// `set_run_archived`).
    pub type RunListing = (
        String,
        String,
        i64,
        Option<String>,
        Option<String>,
        bool,
        bool,
    );

    /// Well-known id of the permanent home session: the pinned, never-deleted
    /// session that is the default delivery target for scheduled jobs and the
    /// `--deliver mobile` target.
    pub const HOME_SESSION_ID: &str = "home";
    /// Display title written onto the home session's run row at creation.
    const HOME_SESSION_TITLE: &str = "Home";

    /// Forward-only column migrations for ledgers created before a column
    /// existed. "duplicate column name" is ignored so the migration stays
    /// idempotent (fresh databases already carry every column from SCHEMA);
    /// any other failure propagates as LEDGER_MIGRATE.
    fn migrate(conn: &Connection) -> Result<(), PantheonError> {
        let add = |sql: &str| {
            crate::add_column_once(conn, sql)
                .map_err(|e| err("LEDGER_MIGRATE", format!("migration failed ({sql}): {e}")))
        };
        add("ALTER TABLE runs ADD COLUMN title TEXT")?;
        // Run -> agent binding. A run belongs to exactly one agent profile for
        // its whole life, so this is written once when the run row is created
        // and never updated. NULL means "pre-binding run" (created before
        // profiles existed) and is read back as such rather than guessed.
        add("ALTER TABLE runs ADD COLUMN agent_id TEXT")?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_runs_agent ON runs(agent_id, created_ms DESC)",
            [],
        )
        .map_err(|e| {
            err(
                "LEDGER_MIGRATE",
                format!("migration failed (idx_runs_agent): {e}"),
            )
        })?;
        // FIFO queued follow-up messages per run (queue/steer), and the
        // run's agent mode ("plan"|"build"). The queue column holds a JSON
        // array of message strings; a pre-migration bare string is treated
        // as a one-element queue on read (see `parse_queue`). NULL = empty.
        add("ALTER TABLE runs ADD COLUMN queued_message TEXT")?;
        add("ALTER TABLE runs ADD COLUMN mode TEXT NOT NULL DEFAULT 'build'")?;
        // Named projects: user-created buckets that sessions are assigned
        // to by hand (`/project`). NULL = never assigned.
        add("ALTER TABLE runs ADD COLUMN project TEXT")?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_runs_project ON runs(project, created_ms DESC)",
            [],
        )
        .map_err(|e| {
            err(
                "LEDGER_MIGRATE",
                format!("migration failed (idx_runs_project): {e}"),
            )
        })?;
        // Cooperative cancel flag for worker-thread turns (AG-UI). The
        // in-process `AtomicBool` on `Session` cannot reach a turn running on
        // a different `Session` object (the AG-UI server builds a fresh
        // `Session` per RPC, and the dashboard kills from yet another
        // process), so cancel intent is recorded on the run row and the
        // drive loop polls it at every turn boundary. 0 = no intent,
        // 1 = wind down. `reopen_run` clears it so a continued run never
        // inherits a stale flag.
        add("ALTER TABLE runs ADD COLUMN cancel_intent INTEGER NOT NULL DEFAULT 0")?;
        // Operator session metadata: `pinned` keeps a run visually pinned
        // in the session picker; `archived` hides it from the run list
        // unless explicitly included. Direct writes like `project`: operator
        // metadata, not agent events.
        add("ALTER TABLE runs ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0")?;
        add("ALTER TABLE runs ADD COLUMN archived INTEGER NOT NULL DEFAULT 0")?;
        // Fork lineage: when a run was created by forking another run, the
        // source run id is recorded here. NULL for runs that were not forked.
        // Written once at fork creation; never updated (a fork-of-a-fork
        // points at its direct parent only, giving a clean chain, not a tree).
        add("ALTER TABLE runs ADD COLUMN forked_from TEXT")?;
        add("ALTER TABLE events ADD COLUMN run_seq INTEGER NOT NULL DEFAULT 0")?;
        add("ALTER TABLE events ADD COLUMN version INTEGER NOT NULL DEFAULT 1")?;
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

    /// One tool call that started and never completed, with the
    /// provenance a recovery pass can actually confirm from the ledger.
    /// `output_seen`/`result_seen` are about ledger rows, not about the
    /// outside world: a `ToolOutput` or `ToolMessage` row may exist while
    /// the tool's side effect is still unknown, so neither flag may be
    /// read as "the tool finished".
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct UnfinishedCall {
        pub call_id: String,
        pub tool: String,
        pub args: String,
        pub output_seen: bool,
        pub result_seen: bool,
    }

    impl Ledger {
        pub fn open(path: &Path) -> Result<Self, PantheonError> {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| err("LEDGER_MKDIR", e.to_string()))?;
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
            let conn =
                Connection::open_in_memory().map_err(|e| err("LEDGER_OPEN", e.to_string()))?;
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
            let json =
                serde_json::to_string(event).map_err(|e| err("LEDGER_SER", e.to_string()))?;
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
            // only once - a run that is already bound to a different agent is
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
            // Derived read model: latest browser narration per session. Powers
            // `GET /api/browser/status`'s `last_activity` without scanning the
            // events table (the app polls it every couple of seconds).
            if let Event::BrowserActivity {
                session,
                action,
                detail,
                ..
            } = event
            {
                conn.execute(
                    "INSERT INTO browser_activity (session, action, detail, ts_ms) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(session) DO UPDATE SET action = excluded.action, detail = excluded.detail, ts_ms = excluded.ts_ms",
                    params![session, action, detail, ts],
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
                "INSERT INTO events (run_id, seq, run_seq, version, ts_ms, event_json) VALUES (?1, (SELECT COALESCE(MAX(seq),0)+1 FROM events), (SELECT COALESCE(MAX(run_seq),0)+1 FROM events WHERE run_id=?1), 1, ?2, ?3)",
                params![run_id, ts, json],
            )
            .map_err(|e| err("LEDGER_APPEND", e.to_string()))?;
            let id: i64 = conn.last_insert_rowid();
            // The rowid and the global `seq` coincide only while no event row is
            // ever deleted; the stored seq is the authority, so read it back
            // rather than assuming they match.
            let (seq, run_seq, version): (i64, i64, i64) = conn
                .query_row(
                    "SELECT seq, run_seq, version FROM events WHERE id = ?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .map_err(|e| err("LEDGER_APPEND", e.to_string()))?;
            conn.commit()
                .map_err(|e| err("LEDGER_APPEND", e.to_string()))?;
            Ok(LedgerEntry {
                id,
                run_id,
                seq,
                run_seq,
                version,
                ts_ms: ts,
                event: event.clone(),
            })
        }

        /// Latest browser narration for one browser session, from the
        /// `browser_activity` read model (maintained by [`Ledger::append`]).
        /// `None` when the session has no recorded activity yet.
        pub fn browser_activity(
            &self,
            session: &str,
        ) -> Result<Option<BrowserActivityView>, PantheonError> {
            let raw_conn = self
                .conn
                .lock()
                .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
            let row: Option<(String, String, i64)> = raw_conn
                .query_row(
                    "SELECT action, detail, ts_ms FROM browser_activity WHERE session = ?1",
                    params![session],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .map_err(|e| err("LEDGER_READ", e.to_string()))?;
            Ok(row.map(|(action, detail, ts_ms)| BrowserActivityView {
                session: session.to_string(),
                action,
                detail,
                ts_ms,
            }))
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
                    "SELECT id, run_id, seq, run_seq, version, ts_ms, event_json FROM events WHERE run_id = ?1 ORDER BY run_seq, id",
                )
                .map_err(|e| err("LEDGER_REPLAY", e.to_string()))?;
            let rows = stmt
                .query_map(params![run_id], |row| {
                    let json: String = row.get(6)?;
                    let event: Event = serde_json::from_str(&json).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            6,
                            rusqlite::types::Type::Text,
                            e.into(),
                        )
                    })?;
                    Ok(LedgerEntry {
                        id: row.get(0)?,
                        run_id: row.get(1)?,
                        seq: row.get(2)?,
                        run_seq: row.get(3)?,
                        version: row.get(4)?,
                        ts_ms: row.get(5)?,
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
                    "SELECT id, run_id, seq, run_seq, version, ts_ms, event_json FROM events \
                     WHERE ts_ms >= ?1 AND ts_ms < ?2 AND event_json LIKE '%\"UsageRecorded\"%' \
                     ORDER BY ts_ms, id",
                )
                .map_err(|e| err("LEDGER_USAGE", e.to_string()))?;
            let rows = stmt
                .query_map(params![from_ms, to_ms], |row| {
                    let json: String = row.get(6)?;
                    let event: Event = serde_json::from_str(&json).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            6,
                            rusqlite::types::Type::Text,
                            e.into(),
                        )
                    })?;
                    Ok(LedgerEntry {
                        id: row.get(0)?,
                        run_id: row.get(1)?,
                        seq: row.get(2)?,
                        run_seq: row.get(3)?,
                        version: row.get(4)?,
                        ts_ms: row.get(5)?,
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

        /// Replace the run's todo snapshot (upsert). The event stream keeps
        /// every `TodosUpdated` change for the transcript; this table is the
        /// restart-safe *current* list, reloaded into the session on resume.
        pub fn set_todos(&self, run_id: &str, items: &[TodoItem]) -> Result<(), PantheonError> {
            let json =
                serde_json::to_string(items).map_err(|e| err("LEDGER_TODO", e.to_string()))?;
            let conn = self
                .conn
                .lock()
                .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
            let ts = now_ms();
            conn.execute(
                "INSERT INTO todos (run_id, todos_json, updated_ms) VALUES (?1, ?2, ?3)
                 ON CONFLICT(run_id) DO UPDATE SET todos_json=excluded.todos_json, updated_ms=excluded.updated_ms",
                params![run_id, json, ts],
            )
            .map_err(|e| err("LEDGER_TODO", e.to_string()))?;
            Ok(())
        }

        /// The run's persisted todo snapshot. Empty when the run never set
        /// one; a corrupt row is an error, not a silent empty list, so a
        /// broken snapshot can never masquerade as "no plan".
        pub fn todos(&self, run_id: &str) -> Result<Vec<TodoItem>, PantheonError> {
            let conn = self
                .conn
                .lock()
                .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
            let json: Option<String> = conn
                .query_row(
                    "SELECT todos_json FROM todos WHERE run_id=?1",
                    params![run_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| err("LEDGER_TODO", e.to_string()))?;
            match json {
                None => Ok(Vec::new()),
                Some(j) => serde_json::from_str(&j).map_err(|e| err("LEDGER_TODO", e.to_string())),
            }
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

        pub fn process_groups(
            &self,
            run_id: &str,
            lease_id: &str,
        ) -> Result<Vec<i32>, PantheonError> {
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
            self.list_runs_filtered(limit, false)
        }

        /// Like [`Ledger::list_runs`], but archived runs are excluded by the
        /// SQL query itself unless `include_archived` is set. Session-list
        /// surfaces use this: filtering archived rows in Rust after `LIMIT`
        /// starved the list whenever the newest window was archive-heavy (the
        /// dashboard fetched `limit * 4` rows and still underfilled past 75%
        /// archived, and the home row could fall outside the window entirely).
        pub fn list_runs_visible(
            &self,
            limit: usize,
            include_archived: bool,
        ) -> Result<Vec<RunListing>, PantheonError> {
            self.list_runs_filtered(limit, !include_archived)
        }

        /// Like [`Self::list_runs_visible`], but `status` and `query`
        /// (substring over run id or title, case-insensitive) filter in SQL
        /// before LIMIT. Filtering in Rust after LIMIT starved filtered
        /// pages: a page of the newest `limit` rows could contain zero
        /// matches even when older rows matched.
        pub fn list_runs_visible_filtered(
            &self,
            limit: usize,
            include_archived: bool,
            status: Option<&str>,
            query: Option<&str>,
        ) -> Result<Vec<RunListing>, PantheonError> {
            let conn = self
                .conn
                .lock()
                .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
            let mut sql = "SELECT run_id, status, created_ms, title, project, pinned, archived FROM runs WHERE (?2 = 0 OR archived = 0)".to_string();
            let mut params: Vec<rusqlite::types::Value> =
                vec![(limit as i64).into(), i64::from(!include_archived).into()];
            // Numbered placeholders must match the positional params slice
            // (rusqlite binds ?N by position), so number them dynamically:
            // a missing status filter must not shift the query's numbers.
            let mut next_param = 3;
            if let Some(s) = status.filter(|s| !s.is_empty()) {
                sql.push_str(&format!(" AND status = ?{next_param}"));
                next_param += 1;
                params.push(s.to_string().into());
            }
            if let Some(q) = query.filter(|q| !q.is_empty()) {
                // Escape LIKE wildcards so the query is a literal substring.
                let escaped = q
                    .replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_");
                let a = next_param;
                let b = next_param + 1;
                sql.push_str(&format!(
                    " AND (LOWER(run_id) LIKE LOWER(?{a}) ESCAPE '\\' OR LOWER(COALESCE(title, '')) LIKE LOWER(?{b}) ESCAPE '\\')"
                ));
                let pat = format!("%{escaped}%");
                params.push(pat.clone().into());
                params.push(pat.into());
            }
            sql.push_str(" ORDER BY created_ms DESC, rowid DESC LIMIT ?1");
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| err("LEDGER_QUERY", e.to_string()))?;
            let param_refs: Vec<&dyn rusqlite::types::ToSql> = params
                .iter()
                .map(|v| v as &dyn rusqlite::types::ToSql)
                .collect();
            let rows = stmt
                .query_map(&param_refs[..], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, i64>(5)? != 0,
                        r.get::<_, i64>(6)? != 0,
                    ))
                })
                .map_err(|e| err("LEDGER_QUERY", e.to_string()))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(|e| err("LEDGER_QUERY", e.to_string()))?);
            }
            Ok(out)
        }

        fn list_runs_filtered(
            &self,
            limit: usize,
            exclude_archived: bool,
        ) -> Result<Vec<RunListing>, PantheonError> {
            let conn = self
                .conn
                .lock()
                .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
            let mut stmt = conn
                .prepare(
                    "SELECT run_id, status, created_ms, title, project, pinned, archived FROM runs WHERE (?2 = 0 OR archived = 0) ORDER BY created_ms DESC, rowid DESC LIMIT ?1",
                )
                .map_err(|e| err("LEDGER_QUERY", e.to_string()))?;
            let rows = stmt
                .query_map(params![limit as i64, i64::from(exclude_archived)], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, i64>(5)? != 0,
                        r.get::<_, i64>(6)? != 0,
                    ))
                })
                .map_err(|e| err("LEDGER_QUERY", e.to_string()))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(|e| err("LEDGER_QUERY", e.to_string()))?);
            }
            Ok(out)
        }

        /// Auto-create the home session on first access. Idempotent: when the
        /// run row already exists this is a no-op. Every session-list and
        /// delivery surface calls this before reading, so the permanent
        /// session exists exactly when something reaches for it - never before.
        /// The row is created directly (single statement, so two racing first
        /// accesses cannot both win) and gets a `RunStarted` birth event, which
        /// keeps `replay("home")` non-empty for the run-detail endpoints.
        pub fn ensure_home_session(&self) -> Result<(), PantheonError> {
            let conn = self
                .conn
                .lock()
                .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
            let inserted = conn
                .execute(
                    "INSERT OR IGNORE INTO runs (run_id, created_ms, status, title) VALUES (?1, ?2, 'running', ?3)",
                    params![HOME_SESSION_ID, now_ms(), HOME_SESSION_TITLE],
                )
                .map_err(|e| err("LEDGER_HOME", e.to_string()))?;
            drop(conn);
            if inserted > 0 {
                self.append(&Event::RunStarted {
                    run_id: HOME_SESSION_ID.to_string(),
                })?;
            }
            Ok(())
        }

        /// Move the home session to the front of a run listing, keeping the
        /// relative order of everything else. Session-list surfaces call this
        /// after fetching their (recency-sorted) rows so the pinned session
        /// always leads, no matter how old its last activity is.
        pub fn pin_home_first(mut listings: Vec<RunListing>) -> Vec<RunListing> {
            if let Some(pos) = listings
                .iter()
                .position(|(id, _, _, _, _, _, _)| id == HOME_SESSION_ID)
            {
                let home = listings.remove(pos);
                listings.insert(0, home);
            }
            listings
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

        /// The run's named project, if it was ever assigned one (`/project`).
        /// `None` = unassigned, which the session picker treats as its own
        /// implicit group rather than guessing.
        pub fn run_project(&self, run_id: &str) -> Result<Option<String>, PantheonError> {
            let conn = self
                .conn
                .lock()
                .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
            conn.query_row(
                "SELECT project FROM runs WHERE run_id=?1",
                params![run_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(|e| err("LEDGER_PROJECT", e.to_string()))
            .map(|o| o.flatten())
        }

        /// Assign a run to a named project, or pass `None` to unassign it.
        /// Direct write like `agent_id`: project membership is operator
        /// metadata, not an agent event.
        pub fn set_run_project(
            &self,
            run_id: &str,
            project: Option<&str>,
        ) -> Result<(), PantheonError> {
            let conn = self
                .conn
                .lock()
                .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
            conn.execute(
                "UPDATE runs SET project = ?2 WHERE run_id = ?1",
                params![run_id, project],
            )
            .map_err(|e| err("LEDGER_PROJECT", e.to_string()))?;
            Ok(())
        }

        /// Every named project in use, most-recently-active first. Drives
        /// the `/project` list and the sessions picker's project filter.
        /// Archived runs do not keep a project listed: the exclusion lives in
        /// this query so every consumer (dashboard, TUI picker) agrees.
        pub fn list_projects(&self) -> Result<Vec<String>, PantheonError> {
            let conn = self
                .conn
                .lock()
                .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
            let mut stmt = conn
                .prepare(
                    "SELECT project, MAX(created_ms) FROM runs \
                     WHERE project IS NOT NULL AND project != '' AND archived = 0 \
                     GROUP BY project ORDER BY 2 DESC",
                )
                .map_err(|e| err("LEDGER_PROJECT", e.to_string()))?;
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(|e| err("LEDGER_PROJECT", e.to_string()))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(|e| err("LEDGER_PROJECT", e.to_string()))?);
            }
            Ok(out)
        }

        /// Parse the queue column: a JSON array of messages (current
        /// format), or a bare pre-migration string, which is treated as a
        /// one-element queue. NULL, empty, or malformed = empty queue.
        fn parse_queue(raw: Option<String>) -> Vec<String> {
            let text = raw.map(|t| t.trim().to_string()).unwrap_or_default();
            if text.is_empty() {
                return Vec::new();
            }
            if let Ok(arr) = serde_json::from_str::<Vec<String>>(&text) {
                return arr;
            }
            vec![text]
        }

        /// The run's queued follow-up messages, oldest first. `None` =
        /// nothing queued.
        pub fn queued_message(&self, run_id: &str) -> Result<Option<String>, PantheonError> {
            Ok(self.queued_messages(run_id)?.into_iter().next())
        }

        /// The run's queued follow-up messages as a FIFO list, oldest
        /// first. Empty = nothing queued. This is the accessor the
        /// dashboard exposes to clients.
        pub fn queued_messages(&self, run_id: &str) -> Result<Vec<String>, PantheonError> {
            let conn = self
                .conn
                .lock()
                .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
            let raw: Option<String> = conn
                .query_row(
                    "SELECT queued_message FROM runs WHERE run_id=?1",
                    params![run_id],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()
                .map_err(|e| err("LEDGER_STATUS", e.to_string()))?
                .flatten();
            Ok(Self::parse_queue(raw))
        }

        /// Read-modify-write helper for the queued-message list. The SELECT
        /// and the UPDATE run inside one IMMEDIATE transaction: the write
        /// lock is taken up front, so a second process or handle blocks here
        /// instead of interleaving its own SELECT between our read and our
        /// write - otherwise concurrent appends would silently lose all but
        /// the last writer's message. (The in-process `Mutex<Connection>`
        /// serializes threads of this process; the transaction serializes
        /// across processes.)
        ///
        /// The closure receives the current queue and returns the queue to
        /// persist (`Some`) or `None` to leave the row untouched - dropping
        /// the tx rolls back the no-op - plus a caller-chosen return value.
        fn with_queue<R>(
            &self,
            run_id: &str,
            f: impl FnOnce(Vec<String>) -> (Option<Vec<String>>, R),
        ) -> Result<R, PantheonError> {
            let mut conn = self
                .conn
                .lock()
                .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|e| err("LEDGER_STATUS", e.to_string()))?;
            let raw: Option<String> = tx
                .query_row(
                    "SELECT queued_message FROM runs WHERE run_id=?1",
                    params![run_id],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()
                .map_err(|e| err("LEDGER_STATUS", e.to_string()))?
                .flatten();
            let queue = Self::parse_queue(raw);
            let (new_queue, ret) = f(queue);
            if let Some(new_queue) = new_queue {
                let stored: Option<String> = if new_queue.is_empty() {
                    None
                } else {
                    Some(
                        serde_json::to_string(&new_queue)
                            .map_err(|e| err("LEDGER_SER", e.to_string()))?,
                    )
                };
                tx.execute(
                    "UPDATE runs SET queued_message=?2 WHERE run_id=?1",
                    params![run_id, stored],
                )
                .map_err(|e| err("LEDGER_STATUS", e.to_string()))?;
                tx.commit()
                    .map_err(|e| err("LEDGER_STATUS", e.to_string()))?;
            }
            // `None`: the tx is dropped without commit - a rolled-back no-op.
            Ok(ret)
        }

        /// Append to the run's queued-message list (`None` clears the
        /// whole queue, e.g. for steer/cancel). The read-modify-write is
        /// atomic across processes (see [`Self::with_queue`]): concurrent
        /// appends cannot lose each other's messages.
        pub fn set_queued_message(
            &self,
            run_id: &str,
            message: Option<&str>,
        ) -> Result<(), PantheonError> {
            self.with_queue(run_id, |mut queue| {
                match message {
                    Some(msg) => queue.push(msg.to_string()),
                    None => queue.clear(),
                }
                (Some(queue), ())
            })
        }

        /// Atomically pop the OLDEST queued message (FIFO): one drain, one
        /// consumer. Returns the popped message, if any; the rest of the
        /// queue stays queued for subsequent turns. Atomic across processes
        /// via [`Self::with_queue`]: each queued message is handed to
        /// exactly one consumer; none is lost or duplicated.
        pub fn take_queued_message(&self, run_id: &str) -> Result<Option<String>, PantheonError> {
            self.with_queue(run_id, |mut queue| {
                if queue.is_empty() {
                    // Nothing to do; the row stays untouched.
                    (None, None)
                } else {
                    let head = queue.remove(0);
                    (Some(queue), Some(head))
                }
            })
        }

        /// Remove the queued message at `index` (0 = oldest). Returns
        /// `false` when the index is out of range (also 404s in the API).
        /// Atomic across processes via [`Self::with_queue`].
        pub fn remove_queued_at(&self, run_id: &str, index: usize) -> Result<bool, PantheonError> {
            self.with_queue(run_id, |mut queue| {
                if index >= queue.len() {
                    (None, false)
                } else {
                    queue.remove(index);
                    (Some(queue), true)
                }
            })
        }

        /// Replace the queued message at `index` (0 = oldest) with new
        /// text. Returns `false` when the index is out of range. An empty
        /// `text` is a validation error (`QUEUE_EMPTY`), not a removal
        /// use [`Self::remove_queued_at`] to delete. Atomic across processes
        /// via [`Self::with_queue`].
        pub fn update_queued_at(
            &self,
            run_id: &str,
            index: usize,
            text: &str,
        ) -> Result<bool, PantheonError> {
            if text.trim().is_empty() {
                return Err(err(
                    "QUEUE_EMPTY",
                    "queued message text must not be empty".to_string(),
                ));
            }
            self.with_queue(run_id, |mut queue| {
                if index >= queue.len() {
                    (None, false)
                } else {
                    queue[index] = text.to_string();
                    (Some(queue), true)
                }
            })
        }

        /// The run's agent mode (`"plan"` or `"build"`). A missing row or an
        /// unexpected value reads as `"build"` - the mode is advisory, and a
        /// corrupt value must never break a turn.
        pub fn run_mode(&self, run_id: &str) -> Result<String, PantheonError> {
            let conn = self
                .conn
                .lock()
                .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
            let mode: Option<String> = conn
                .query_row(
                    "SELECT mode FROM runs WHERE run_id=?1",
                    params![run_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| err("LEDGER_STATUS", e.to_string()))?
                .flatten();
            Ok(match mode.as_deref() {
                Some("plan") => "plan".to_string(),
                _ => "build".to_string(),
            })
        }

        /// Persist the run's agent mode. Only `"plan"`/`"build"` are stored;
        /// anything else is a caller bug.
        pub fn set_run_mode(&self, run_id: &str, mode: &str) -> Result<(), PantheonError> {
            debug_assert!(mode == "plan" || mode == "build");
            let conn = self
                .conn
                .lock()
                .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
            conn.execute(
                "UPDATE runs SET mode=?2 WHERE run_id=?1",
                params![run_id, mode],
            )
            .map_err(|e| err("LEDGER_STATUS", e.to_string()))?;
            Ok(())
        }

        /// Whether the run is operator-pinned. A missing row or an unset
        /// flag reads as `false`.
        pub fn run_pinned(&self, run_id: &str) -> Result<bool, PantheonError> {
            let conn = self
                .conn
                .lock()
                .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
            conn.query_row(
                "SELECT pinned FROM runs WHERE run_id=?1",
                params![run_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map_err(|e| err("LEDGER_STATUS", e.to_string()))
            .map(|o| o.map(|v| v != 0).unwrap_or(false))
        }

        /// Whether the run is archived. A missing row or an unset flag reads
        /// as `false`.
        pub fn run_archived(&self, run_id: &str) -> Result<bool, PantheonError> {
            let conn = self
                .conn
                .lock()
                .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
            conn.query_row(
                "SELECT archived FROM runs WHERE run_id=?1",
                params![run_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map_err(|e| err("LEDGER_STATUS", e.to_string()))
            .map(|o| o.map(|v| v != 0).unwrap_or(false))
        }

        /// Set the operator-pinned flag on a run. Direct write like
        /// `set_run_mode`: operator metadata, not an agent event.
        pub fn set_run_pinned(&self, run_id: &str, pinned: bool) -> Result<(), PantheonError> {
            let conn = self
                .conn
                .lock()
                .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
            conn.execute(
                "UPDATE runs SET pinned=?2 WHERE run_id=?1",
                params![run_id, i64::from(pinned)],
            )
            .map_err(|e| err("LEDGER_STATUS", e.to_string()))?;
            Ok(())
        }

        /// Set the archived flag on a run: `true` hides it from the run list
        /// unless explicitly included, `false` restores it. Direct write
        /// like `set_run_mode`: operator metadata, not an agent event.
        pub fn set_run_archived(&self, run_id: &str, archived: bool) -> Result<(), PantheonError> {
            let conn = self
                .conn
                .lock()
                .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
            conn.execute(
                "UPDATE runs SET archived=?2 WHERE run_id=?1",
                params![run_id, i64::from(archived)],
            )
            .map_err(|e| err("LEDGER_STATUS", e.to_string()))?;
            Ok(())
        }

        /// Record fork lineage on a freshly-created run row. The forked-from
        /// pointer is written exactly once, when the fork is created, and
        /// never afterward: it is immutable provenance, not mutable metadata.
        /// A run that already has a parent is a hard error (a fork-of-a-fork
        /// should point at its direct parent, set at its own creation).
        pub fn set_run_forked_from(
            &self,
            run_id: &str,
            parent_run: &str,
        ) -> Result<(), PantheonError> {
            let conn = self
                .conn
                .lock()
                .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
            let existing: Option<String> = conn
                .query_row(
                    "SELECT forked_from FROM runs WHERE run_id=?1",
                    params![run_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| err("LEDGER_FORK_READ", e.to_string()))?
                .flatten();
            if existing.is_some() {
                return Err(err(
                    "LEDGER_FORK_REBIND",
                    format!(
                        "run {run_id} already has a fork parent ({existing:?}); refusing to rebind"
                    ),
                ));
            }
            conn.execute(
                "UPDATE runs SET forked_from=?2 WHERE run_id=?1",
                params![run_id, parent_run],
            )
            .map_err(|e| err("LEDGER_FORK_WRITE", e.to_string()))?;
            Ok(())
        }

        /// The direct parent run this run was forked from, or `None` when the
        /// run was not created by a fork. Following the chain repeatedly yields
        /// the full lineage to the root. A NULL column and a missing row both
        /// resolve to `None`; the fetcher is typed `Option<String>` so NULL
        /// never surfaces as an error.
        pub fn forked_from(&self, run_id: &str) -> Result<Option<String>, PantheonError> {
            let conn = self
                .conn
                .lock()
                .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
            conn.query_row(
                "SELECT forked_from FROM runs WHERE run_id=?1",
                params![run_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .map(|inner| inner.flatten())
            .map_err(|e| err("LEDGER_FORK_READ", e.to_string()))
        }

        /// Reopen a terminal run for continued conversation. Only terminal
        /// statuses flip back to running; a running/awaiting run is untouched
        /// (the caller then follows the normal path). Returns whether a
        /// reopen happened. The event trail keeps its original shape; the
        /// status flip is the continuation marker.
        ///
        /// Reopening also clears `cancel_intent`: the flag belongs to the turn
        /// that was canceled, and a continued run must never inherit a stale
        /// wind-down order.
        pub fn reopen_run(&self, run_id: &str) -> Result<bool, PantheonError> {
            let conn = self
                .conn
                .lock()
                .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
            let n = conn
                .execute(
                    "UPDATE runs SET status='running', cancel_intent=0 WHERE run_id=?1
                     AND status IN ('completed','failed','canceled')",
                    params![run_id],
                )
                .map_err(|e| err("LEDGER_UPDATE", e.to_string()))?;
            Ok(n > 0)
        }

        /// Record cooperative-cancel intent for a worker-thread turn.
        ///
        /// The in-process cancel token on `Session` is an `Arc<AtomicBool>` that
        /// only reaches turns running on that same `Session` object. AG-UI
        /// turns run on a fresh `Session` per RPC (see `agui.rs` `SendMsg`), and
        /// the dashboard issues kill from yet another process, so neither can
        /// flip the token. This flag is the cross-thread/process channel: set
        /// by `Supervisor::cancel_run_intent`, polled by the drive loop at
        /// every turn boundary, cleared by [`reopen_run`].
        pub fn set_cancel_intent(&self, run_id: &str, intent: bool) -> Result<(), PantheonError> {
            let conn = self
                .conn
                .lock()
                .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
            conn.execute(
                "UPDATE runs SET cancel_intent=?2 WHERE run_id=?1",
                params![run_id, intent as i64],
            )
            .map_err(|e| err("LEDGER_UPDATE", e.to_string()))?;
            Ok(())
        }

        /// Whether cooperative cancel has been requested for this run. Fail
        /// closed: a missing row reads as no intent (the run does not exist),
        /// a DB error propagates.
        pub fn cancel_intent(&self, run_id: &str) -> Result<bool, PantheonError> {
            let conn = self
                .conn
                .lock()
                .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
            let v: Option<i64> = conn
                .query_row(
                    "SELECT cancel_intent FROM runs WHERE run_id=?1",
                    params![run_id],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| err("LEDGER_QUERY", e.to_string()))?;
            Ok(v.unwrap_or(0) != 0)
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
        /// worker) calls. Nothing automatic runs on open - see
        /// [`configure_durability`]'s checkpoint note. Returns the number of
        /// event rows pruned.
        ///
        /// Derived `runs` rows are left alone: a pruned run keeps its status
        /// row but loses its event history, which is exactly what "keep titles
        /// and outcomes, drop transcripts" retention means. Pair with
        /// `SessionSearch::prune_before` so the FTS sidecar does not retain text
        /// for history the ledger has already pruned.
        /// Delete a run outright: its events, its `runs` row, its
        /// session-search chunks and FTS rows, its `todos` snapshot, and
        /// its `run_process_groups` rows, all in a single transaction. A
        /// deleted run must not leave searchable text behind - orphaned FTS
        /// rows would make `session_search` return hits for a run that no
        /// longer exists. Returns `(events_deleted, run_row_deleted)`. Used by
        /// the dashboard's prune action; there is no soft-delete - the caller
        /// confirms first. Deleting a run that does not exist is `Ok((0, 0))`,
        /// not an error. The home session can never be deleted: this is a
        /// hard error, not a silent no-op, so a caller that meant to delete
        /// something real learns it targeted the wrong id.
        ///
        /// Deliberately NOT deleted here, because no run link exists to
        /// delete by (deleting by guess would destroy other runs' data):
        /// - `claims`: global idempotency keys, not run-scoped;
        /// - `artifacts`: keyed by `task_id`, which carries no run id;
        /// - `browser_activity`: keyed by browser `session`, not run id;
        /// - `collaboration.db` tasks/messages: a separate file this store
        ///   does not own, keyed by collaboration/task, not run id.
        ///   Those leftovers are documented, not silently dropped on the
        ///   floor: a future run-keyed schema could extend this list.
        pub fn delete_run(&self, run_id: &str) -> Result<(usize, usize), PantheonError> {
            if run_id == HOME_SESSION_ID {
                return Err(err(
                    "LEDGER_HOME_PROTECTED",
                    "the home session can never be deleted".to_string(),
                ));
            }
            let mut conn = self
                .conn
                .lock()
                .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
            let tx = conn
                .transaction()
                .map_err(|e| err("LEDGER_DELETE", e.to_string()))?;
            let events = tx
                .execute("DELETE FROM events WHERE run_id = ?1", params![run_id])
                .map_err(|e| err("LEDGER_DELETE", e.to_string()))?;
            let runs = tx
                .execute("DELETE FROM runs WHERE run_id = ?1", params![run_id])
                .map_err(|e| err("LEDGER_DELETE", e.to_string()))?;
            // The FTS sidecar lives in the same DB file, but its tables are
            // created by SessionSearch::open, not by the ledger schema: a
            // ledger opened without the sidecar has no tables to clean, and
            // the delete must still succeed.
            let has_fts: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
                     AND name IN ('session_chunks', 'session_fts')",
                    [],
                    |r| r.get(0),
                )
                .map_err(|e| err("LEDGER_DELETE", e.to_string()))?;
            if has_fts == 2 {
                // FTS first, then chunks - mirrors prune_before: no dangling
                // references either way, and one transaction keeps it atomic.
                tx.execute("DELETE FROM session_fts WHERE run_id = ?1", params![run_id])
                    .map_err(|e| err("LEDGER_DELETE", e.to_string()))?;
                tx.execute(
                    "DELETE FROM session_chunks WHERE run_id = ?1",
                    params![run_id],
                )
                .map_err(|e| err("LEDGER_DELETE", e.to_string()))?;
            }
            // Run-scoped side tables in this same file: the todo snapshot
            // would otherwise resurrect the run's plan on resume-by-id, and
            // stale process-group rows would let a later lease query reap
            // (or signal) pgids that belong to a dead run.
            tx.execute("DELETE FROM todos WHERE run_id = ?1", params![run_id])
                .map_err(|e| err("LEDGER_DELETE", e.to_string()))?;
            tx.execute(
                "DELETE FROM run_process_groups WHERE run_id = ?1",
                params![run_id],
            )
            .map_err(|e| err("LEDGER_DELETE", e.to_string()))?;
            tx.commit()
                .map_err(|e| err("LEDGER_DELETE", e.to_string()))?;
            Ok((events, runs))
        }

        /// Retention-safe prune: like [`Self::prune_events_before_active_safe`], but never
        /// deletes events of runs whose status is not terminal. The active run's
        /// transcript survives even when its oldest events predate the cutoff
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
                    "SELECT run_id, status, created_ms, title, project, pinned, archived FROM runs \
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
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, i64>(5)? != 0,
                        r.get::<_, i64>(6)? != 0,
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
                // The home session is a permanent session, never a corpse: it
                // sits at `running` by design, so repair must never settle it.
                if row.0 == HOME_SESSION_ID {
                    continue;
                }
                out.push(row);
            }
            Ok(out)
        }

        /// Whether a run lease is held by a process that is actually alive.
        ///
        /// A lease row outlives a `kill -9` - nothing gets to release it and its
        /// TTL keeps counting - so testing `lease_until_ms > now` alone reports a
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

        /// Named alias of [`has_active_lease`] for the dashboard compress
        /// guard: compress must 409 while the run holds a live lease, and the
        /// dashboard leaf asked for this exact symbol. Same predicate, same
        /// fail-closed semantics (a lease row outliving a `kill -9` still
        /// reports busy until the heartbeat-freshness check fails).
        pub fn run_has_active_lease(&self, run_id: &str) -> Result<bool, PantheonError> {
            self.has_active_lease(run_id)
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

        /// Tool calls of one run that started and were never completed or
        /// interrupted, in first-started order. This is the ledger-level
        /// form of the runtime's `unfinished_calls` helper, with tool and
        /// args attached so a recovery record can name what was in flight.
        /// Rewind markers are already applied by [`Ledger::replay`], so a
        /// rewound call is not reported as unfinished.
        pub fn unfinished_calls(&self, run_id: &str) -> Result<Vec<UnfinishedCall>, PantheonError> {
            let entries = self.replay(run_id)?;
            let mut started: Vec<UnfinishedCall> = Vec::new();
            let mut index: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            let mut done: std::collections::HashSet<String> = std::collections::HashSet::new();
            let mut interrupted: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            let mut output_seen: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            let mut result_seen: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for e in &entries {
                match &e.event {
                    Event::ToolStarted {
                        call_id,
                        tool,
                        args,
                        ..
                    } => {
                        if !index.contains_key(call_id) {
                            index.insert(call_id.clone(), started.len());
                            started.push(UnfinishedCall {
                                call_id: call_id.clone(),
                                tool: tool.clone(),
                                args: args.clone(),
                                output_seen: false,
                                result_seen: false,
                            });
                        }
                    }
                    Event::ToolOutput { call_id, .. } => {
                        output_seen.insert(call_id.clone());
                    }
                    Event::ToolMessage { message, .. } => {
                        if let Some(call_id) = message.tool_call_id.as_deref() {
                            result_seen.insert(call_id.to_string());
                        }
                    }
                    Event::ToolCompleted { call_id, .. } => {
                        done.insert(call_id.clone());
                    }
                    Event::ToolInterrupted { call_id, .. } => {
                        interrupted.insert(call_id.clone());
                    }
                    _ => {}
                }
            }
            let mut out = Vec::new();
            for mut call in started {
                if done.contains(&call.call_id) || interrupted.contains(&call.call_id) {
                    continue;
                }
                call.output_seen = output_seen.contains(&call.call_id);
                call.result_seen = result_seen.contains(&call.call_id);
                out.push(call);
            }
            Ok(out)
        }

        /// Retire every unfinished tool call of one run as interrupted.
        /// Each retired call gets one [`Event::ToolInterrupted`] row carrying
        /// its tool, issued args, and what the ledger had already confirmed
        /// about it. Idempotent: a call that already has a `ToolInterrupted`
        /// (or a `ToolCompleted`) row is skipped, so a second pass returns
        /// an empty vec and writes nothing. This does not change the run's
        /// status — call it before a terminal event, as
        /// [`Ledger::settle_stuck_run`] does, if the run itself must end.
        /// It does not retire approval-parked calls (they never emitted
        /// `ToolStarted`) and it does not touch `ApprovalRequested` rows:
        /// operator decisions are not recovery residue.
        pub fn retire_unfinished_calls(&self, run_id: &str) -> Result<Vec<String>, PantheonError> {
            let mut retired = Vec::new();
            for call in self.unfinished_calls(run_id)? {
                self.append(&Event::ToolInterrupted {
                    run_id: run_id.to_string(),
                    call_id: call.call_id.clone(),
                    tool: call.tool.clone(),
                    args: call.args.clone(),
                    output_seen: call.output_seen,
                    result_seen: call.result_seen,
                    detail: "interrupted before completion; outcome unconfirmed — \
                             do not assume the tool's side effect completed or failed"
                        .to_string(),
                })?;
                retired.push(call.call_id);
            }
            Ok(retired)
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
            // Retire in-flight calls first, while the run is still
            // non-terminal: once the run is failed, its unfinished calls
            // would otherwise keep looking pending to every replay consumer
            // even though they can never complete. Idempotent, and a no-op
            // when there are none.
            self.retire_unfinished_calls(run_id)?;
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

        /// Settle every crash-orphaned run: status `running` with no live
        /// lease. This is the automatic half of `repair`, meant to run once at
        /// (gateway) startup so a server crash mid-turn stops showing the run
        /// as live in the dashboard.
        ///
        /// Only `running` runs are settled. `awaiting_approval` parks are
        /// operator decisions, not crash victims: their `ApprovalRequested`
        /// rows survive a restart and the operator can still grant or deny
        /// them, so auto-settling those would destroy a pending decision.
        /// The home session is already excluded by [`stuck_runs`].
        ///
        /// Each settle goes through [`settle_stuck_run`], which re-checks the
        /// lease fail-closed - a run whose driver came back between the scan
        /// and the settle is left alone - and retires that run's unfinished
        /// tool calls as [`Event::ToolInterrupted`] first, so the settled
        /// run leaves no pending-call residue behind. Returns the settled
        /// run ids, oldest first.
        pub fn settle_expired_runs(&self) -> Result<Vec<String>, PantheonError> {
            let mut settled = Vec::new();
            for (run_id, status, _, _, _, _, _) in self.stuck_runs()? {
                if status != "running" {
                    continue;
                }
                self.settle_stuck_run(
                    &run_id,
                    "startup recovery: lease expired with no live holder; the turn was interrupted",
                )?;
                settled.push(run_id);
            }
            Ok(settled)
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
            // An unknown run has no events at all; folding that into
            // all-zero metrics with exit 0 used to masquerade a typo'd id
            // as a run that did nothing. Fail loudly instead.
            let entries = self.replay(run_id)?;
            if entries.is_empty() {
                return Err(err("LEDGER_NO_RUN", format!("no such run {run_id:?}")));
            }
            let mut m = RunMetrics::default();
            for e in entries {
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

        /// Global fold over every run in the ledger. This is the metrics
        /// endpoint the health surface reads. It reuses the per run fold by
        /// replaying each run id once.
        pub fn global_metrics(&self) -> Result<RunMetrics, PantheonError> {
            let ids: Vec<String> = {
                let conn = self
                    .conn
                    .lock()
                    .map_err(|e| err("LEDGER_LOCK", e.to_string()))?;
                let mut stmt = conn
                    .prepare("SELECT DISTINCT run_id FROM events")
                    .map_err(|e| err("LEDGER_QUERY", e.to_string()))?;
                let rows = stmt
                    .query_map([], |r| r.get::<_, String>(0))
                    .map_err(|e| err("LEDGER_QUERY", e.to_string()))?;
                let mut out = Vec::new();
                for r in rows {
                    out.push(r.map_err(|e| err("LEDGER_QUERY", e.to_string()))?);
                }
                out
            };
            let mut total = RunMetrics::default();
            for id in ids {
                if let Ok(m) = self.metrics(&id) {
                    total.runs_started += m.runs_started;
                    total.runs_completed += m.runs_completed;
                    total.runs_failed += m.runs_failed;
                    total.runs_canceled += m.runs_canceled;
                    total.tool_calls += m.tool_calls;
                    total.model_turns += m.model_turns;
                    total.approvals_requested += m.approvals_requested;
                    total.approvals_granted += m.approvals_granted;
                    total.approvals_denied += m.approvals_denied;
                    total.agents_spawned += m.agents_spawned;
                    total.context_trims += m.context_trims;
                    total.context_compressions += m.context_compressions;
                }
            }
            Ok(total)
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
            Event::UserMessage { text, .. } => {
                format!("user: {}", text.chars().take(80).collect::<String>())
            }
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
            Event::ToolInterrupted {
                tool,
                call_id,
                output_seen,
                result_seen,
                ..
            } => format!(
                "tool interrupted: {tool} ({call_id}; outcome unconfirmed; output_seen={output_seen}, result_seen={result_seen})"
            ),
            Event::AgentSpawned { agent, .. } => format!("spawned sub-agent: {agent}"),
            Event::AgentMessage { agent, .. } => format!("sub-agent message: {agent}"),
            Event::AgentCompleted { agent, .. } => format!("sub-agent done: {agent}"),
            Event::MemoryProposed { .. } => String::from("memory write proposed"),
            Event::ApprovalRequested { scope, .. } => format!("approval requested: {scope}"),
            Event::UserInputRequested { question, .. } => format!("user input requested: {question}"),
            Event::UserInputProvided { answer, .. } => format!("user input provided: {answer}"),
            Event::TodosUpdated { items, .. } => {
                let done = items
                    .iter()
                    .filter(|i| i.status == pantheon_api::todo::TodoStatus::Completed)
                    .count();
                format!("todos updated: {done}/{} done", items.len())
            }
            Event::UiContent { ui_id, .. } => {
                format!("UI content published: {ui_id}")
            }
            Event::UiUpdated {
                ui_id, revision, ..
            } => {
                format!("UI content revised: {ui_id} (rev {revision})")
            }
            Event::UiActionRequested { ui_id, .. } => {
                format!("UI action requested: {ui_id}")
            }
            Event::UiActionSubmitted {
                ui_id, action_id, ..
            } => {
                format!("UI action submitted: {ui_id}#{action_id}")
            }
            Event::ApprovalGranted { scope, .. } => format!("approval granted: {scope}"),
            Event::ApprovalDenied { scope, .. } => format!("approval denied: {scope}"),
            Event::PreStateRecorded { path, sha256, .. } => {
                format!("pre-state recorded: {path} ({sha256})")
            }
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
            Event::AgentMessageDelivered {
                sender,
                kind,
                correlation_id,
                ..
            } => {
                let corr = correlation_id.as_deref().unwrap_or("none");
                format!("agent message delivered from {sender} ({kind}, correlation: {corr})")
            }
            Event::ImportedReasoning { turn_id, text, .. } => {
                format!(
                    "imported reasoning ({}): {}",
                    turn_id,
                    text.chars().take(120).collect::<String>()
                )
            }
            Event::BrowserActivity {
                session,
                action,
                detail,
                ..
            } => {
                if detail.is_empty() {
                    format!("browser [{session}]: {action}")
                } else {
                    format!("browser [{session}]: {action} ({detail})")
                }
            }
            Event::ScheduledTaskFailed { job_id, error, .. } => {
                format!("scheduled task {job_id} FAILED ({error})")
            }
            Event::ScheduledTaskRecovered { job_id, .. } => {
                format!("scheduled task {job_id} recovered after self-heal")
            }
        }
    }

    /// Whether `run_id` holds a lease that a live process is renewing.
    ///
    /// Takes `&Connection` rather than `&Ledger` so it can be called from a method
    /// that already holds the connection lock; going back through `&self` would
    /// deadlock on the same non-re-entrant `Mutex`.
    fn lease_is_live(conn: &rusqlite::Connection, run_id: &str) -> Result<bool, PantheonError> {
        let now = now_ms();
        // Fail CLOSED on storage errors: a missing row is `Ok(false)` (no live
        // lease), but a genuine DB error must propagate - treating it as
        // "lease dead" would let `settle_stuck_run` repair a possibly-live run.
        //
        // One exception: a missing `run_leases` table. The table is created by
        // `RunLeaseStore` before any lease can be acquired, so no table means
        // no lease ever existed on this database - provably "no live lease",
        // not corruption. Without this, `has_active_lease` / `stuck_runs` /
        // `settle_expired_runs` fail on any ledger whose supervisor never
        // opened the lease store, instead of reporting the true fact.
        let row: Option<(i64, i64)> = match conn.query_row(
            "SELECT lease_until_ms, heartbeat_ms FROM run_leases WHERE run_id = ?1",
            params![run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        ) {
            Err(e) if e.to_string().contains("no such table: run_leases") => None,
            other => other
                .optional()
                .map_err(|e| err("LEDGER_LEASE_CHECK", e.to_string()))?,
        };
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

    /// A fresh in-memory ledger for the test modules below.
    #[cfg(test)]
    fn mem() -> Ledger {
        Ledger::open_in_memory().expect("in-memory ledger")
    }

    #[cfg(test)]
    mod project_tests {
        use super::*;

        fn start(ledger: &Ledger, run_id: &str) {
            ledger
                .append(&Event::RunStarted {
                    run_id: run_id.to_string(),
                })
                .expect("append RunStarted");
        }

        #[test]
        fn project_assign_roundtrip() {
            let l = mem();
            start(&l, "r1");
            assert_eq!(l.run_project("r1").unwrap(), None);
            l.set_run_project("r1", Some("alpha")).unwrap();
            assert_eq!(l.run_project("r1").unwrap(), Some("alpha".to_string()));
            l.set_run_project("r1", None).unwrap();
            assert_eq!(l.run_project("r1").unwrap(), None);
        }

        #[test]
        fn list_runs_visible_survives_archive_heavy_window() {
            let l = mem();
            // Newest window of 12 runs: the 9 newest are archived (75%), so a
            // post-LIMIT Rust filter over a small window would return nothing
            // visible at all. SQL-side exclusion must still fill the page.
            for i in 0..12 {
                start(&l, &format!("r{i}"));
            }
            for i in 3..12 {
                l.set_run_archived(&format!("r{i}"), true).unwrap();
            }
            let visible = l.list_runs_visible(3, false).unwrap();
            let ids: Vec<&str> = visible.iter().map(|r| r.0.as_str()).collect();
            assert_eq!(ids, vec!["r2", "r1", "r0"]);
            assert!(
                visible.iter().all(|r| !r.6),
                "no archived rows leak through"
            );
            // include_archived brings the archived rows back, newest first.
            let all = l.list_runs_visible(3, true).unwrap();
            let ids: Vec<&str> = all.iter().map(|r| r.0.as_str()).collect();
            assert_eq!(ids, vec!["r11", "r10", "r9"]);
            // The unfiltered listing is unchanged by the new entry point.
            assert_eq!(l.list_runs(20).unwrap().len(), 12);
        }

        #[test]
        fn list_projects_ignores_archived_runs() {
            let l = mem();
            start(&l, "r1");
            l.set_run_project("r1", Some("alpha")).unwrap();
            l.set_run_archived("r1", true).unwrap();
            assert!(
                l.list_projects().unwrap().is_empty(),
                "a project whose only run is archived must not be listed"
            );
            l.set_run_archived("r1", false).unwrap();
            assert_eq!(l.list_projects().unwrap(), vec!["alpha".to_string()]);
        }

        #[test]
        fn project_unknown_run_is_none() {
            let l = mem();
            assert_eq!(l.run_project("nope").unwrap(), None);
        }

        #[test]
        fn list_projects_distinct_most_recent_first() {
            let l = mem();
            start(&l, "r1");
            start(&l, "r2");
            start(&l, "r3");
            l.set_run_project("r1", Some("alpha")).unwrap();
            l.set_run_project("r2", Some("beta")).unwrap();
            l.set_run_project("r3", Some("alpha")).unwrap();
            let ps = l.list_projects().unwrap();
            // r3 (alpha) is the most recently started run carrying a project.
            assert_eq!(ps, vec!["alpha".to_string(), "beta".to_string()]);
            // Unassigning the only beta run drops it from the list entirely.
            l.set_run_project("r2", None).unwrap();
            assert_eq!(l.list_projects().unwrap(), vec!["alpha".to_string()]);
        }

        #[test]
        fn list_runs_carries_project() {
            let l = mem();
            start(&l, "r1");
            l.set_run_project("r1", Some("alpha")).unwrap();
            let runs = l.list_runs(10).unwrap();
            assert_eq!(runs.len(), 1);
            let (id, _status, _ts, _title, project, pinned, archived) = &runs[0];
            assert_eq!(id, "r1");
            assert_eq!(project.as_deref(), Some("alpha"));
            assert!(!pinned, "new runs are not pinned");
            assert!(!archived, "new runs are not archived");
        }

        #[test]
        fn pinned_archived_roundtrip() {
            let l = mem();
            start(&l, "r1");
            assert!(!l.run_pinned("r1").unwrap());
            assert!(!l.run_archived("r1").unwrap());
            l.set_run_pinned("r1", true).unwrap();
            l.set_run_archived("r1", true).unwrap();
            assert!(l.run_pinned("r1").unwrap());
            assert!(l.run_archived("r1").unwrap());
            let (id, _status, _ts, _title, _project, pinned, archived) =
                &l.list_runs(10).unwrap()[0];
            assert_eq!(id, "r1");
            assert!(*pinned);
            assert!(*archived);
            l.set_run_pinned("r1", false).unwrap();
            l.set_run_archived("r1", false).unwrap();
            assert!(!l.run_pinned("r1").unwrap());
            assert!(!l.run_archived("r1").unwrap());
        }

        #[test]
        fn pinned_archived_unknown_run_is_false() {
            let l = mem();
            assert!(!l.run_pinned("nope").unwrap());
            assert!(!l.run_archived("nope").unwrap());
        }

        #[test]
        fn list_runs_visible_filtered_filters_before_limit() {
            let l = mem();
            // Newest first: the three newest do not match, the two older do.
            // A post-LIMIT Rust filter over a window of 2 would return
            // nothing; SQL-side filtering must return both matches.
            start(&l, "alpha-1");
            start(&l, "alpha-2");
            start(&l, "beta-3");
            start(&l, "beta-4");
            start(&l, "beta-5");
            let matched = l
                .list_runs_visible_filtered(2, false, None, Some("alpha"))
                .unwrap();
            let ids: Vec<&str> = matched.iter().map(|r| r.0.as_str()).collect();
            assert_eq!(ids, vec!["alpha-2", "alpha-1"]);
            // Case-insensitive substring over run id.
            let matched = l
                .list_runs_visible_filtered(10, false, None, Some("ALPHA"))
                .unwrap();
            assert_eq!(matched.len(), 2);
            // Status filter: started runs are 'running'.
            let running = l
                .list_runs_visible_filtered(10, false, Some("running"), None)
                .unwrap();
            assert_eq!(running.len(), 5);
            let done = l
                .list_runs_visible_filtered(10, false, Some("completed"), None)
                .unwrap();
            assert!(done.is_empty());
            // LIKE wildcards in the query are literal, not patterns.
            start(&l, "a%b");
            start(&l, "axb");
            let literal = l
                .list_runs_visible_filtered(10, false, None, Some("a%b"))
                .unwrap();
            let ids: Vec<&str> = literal.iter().map(|r| r.0.as_str()).collect();
            assert_eq!(ids, vec!["a%b"]);
        }
    }

    #[cfg(test)]
    mod delete_run_tests {
        use super::*;
        use crate::search::{SessionChunk, SessionSearch};

        fn stores() -> (Ledger, SessionSearch, tempfile::TempDir) {
            let dir = tempfile::TempDir::new().expect("temp dir");
            let db = dir.path().join("ledger.db");
            let ledger = Ledger::open(&db).expect("ledger");
            // Same file, like Supervisor::open: the FTS sidecar lives in the
            // ledger's own DB.
            let search = SessionSearch::open(&db).expect("search");
            (ledger, search, dir)
        }

        fn chunk(run_id: &str, seq: i64, text: &str) -> SessionChunk {
            SessionChunk {
                chunk_id: format!("{run_id}:{seq}:title"),
                run_id: run_id.to_string(),
                seq,
                kind: "title".to_string(),
                text: text.to_string(),
                ts_ms: 0,
            }
        }

        /// P1 regression: deleting a run must also remove its FTS rows. The
        /// old `delete_run` left `session_chunks`/`session_fts` rows behind,
        /// so `session_search` kept returning hits for a run that no longer
        /// existed. Other runs' rows must be untouched.
        #[test]
        fn delete_run_removes_fts_rows_but_keeps_other_runs() {
            let (ledger, search, _dir) = stores();
            ledger
                .append(&Event::RunStarted {
                    run_id: "run-a".into(),
                })
                .unwrap();
            ledger
                .append(&Event::RunStarted {
                    run_id: "run-b".into(),
                })
                .unwrap();
            search
                .index(&chunk("run-a", 1, "alpha zzzq unique text"))
                .unwrap();
            search
                .index(&chunk("run-b", 1, "beta zzzq other text"))
                .unwrap();
            assert_eq!(search.search("zzzq", 10).unwrap().len(), 2);

            let (events, _rows) = ledger.delete_run("run-a").unwrap();
            assert_eq!(events, 1);

            let hits = search.search("zzzq", 10).unwrap();
            assert_eq!(hits.len(), 1, "deleted run's FTS rows must be gone");
            assert_eq!(hits[0].chunk.run_id, "run-b");
            assert!(ledger.replay("run-a").unwrap().is_empty());
        }

        /// `delete_run` also removes the run-scoped side tables (`todos`,
        /// `run_process_groups`) in the same transaction, while the
        /// documented leftovers (claims, artifacts - neither carries a run
        /// id to delete by) survive, as `delete_run`'s doc promises.
        #[test]
        fn delete_run_removes_todos_and_process_groups_but_keeps_unlinked_rows() {
            let (ledger, _search, _dir) = stores();
            ledger
                .append(&Event::RunStarted {
                    run_id: "run-a".into(),
                })
                .unwrap();
            ledger
                .set_todos(
                    "run-a",
                    &[pantheon_api::todo::TodoItem {
                        content: "do the thing".into(),
                        status: pantheon_api::todo::TodoStatus::Pending,
                        active_form: None,
                    }],
                )
                .unwrap();
            ledger
                .register_process_group("run-a", 4242, "lease-1")
                .unwrap();
            ledger
                .put_artifact("task-1", "text/plain", b"blob")
                .unwrap();
            assert!(ledger.claim("claim-1").unwrap());

            let (events, _rows) = ledger.delete_run("run-a").unwrap();
            assert_eq!(events, 1);
            assert!(
                ledger.todos("run-a").unwrap().is_empty(),
                "todo snapshot must go"
            );
            assert!(
                ledger
                    .process_groups("run-a", "lease-1")
                    .unwrap()
                    .is_empty(),
                "process-group rows must go"
            );
            // Documented leftovers: no run link exists to delete these by.
            assert!(
                ledger.artifact("task-1").unwrap().is_some(),
                "artifacts are task-keyed, not run-keyed"
            );
            assert!(
                !ledger.claim("claim-1").unwrap(),
                "claims are global, not run-scoped"
            );
        }

        /// A ledger opened without the search sidecar has no FTS tables; the
        /// FTS cleanup must be skipped, not fail the delete.
        #[test]
        fn delete_run_without_search_tables_succeeds() {
            let dir = tempfile::TempDir::new().expect("temp dir");
            let ledger = Ledger::open(&dir.path().join("ledger.db")).expect("ledger");
            ledger
                .append(&Event::RunStarted {
                    run_id: "r1".into(),
                })
                .unwrap();
            let (events, _) = ledger.delete_run("r1").unwrap();
            assert_eq!(events, 1);
        }
    }

    #[cfg(test)]
    mod queue_atomicity_tests {
        use super::*;
        use std::sync::{Arc, Mutex};

        /// Item 5: a migration that reports `Ok` really migrated. Against a
        /// connection with no `runs` table the ALTER fails with "no such
        /// table" - that must propagate as LEDGER_MIGRATE, not be swallowed
        /// by `let _ =`.
        #[test]
        fn migrate_propagates_real_errors() {
            let conn = Connection::open_in_memory().expect("in-memory conn");
            let err = migrate(&conn).expect_err("missing runs table must fail loudly");
            assert!(
                err.code.starts_with("LEDGER_MIGRATE"),
                "unexpected error: {err:?}"
            );
        }

        /// Item 5: idempotency is preserved - on a fresh SCHEMA database
        /// every ALTER hits "duplicate column name", which stays ignored.
        #[test]
        fn migrate_stays_idempotent_on_fresh_schema() {
            let conn = Connection::open_in_memory().expect("in-memory conn");
            conn.execute_batch(SCHEMA).expect("fresh schema");
            migrate(&conn).expect("first migrate on fresh schema");
            migrate(&conn).expect("second migrate still ok");
        }

        /// A ledger created before the pinned/archived columns existed
        /// gains them on open; existing rows default to
        /// unpinned/unarchived and the new flags round-trip.
        #[test]
        fn migrate_adds_pinned_archived_to_legacy_schema() {
            let dir = std::env::temp_dir().join(format!(
                "pantheon-ledger-legacy-test-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let db = dir.join("legacy.db");
            let _ = std::fs::remove_file(&db);
            {
                let conn = Connection::open(&db).expect("create legacy db");
                conn.execute_batch(
                    "CREATE TABLE runs (
                       run_id TEXT PRIMARY KEY,
                       created_ms INTEGER NOT NULL,
                       status TEXT NOT NULL DEFAULT 'running'
                     );
                     INSERT INTO runs (run_id, created_ms, status)
                     VALUES ('legacy-1', 1000, 'completed');",
                )
                .expect("seed legacy schema");
            }
            let ledger = Ledger::open(&db).expect("open migrated ledger");
            assert!(
                !ledger.run_pinned("legacy-1").unwrap(),
                "legacy rows default unpinned"
            );
            assert!(
                !ledger.run_archived("legacy-1").unwrap(),
                "legacy rows default unarchived"
            );
            ledger.set_run_pinned("legacy-1", true).unwrap();
            ledger.set_run_archived("legacy-1", true).unwrap();
            assert!(ledger.run_pinned("legacy-1").unwrap());
            assert!(ledger.run_archived("legacy-1").unwrap());
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// Item 5: two handles on the same DB file, N queued messages,
        /// concurrent takes from threads on both handles - each message is
        /// delivered exactly once, none lost, none duplicated. The
        /// IMMEDIATE transaction holds the write lock across the
        /// read-modify-write so a second handle cannot interleave.
        #[test]
        fn concurrent_takes_deliver_each_message_exactly_once() {
            const N: usize = 40;
            let dir = std::env::temp_dir()
                .join(format!("pantheon-ledger-queue-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let db = dir.join("queue.db");
            let _ = std::fs::remove_file(&db);
            let seed = Ledger::open(&db).expect("open ledger");
            seed.append(&Event::RunStarted {
                run_id: "rq".to_string(),
            })
            .expect("RunStarted");
            for i in 0..N {
                seed.set_queued_message("rq", Some(&format!("msg-{i}")))
                    .expect("queue message");
            }
            drop(seed);

            let a = Arc::new(Ledger::open(&db).expect("handle a"));
            let b = Arc::new(Ledger::open(&db).expect("handle b"));
            let taken: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let mut threads = Vec::new();
            for t in 0..8 {
                let (ledger, taken) = if t % 2 == 0 {
                    (Arc::clone(&a), Arc::clone(&taken))
                } else {
                    (Arc::clone(&b), Arc::clone(&taken))
                };
                threads.push(std::thread::spawn(move || {
                    while let Some(msg) = ledger.take_queued_message("rq").expect("take works") {
                        taken.lock().unwrap().push(msg);
                    }
                }));
            }
            for th in threads {
                th.join().expect("worker thread");
            }
            // Both handles agree the queue is drained.
            assert_eq!(a.take_queued_message("rq").unwrap(), None);
            assert_eq!(b.take_queued_message("rq").unwrap(), None);

            let mut got = taken.lock().unwrap().clone();
            got.sort();
            let mut want: Vec<String> = (0..N).map(|i| format!("msg-{i}")).collect();
            want.sort();
            assert_eq!(got, want, "each message delivered exactly once");
            std::fs::remove_dir_all(&dir).ok();
        }

        /// Item 2: two handles on the same DB file, 8 threads hammering
        /// append - every message survives. The read-modify-write in
        /// `set_queued_message` runs inside an IMMEDIATE transaction (via
        /// `with_queue`), so a second handle cannot interleave its SELECT
        /// between our read and write and silently drop the first append.
        #[test]
        fn concurrent_appends_lose_nothing() {
            const N: usize = 40;
            const T: usize = 8;
            let dir = std::env::temp_dir().join(format!(
                "pantheon-ledger-append-test-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let db = dir.join("append.db");
            let _ = std::fs::remove_file(&db);
            let seed = Ledger::open(&db).expect("open ledger");
            seed.append(&Event::RunStarted {
                run_id: "ra".to_string(),
            })
            .expect("RunStarted");
            drop(seed);

            let a = Arc::new(Ledger::open(&db).expect("handle a"));
            let b = Arc::new(Ledger::open(&db).expect("handle b"));
            let mut threads = Vec::new();
            for t in 0..T {
                let ledger = if t % 2 == 0 {
                    Arc::clone(&a)
                } else {
                    Arc::clone(&b)
                };
                threads.push(std::thread::spawn(move || {
                    for i in 0..N {
                        ledger
                            .set_queued_message("ra", Some(&format!("t{t}-msg-{i}")))
                            .expect("append works");
                    }
                }));
            }
            for th in threads {
                th.join().expect("worker thread");
            }

            // Drain from one handle: all T*N messages must be present.
            let mut got = Vec::new();
            while let Some(msg) = a.take_queued_message("ra").expect("take works") {
                got.push(msg);
            }
            let mut want: Vec<String> = (0..T)
                .flat_map(|t| (0..N).map(move |i| format!("t{t}-msg-{i}")))
                .collect();
            got.sort();
            want.sort();
            assert_eq!(got, want, "no append lost under concurrency");
            std::fs::remove_dir_all(&dir).ok();
        }
    }

    #[cfg(test)]
    mod cancel_and_recovery_tests {
        use super::*;
        use crate::leases::RunLeaseStore;

        fn tmp_db(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
            let dir =
                std::env::temp_dir().join(format!("pantheon-ledger-{name}-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let db = dir.join("ledger.db");
            let _ = std::fs::remove_file(&db);
            (dir, db)
        }

        fn start(ledger: &Ledger, run_id: &str) {
            ledger
                .append(&Event::RunStarted {
                    run_id: run_id.to_string(),
                })
                .expect("append RunStarted");
        }

        /// #7: cancel_intent round-trips, defaults to false, and is cleared by
        /// reopen_run so a continued run never inherits a stale wind-down order.
        #[test]
        fn cancel_intent_roundtrip_and_reopen_clears() {
            let l = Ledger::open_in_memory().expect("in-memory ledger");
            start(&l, "r1");
            assert!(!l.cancel_intent("r1").expect("read flag"));
            // Missing row reads as no intent (fail closed on the read path).
            assert!(!l.cancel_intent("nope").expect("missing row"));

            l.set_cancel_intent("r1", true).expect("set flag");
            assert!(l.cancel_intent("r1").expect("read flag"));

            // Terminal -> reopen clears the flag along with the status flip.
            l.append(&Event::RunCanceled {
                run_id: "r1".into(),
                reason: "test".into(),
            })
            .expect("cancel");
            assert_eq!(l.status("r1").unwrap().as_deref(), Some("canceled"));
            assert!(l.reopen_run("r1").expect("reopen"));
            assert_eq!(l.status("r1").unwrap().as_deref(), Some("running"));
            assert!(!l.cancel_intent("r1").expect("flag cleared on reopen"));
        }

        /// #10: run_has_active_lease is the same predicate as has_active_lease.
        #[test]
        fn run_has_active_lease_matches_has_active_lease() {
            let (_dir, db) = tmp_db("lease-alias");
            let l = Ledger::open(&db).expect("open ledger");
            start(&l, "r1");
            start(&l, "r2");
            assert!(!l.run_has_active_lease("r1").unwrap());
            assert_eq!(
                l.run_has_active_lease("r1").unwrap(),
                l.has_active_lease("r1").unwrap()
            );
            let leases = RunLeaseStore::open(&db).expect("open lease store");
            leases
                .acquire("r2", "lease-1", 30_000)
                .expect("acquire lease");
            assert!(l.run_has_active_lease("r2").unwrap());
            assert_eq!(
                l.run_has_active_lease("r2").unwrap(),
                l.has_active_lease("r2").unwrap()
            );
            std::fs::remove_dir_all(&_dir).ok();
        }

        /// #8: settle_expired_runs settles crash-orphaned `running` runs (no
        /// live lease), leaves live-lease runs alone, and never touches
        /// `awaiting_approval` parks (those are operator decisions, still
        /// grantable after a restart).
        #[test]
        fn settle_expired_runs_only_settles_running_without_live_lease() {
            let (_dir, db) = tmp_db("settle-expired");
            let l = Ledger::open(&db).expect("open ledger");
            // Crash orphan: running, no lease.
            start(&l, "orphan");
            // Live run: running, live lease.
            start(&l, "live");
            // Parked run: awaiting approval, no lease (lease dropped at park).
            start(&l, "parked");
            l.append(&Event::ApprovalRequested {
                run_id: "parked".into(),
                scope: "shell:rm -rf /".into(),
            })
            .expect("park");
            assert_eq!(
                l.status("parked").unwrap().as_deref(),
                Some("awaiting_approval")
            );
            // Already terminal: must not be touched.
            start(&l, "done");
            l.append(&Event::RunCompleted {
                run_id: "done".into(),
            })
            .expect("complete");

            let leases = RunLeaseStore::open(&db).expect("open lease store");
            leases.acquire("live", "lease-1", 30_000).expect("acquire");

            let settled = l.settle_expired_runs().expect("settle");
            assert_eq!(settled, vec!["orphan".to_string()]);

            assert_eq!(l.status("orphan").unwrap().as_deref(), Some("failed"));
            assert_eq!(l.status("live").unwrap().as_deref(), Some("running"));
            assert_eq!(
                l.status("parked").unwrap().as_deref(),
                Some("awaiting_approval")
            );
            assert_eq!(l.status("done").unwrap().as_deref(), Some("completed"));

            // The event trail explains itself: a progress note, then the
            // terminal REPAIRED failure.
            let events: Vec<String> = l
                .replay("orphan")
                .expect("replay")
                .iter()
                .map(|e| format!("{:?}", e.event))
                .collect();
            assert!(
                events.iter().any(|e| e.contains("RunProgress")),
                "expected a repair note, got: {events:?}"
            );
            assert!(
                events.iter().any(|e| e.contains("REPAIRED")),
                "expected REPAIRED, got: {events:?}"
            );
            std::fs::remove_dir_all(&_dir).ok();
        }

        /// #8: nothing to settle -> empty vec, no writes.
        #[test]
        fn settle_expired_runs_empty_when_nothing_stuck() {
            let l = Ledger::open_in_memory().expect("in-memory ledger");
            assert_eq!(l.settle_expired_runs().unwrap(), Vec::<String>::new());
        }

        fn started_call(ledger: &Ledger, run_id: &str, call_id: &str, tool: &str, args: &str) {
            ledger
                .append(&Event::ToolStarted {
                    run_id: run_id.to_string(),
                    call_id: call_id.to_string(),
                    tool: tool.to_string(),
                    args: args.to_string(),
                    provenance: pantheon_api::provenance::Provenance::system("test"),
                })
                .expect("append ToolStarted");
        }

        /// Recovery provenance: an interrupted call is recorded with its
        /// identity, tool, issued args, and what the ledger had already
        /// confirmed about it — never as a bare "run interrupted".
        #[test]
        fn retire_unfinished_calls_records_provenance() {
            let l = Ledger::open_in_memory().expect("in-memory ledger");
            start(&l, "r1");
            started_call(&l, "r1", "call-1", "shell", "{\"command\":\"ls\"}");
            started_call(&l, "r1", "call-2", "read", "{\"path\":\"a.txt\"}");
            // call-1 emitted output but never completed; call-2 emitted
            // nothing at all; call-3 completed and must never be retired.
            l.append(&Event::ToolOutput {
                run_id: "r1".into(),
                call_id: "call-1".into(),
                tool: "shell".into(),
                truncated: false,
                provenance: pantheon_api::provenance::Provenance::system("test"),
            })
            .expect("output");
            started_call(&l, "r1", "call-3", "read", "{\"path\":\"b.txt\"}");
            l.append(&Event::ToolCompleted {
                run_id: "r1".into(),
                call_id: "call-3".into(),
                tool: "read".into(),
                provenance: pantheon_api::provenance::Provenance::system("test"),
            })
            .expect("complete");

            let unfinished = l.unfinished_calls("r1").expect("unfinished");
            assert_eq!(
                unfinished
                    .iter()
                    .map(|c| c.call_id.as_str())
                    .collect::<Vec<_>>(),
                vec!["call-1", "call-2"],
                "completed calls are not unfinished"
            );
            let call_1 = &unfinished[0];
            assert_eq!(call_1.tool, "shell");
            assert_eq!(call_1.args, "{\"command\":\"ls\"}");
            assert!(call_1.output_seen, "ToolOutput row must be recorded");
            assert!(!call_1.result_seen, "no ToolMessage row exists for call-1");
            assert!(!unfinished[1].output_seen);

            let retired = l.retire_unfinished_calls("r1").expect("retire");
            assert_eq!(retired, vec!["call-1".to_string(), "call-2".to_string()]);
            assert!(
                l.unfinished_calls("r1").expect("re-scan").is_empty(),
                "retired calls are settled, not pending"
            );

            let interrupted: Vec<_> = l
                .replay("r1")
                .expect("replay")
                .into_iter()
                .filter_map(|e| match e.event {
                    Event::ToolInterrupted {
                        call_id,
                        tool,
                        args,
                        output_seen,
                        result_seen,
                        detail,
                        ..
                    } => Some((call_id, tool, args, output_seen, result_seen, detail)),
                    _ => None,
                })
                .collect();
            assert_eq!(interrupted.len(), 2, "one row per retired call");
            assert_eq!(interrupted[0].1, "shell");
            assert_eq!(interrupted[0].2, "{\"command\":\"ls\"}");
            assert!(interrupted[0].3);
            assert!(!interrupted[0].4);
            assert!(
                interrupted[0].5.contains("unconfirmed"),
                "interrupted detail must not claim an outcome: {}",
                interrupted[0].5
            );
            assert!(
                !interrupted.iter().any(|(c, ..)| c == "call-3"),
                "completed call must never get an interrupted row"
            );
        }

        /// Recovery is idempotent: a second pass settles nothing new and
        /// writes no duplicate interrupted rows.
        #[test]
        fn retire_unfinished_calls_is_idempotent() {
            let l = Ledger::open_in_memory().expect("in-memory ledger");
            start(&l, "r1");
            started_call(&l, "r1", "call-1", "shell", "{\"command\":\"ls\"}");
            assert_eq!(
                l.retire_unfinished_calls("r1").expect("first retire"),
                vec!["call-1".to_string()]
            );
            let before = l.replay("r1").expect("replay").len();
            assert_eq!(
                l.retire_unfinished_calls("r1").expect("second retire"),
                Vec::<String>::new(),
                "second pass must settle nothing new"
            );
            assert_eq!(
                l.replay("r1").expect("replay").len(),
                before,
                "second pass must write nothing"
            );
        }

        /// The startup sweep retires unfinished calls before it fails the
        /// run, so a settled run leaves no pending-call residue behind,
        /// while a completed call and an awaiting-approval park are untouched.
        #[test]
        fn settle_expired_runs_retires_unfinished_calls() {
            let (_dir, db) = tmp_db("settle-retires-calls");
            let l = Ledger::open(&db).expect("open ledger");
            start(&l, "orphan");
            started_call(
                &l,
                "orphan",
                "call-1",
                "shell",
                "{\"command\":\"sleep 60\"}",
            );
            started_call(&l, "orphan", "call-done", "read", "{\"path\":\"x\"}");
            l.append(&Event::ToolCompleted {
                run_id: "orphan".into(),
                call_id: "call-done".into(),
                tool: "read".into(),
                provenance: pantheon_api::provenance::Provenance::system("test"),
            })
            .expect("complete");
            start(&l, "parked");
            l.append(&Event::ApprovalRequested {
                run_id: "parked".into(),
                scope: "shell:rm -rf /".into(),
            })
            .expect("park");

            let settled = l.settle_expired_runs().expect("settle");
            assert_eq!(settled, vec!["orphan".to_string()]);
            assert_eq!(l.status("orphan").unwrap().as_deref(), Some("failed"));
            assert!(
                l.unfinished_calls("orphan").expect("re-scan").is_empty(),
                "settled run must have no pending calls"
            );
            let interrupted: Vec<String> = l
                .replay("orphan")
                .expect("replay")
                .into_iter()
                .filter_map(|e| match e.event {
                    Event::ToolInterrupted { call_id, .. } => Some(call_id),
                    _ => None,
                })
                .collect();
            assert_eq!(interrupted, vec!["call-1".to_string()]);
            assert_eq!(
                l.status("parked").unwrap().as_deref(),
                Some("awaiting_approval"),
                "approval parks are still untouched by the sweep"
            );
            assert!(
                l.unfinished_calls("parked")
                    .expect("parked scan")
                    .is_empty(),
                "a parked run has no started calls to retire"
            );
            std::fs::remove_dir_all(&_dir).ok();
        }
    }

    #[cfg(test)]
    mod metrics_tests {
        use super::*;

        #[test]
        fn metrics_unknown_run_errors_instead_of_zeroes() {
            let l = Ledger::open_in_memory().expect("in-memory ledger");
            let err = l
                .metrics("no-such-run")
                .expect_err("unknown run must error");
            assert_eq!(err.code, "LEDGER_NO_RUN");
        }

        #[test]
        fn metrics_known_run_counts() {
            let l = Ledger::open_in_memory().expect("in-memory ledger");
            l.append(&Event::RunStarted {
                run_id: "r1".to_string(),
            })
            .expect("append");
            let m = l.metrics("r1").expect("known run folds");
            assert_eq!(m.runs_started, 1);
            assert_eq!(m.runs_completed, 0);
        }
    }
}

pub mod operations {
    //! Durable operations.
    //!
    //! An operation is a small, versioned state machine persisted in the same
    //! SQLite database as the event ledger.  Tool work is stored as an
    //! operation before execution so a process can be replaced without losing
    //! the operation or accidentally repeating a completed side effect.

    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::logging::now_ms;
    use rusqlite::{params, Connection, OptionalExtension};
    use serde::{Deserialize, Serialize};
    use serde_json::Value;
    use std::path::Path;
    use std::sync::Mutex;

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

    impl OperationStore {
        pub fn open(path: &Path) -> Result<Self, PantheonError> {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| operation_err("OPERATION_MKDIR", e.to_string()))?;
                }
            }
            let conn = Connection::open(path)
                .map_err(|e| operation_err("OPERATION_OPEN", e.to_string()))?;
            Self::from_connection(conn)
        }

        pub fn open_in_memory() -> Result<Self, PantheonError> {
            Self::from_connection(
                Connection::open_in_memory()
                    .map_err(|e| operation_err("OPERATION_OPEN", e.to_string()))?,
            )
        }

        fn from_connection(conn: Connection) -> Result<Self, PantheonError> {
            crate::configure_durability(&conn, "OPERATION")?;
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

        pub fn list(
            &self,
            status: Option<OperationStatus>,
        ) -> Result<Vec<Operation>, PantheonError> {
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
                        decode_row(
                            row.map_err(|e| operation_err("OPERATION_LIST", e.to_string()))?,
                        )
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
                        decode_row(
                            row.map_err(|e| operation_err("OPERATION_LIST", e.to_string()))?,
                        )
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
            self.get(id)?.ok_or_else(|| {
                operation_err("OPERATION_GET", "updated operation disappeared".into())
            })
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
        let state = serde_json::from_str(&row.4)
            .map_err(|e| operation_err("OPERATION_JSON", e.to_string()))?;
        Ok(Operation {
            id: row.0,
            operation_type: row.1,
            version: row.2 as u64,
            status: OperationStatus::from_str(&row.3)?,
            state,
        })
    }
}

pub mod claims {
    //! Durable occurrence claims (scheduler idempotency, §21).
    //!
    //! A fired job must not run twice even after a crash, so occurrence keys
    //! claimed by the scheduler live in storage, not just in process memory.
    //! First claim wins: `claim` is an INSERT OR IGNORE, so a recovered process
    //! replaying the same occurrence gets `false` and does not start a second
    //! run.
    //!
    //! Claim unification: this store and [`Ledger::claim`] share ONE table - the
    //! ledger's `claims` table - when opened on the same database file. A key
    //! claimed through either API is visible to the other, so exactly-once
    //! semantics no longer depend on which call site claimed first. Older
    //! `ClaimStore` databases used a separate `occurrence_claims` table; its rows
    //! are migrated into `claims` on open and the legacy table is dropped.

    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::logging::now_ms;
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
    /// the driver's `HashSet` - re-queueing while a fire is already owed just
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
                    std::fs::create_dir_all(parent)
                        .map_err(|e| err("CLAIM_MKDIR", e.to_string()))?;
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
            let conn =
                Connection::open_in_memory().map_err(|e| err("CLAIM_OPEN", e.to_string()))?;
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
            conn.query_row("SELECT COUNT(*) FROM claims", [], |r| {
                let n: i64 = r.get(0)?;
                Ok(n as usize)
            })
            .map_err(|e| err("CLAIM_QUERY", e.to_string()))
        }

        pub fn is_empty(&self) -> Result<bool, PantheonError> {
            Ok(self.len()? == 0)
        }

        /// Forget claims older than `cutoff_ms` (retention policy).
        ///
        /// Occurrence keys are never released when a run finishes - a late
        /// redelivery must still collapse onto the run it already fired - so the
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
        /// The alternative - deleting only after the drain completes - would
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
}

pub mod leases {
    //! CAS run leases.
    //!
    //! Leases are deliberately separate from process liveness.  A supervisor
    //! owns a lease while it is doing work, renews it from its activity loop, and
    //! must stop mutating the run as soon as the lease can no longer be renewed.
    //! This makes recovery safe even when a process is paused, partitioned, or
    //! has lost its network connection.

    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::logging::now_ms;
    use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
    use serde::{Deserialize, Serialize};
    use std::path::Path;
    use std::sync::Mutex;

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
                    std::fs::create_dir_all(parent)
                        .map_err(|e| err("LEASE_MKDIR", e.to_string()))?;
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
            crate::configure_durability(&conn, "LEASE")?;
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
        pub fn assert_owned(
            &self,
            run_id: &str,
            lease_id: &str,
        ) -> Result<RunLease, LostLeaseError> {
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
}
