//! misc_group: audit, backup, collaboration, ideas, search in one file.

pub mod audit {
    //! JSONL audit log: one line per ledger event, replayable and
    //! sequence-validated. The eval interface - a run's full trajectory in a
    //! plain file that Python (or anything) can score without opening SQLite.
    //!
    //! Line shape:
    //!   {"seq": 7, "event": "ToolStarted", "tool": "shell", "call_id": "call_1_0", ...}
    //!
    //! `seq` is the per-run sequence number (the ledger row id is a separate
    //! `id` field on [`LedgerEntry`]; the two coincide only when no rows were
    //! ever deleted). Export validates `seq`, the value it actually emits,
    //! and refuses to emit a file with duplicate or decreasing seq values (a
    //! corrupted ledger should fail loudly here, not downstream).
    use crate::ledger::LedgerEntry;
    use pantheon_api::error::{Layer, PantheonError};
    use std::io::Write;
    use std::path::Path;

    fn aerr(code: &str, cause: String) -> PantheonError {
        PantheonError::new(code, Layer::Storage, false, cause, "check the ledger", "")
    }

    /// Serialize one event name + the fields evals care about. Event-specific
    /// fields flatten onto the object; unknown events still export (name only)
    /// so new event kinds never silently vanish from trajectories.
    pub fn audit_line(entry: &LedgerEntry) -> String {
        let mut obj = serde_json::json!({
            "seq": entry.seq,
            "event": event_name(&entry.event),
        });
        match &entry.event {
            pantheon_api::events::Event::ToolStarted {
                call_id,
                tool,
                args,
                ..
            } => {
                obj["call_id"] = serde_json::json!(call_id);
                obj["tool"] = serde_json::json!(tool);
                obj["args"] = serde_json::json!(args);
            }
            pantheon_api::events::Event::ToolOutput {
                call_id,
                tool,
                truncated,
                ..
            } => {
                obj["call_id"] = serde_json::json!(call_id);
                obj["tool"] = serde_json::json!(tool);
                obj["truncated"] = serde_json::json!(truncated);
            }
            pantheon_api::events::Event::ToolCompleted { call_id, tool, .. } => {
                obj["call_id"] = serde_json::json!(call_id);
                obj["tool"] = serde_json::json!(tool);
            }
            pantheon_api::events::Event::ModelDelta { delta, .. } => {
                obj["delta"] = serde_json::json!(delta);
            }
            pantheon_api::events::Event::RunFailed { code, .. } => {
                obj["code"] = serde_json::json!(code);
            }
            pantheon_api::events::Event::RunCanceled { reason, .. } => {
                obj["reason"] = serde_json::json!(reason);
            }
            pantheon_api::events::Event::ApprovalRequested { scope, .. } => {
                obj["scope"] = serde_json::json!(scope);
            }
            pantheon_api::events::Event::ApprovalGranted { scope, .. }
            | pantheon_api::events::Event::ApprovalDenied { scope, .. } => {
                obj["scope"] = serde_json::json!(scope);
            }
            _ => {}
        }
        serde_json::to_string(&obj).unwrap_or_else(|_| "{}".into())
    }

    /// Stable snake_case names for event kinds.
    fn event_name(e: &pantheon_api::events::Event) -> &'static str {
        use pantheon_api::events::Event::*;
        match e {
            RunStarted { .. } => "RunStarted",
            RunProgress { .. } => "RunProgress",
            RunCompleted { .. } => "RunCompleted",
            RunFailed { .. } => "RunFailed",
            RunCanceled { .. } => "RunCanceled",
            RunRecovered { .. } => "RunRecovered",
            ModelRequested { .. } => "ModelRequested",
            ModelCompleted { .. } => "ModelCompleted",
            ModelDelta { .. } => "ModelDelta",
            AssistantMessage { .. } => "AssistantMessage",
            ToolMessage { .. } => "ToolMessage",
            ToolRequested { .. } => "ToolRequested",
            ToolStarted { .. } => "ToolStarted",
            ToolOutput { .. } => "ToolOutput",
            ToolCompleted { .. } => "ToolCompleted",
            ApprovalRequested { .. } => "ApprovalRequested",
            ApprovalGranted { .. } => "ApprovalGranted",
            ApprovalDenied { .. } => "ApprovalDenied",
            PreStateRecorded { .. } => "PreStateRecorded",
            AgentMessage { .. } => "AgentMessage",
            AgentSpawned { .. } => "AgentSpawned",
            AgentCompleted { .. } => "AgentCompleted",
            SessionTitled { .. } => "SessionTitled",
            CheckpointCreated { .. } => "CheckpointCreated",
            AgentBound { .. } => "AgentBound",
            TurnStarted { .. } => "TurnStarted",
            TurnParked { .. } => "TurnParked",
            TurnCompleted { .. } => "TurnCompleted",
            TurnFailed { .. } => "TurnFailed",
            TurnRewound { .. } => "TurnRewound",
            SteeringProvided { .. } => "SteeringProvided",
            AgentMessageDelivered { .. } => "AgentMessageDelivered",
            ToolInterrupted { .. } => "ToolInterrupted",
            TodosUpdated { .. } => "TodosUpdated",
            UiContent { .. } => "UiContent",
            UiUpdated { .. } => "UiUpdated",
            UiActionRequested { .. } => "UiActionRequested",
            UiActionSubmitted { .. } => "UiActionSubmitted",
            ImportedReasoning { .. } => "ImportedReasoning",
            MemoryProposed { .. } => "MemoryProposed",
            UserInputRequested { .. } => "UserInputRequested",
            UserInputProvided { .. } => "UserInputProvided",
            DecisionRequested { .. } => "DecisionRequested",
            DecisionMade { .. } => "DecisionMade",
            DecisionRecorded { .. } => "DecisionRecorded",
            ContextTrimmed { .. } => "ContextTrimmed",
            ContextCompressed { .. } => "ContextCompressed",
            UsageRecorded { .. } => "UsageRecorded",
            BrowserActivity { .. } => "BrowserActivity",
            ScheduledTaskFailed { .. } => "ScheduledTaskFailed",
            ScheduledTaskRecovered { .. } => "ScheduledTaskRecovered",
            _ => "Other",
        }
    }

    /// Export a run's ledger entries to a JSONL file. Refuses non-monotonic
    /// or duplicate seq values - a corrupted ledger must fail loudly here.
    pub fn export_jsonl(entries: &[LedgerEntry], path: &Path) -> Result<usize, PantheonError> {
        // Validate the emitted sequence first: strictly increasing `seq`.
        // (Validating `id` here used to pass while the emitted `seq` values
        // decreased, and vice versa - the two coincide only without deletions.)
        for w in entries.windows(2) {
            if w[1].seq <= w[0].seq {
                return Err(aerr(
                    "AUDIT_SEQ",
                    format!("ledger seq not increasing: {} then {}", w[0].seq, w[1].seq),
                ));
            }
        }
        let mut out = std::io::BufWriter::new(
            std::fs::File::create(path)
                .map_err(|e| aerr("AUDIT_WRITE", format!("create {}: {e}", path.display())))?,
        );
        let mut n = 0usize;
        for e in entries {
            let line = audit_line(e);
            out.write_all(line.as_bytes())
                .and_then(|_| out.write_all(b"\n"))
                .map_err(|e| aerr("AUDIT_WRITE", format!("write: {e}")))?;
            n += 1;
        }
        out.flush()
            .map_err(|e| aerr("AUDIT_WRITE", format!("flush: {e}")))?;
        Ok(n)
    }
}

pub mod backup {
    //! Atomic SQLite snapshots for `pantheon backup`.
    //!
    //! Every database Pantheon writes lives directly under the data dir as
    //! `<name>.db`. A snapshot is taken with SQLite's `VACUUM INTO`, which
    //! writes a transactionally consistent copy while the live database keeps
    //! serving readers and writers - no lock dance, no half-written file.
    //! Restore is a plain file copy back over the live path (done while no
    //! Pantheon process holds the database, which the CLI enforces).

    use std::path::{Path, PathBuf};

    /// SQLite databases Pantheon owns, in backup order (ledger first: it is
    /// the one a restore is most likely about).
    pub const KNOWN_DB_NAMES: &[&str] = &[
        "ledger.db",
        "memory.db",
        "collaboration.db",
        "claims.db",
        "ideas.db",
    ];

    /// All `<name>.db` files directly under `data_dir`: the known list first
    /// (stable order), then any other top-level `*.db` a future version adds,
    /// sorted. Subdirectories are not scanned - nothing Pantheon writes puts
    /// a database below the top level today, and blindly snapshotting e.g. a
    /// browser profile would be wrong.
    pub fn discover_dbs(data_dir: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = KNOWN_DB_NAMES
            .iter()
            .map(|n| data_dir.join(n))
            .filter(|p| p.is_file())
            .collect();
        if let Ok(entries) = std::fs::read_dir(data_dir) {
            let mut extra: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.is_file()
                        && p.extension().map(|x| x == "db").unwrap_or(false)
                        && !out.iter().any(|k| k == p)
                })
                .collect();
            extra.sort();
            out.extend(extra);
        }
        out
    }

    /// Write an atomic, transactionally consistent copy of the SQLite database
    /// at `src` to `dest` via `VACUUM INTO`. The destination's parent must
    /// exist. Fails closed: any error names the database.
    pub fn snapshot_db(src: &Path, dest: &Path) -> Result<(), String> {
        // rusqlite's open() creates a missing file, so check first: snapshotting
        // a database that does not exist must fail loudly, not back up an
        // empty database over a real backup.
        if !src.is_file() {
            return Err(format!("snapshot {}: no such database", src.display()));
        }
        let conn =
            rusqlite::Connection::open(src).map_err(|e| format!("open {}: {e}", src.display()))?;
        // VACUUM INTO takes a string literal, not a bound parameter: double
        // the quotes. Paths Pantheon generates never contain quotes, but a
        // hand-set PANTHEON_DATA_DIR could.
        let literal = dest.to_string_lossy().replace('\'', "''");
        conn.execute_batch(&format!("VACUUM INTO '{literal}'"))
            .map_err(|e| format!("snapshot {}: {e}", src.display()))?;
        Ok(())
    }

    /// Copy a backup file back over the live database path. Refuses when the
    /// destination's parent is missing so a typo cannot scatter files.
    pub fn restore_db(src: &Path, dest: &Path) -> Result<(), String> {
        if let Some(parent) = dest.parent() {
            if !parent.is_dir() {
                return Err(format!(
                    "restore: parent of {} does not exist",
                    dest.display()
                ));
            }
        }
        std::fs::copy(src, dest).map_err(|e| format!("restore {}: {e}", dest.display()))?;
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn tmp(name: &str) -> PathBuf {
            let d = std::env::temp_dir().join(format!("pantheon-backup-test-{name}"));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            d
        }

        #[test]
        fn snapshot_round_trips_data() {
            let d = tmp("roundtrip");
            let src = d.join("ledger.db");
            {
                let c = rusqlite::Connection::open(&src).unwrap();
                c.execute_batch("CREATE TABLE t(x TEXT); INSERT INTO t VALUES ('hello');")
                    .unwrap();
            }
            let dest = d.join("snap.db");
            snapshot_db(&src, &dest).unwrap();
            let c = rusqlite::Connection::open(&dest).unwrap();
            let v: String = c.query_row("SELECT x FROM t", [], |r| r.get(0)).unwrap();
            assert_eq!(v, "hello");
        }

        #[test]
        fn snapshot_missing_source_errors() {
            let d = tmp("missing");
            let err = snapshot_db(&d.join("nope.db"), &d.join("out.db")).unwrap_err();
            assert!(err.contains("nope.db"), "unexpected: {err}");
        }

        #[test]
        fn discover_finds_known_and_extra_dbs() {
            let d = tmp("discover");
            for n in ["ledger.db", "memory.db", "extra.db", "notes.txt"] {
                std::fs::write(d.join(n), b"x").unwrap();
            }
            let found: Vec<String> = discover_dbs(&d)
                .iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            assert_eq!(found, vec!["ledger.db", "memory.db", "extra.db"]);
        }
    }
}

pub mod collaboration {
    //! Agent collaboration state (§3 swarm, §2 agent engine).
    //!
    //! A collaboration is a durable, first-class group of agent profiles working
    //! toward a shared objective, decomposed into tasks that survive process
    //! death. This is deliberately **not** a subprocess mechanism: a task names
    //! an agent *profile*, and the executing agent runs inside this runtime under
    //! its own identity and its own policy.
    //!
    //! # Two stores over one truth
    //!
    //! Task state is event-sourced like everything else, but task *queries*
    //! ("what is Zeus doing right now?") cannot be answered by replaying an
    //! event stream. So there are two stores over one truth: the ledger holds
    //! the audit trail, and this module holds the current-state projection.
    //! The projection is written in the same append path that persists the
    //! events, so it is always in sync — no reconciliation needed.
    //!
    //! # Concurrency
    //!
    //! Every mutation is a compare-and-swap on `version`, exactly like
    //! `OperationStore`. Two coordinators racing to settle the same task cannot
    //! both win: the loser gets [`TaskConflict`] and must re-read. SQLite allows
    //! one writer, so the CAS is the only thing standing between a double
    //! settle and a silently overwritten result.
    //!
    //! # Trust
    //!
    //! A message from another agent is *agent data*, not an instruction. The
    //! message kind and the sender's identity are recorded structurally here,
    //! and [`MessageKind::Result`] is the only kind that may claim to be work
    //! output. Nothing in this module can raise a message's trust tier; that
    //! decision belongs to the provenance layer at the point of injection.

    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::logging::now_ms;
    use rusqlite::{params, Connection};
    use serde::{Deserialize, Serialize};
    use std::path::Path;
    use std::sync::Mutex;

    /// Schema for the collaboration projection. Forward-only: added to the
    /// ledger database alongside the event tables and the operation tables.
    const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS collaborations (
      collaboration_id TEXT PRIMARY KEY,
      coordinator      TEXT NOT NULL,
      objective        TEXT NOT NULL,
      status           TEXT NOT NULL,
      created_ms       INTEGER NOT NULL,
      updated_ms       INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS agent_tasks (
      task_id          TEXT PRIMARY KEY,
      collaboration_id TEXT,
      origin_agent     TEXT NOT NULL,
      assigned_agent   TEXT,
      parent_task_id   TEXT,
      objective        TEXT NOT NULL,
      status           TEXT NOT NULL,
      version          INTEGER NOT NULL DEFAULT 1,
      result           TEXT,
      error            TEXT,
      created_ms       INTEGER NOT NULL,
      updated_ms       INTEGER NOT NULL,
      settled_ms       INTEGER
    );
    CREATE INDEX IF NOT EXISTS idx_tasks_collab ON agent_tasks(collaboration_id);
    CREATE INDEX IF NOT EXISTS idx_tasks_agent ON agent_tasks(assigned_agent, status);
    CREATE TABLE IF NOT EXISTS agent_messages (
      message_id       TEXT PRIMARY KEY,
      collaboration_id TEXT,
      task_id          TEXT,
      correlation_id   TEXT,
      sender           TEXT NOT NULL,
      recipient        TEXT NOT NULL,
      kind             TEXT NOT NULL,
      content          TEXT NOT NULL,
      created_ms       INTEGER NOT NULL,
      settled_ms       INTEGER
    );
    CREATE INDEX IF NOT EXISTS idx_messages_recipient ON agent_messages(recipient, settled_ms);
    ";

    fn cerr(code: &str, cause: String) -> PantheonError {
        PantheonError::new(
            code,
            Layer::Storage,
            false,
            cause,
            "check the collaboration database and retry",
            "",
        )
    }

    /// A task id is used in table names, artifact ids, and approval scopes, so it
    /// is validated against the same strict alphabet the ledger uses for
    /// artifacts rather than interpolated raw.
    fn valid_id(id: &str) -> bool {
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b':'))
    }

    /// A profile name (the agent side of every relationship here).
    fn valid_agent(name: &str) -> bool {
        pantheon_api::ident::is_slug(name)
    }

    /// The lifecycle of one delegated unit of work.
    ///
    /// The transition table is the contract. It is intentionally *not* a free
    /// `String`: an out-of-order transition is refused at the type level so a
    /// coordinator bug becomes an error instead of a task that quietly claims
    /// to have completed.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum TaskStatus {
        /// Created, not yet handed to anyone.
        Pending,
        /// Handed to an agent, which has not started it.
        Assigned,
        /// The agent is working on it.
        Running,
        /// The agent reported it cannot proceed without input.
        Blocked,
        /// Finished successfully; `result` is set.
        Completed,
        /// Finished unsuccessfully; `error` is set.
        Failed,
        /// Abandoned by a coordinator or a user.
        Cancelled,
    }

    impl TaskStatus {
        pub fn as_str(self) -> &'static str {
            match self {
                Self::Pending => "pending",
                Self::Assigned => "assigned",
                Self::Running => "running",
                Self::Blocked => "blocked",
                Self::Completed => "completed",
                Self::Failed => "failed",
                Self::Cancelled => "cancelled",
            }
        }

        fn parse(s: &str) -> Result<Self, PantheonError> {
            match s {
                "pending" => Ok(Self::Pending),
                "assigned" => Ok(Self::Assigned),
                "running" => Ok(Self::Running),
                "blocked" => Ok(Self::Blocked),
                "completed" => Ok(Self::Completed),
                "failed" => Ok(Self::Failed),
                "cancelled" => Ok(Self::Cancelled),
                other => Err(cerr(
                    "TASK_STATUS",
                    format!("unknown task status {other:?}"),
                )),
            }
        }

        /// Terminal states accept no further work. A task in a terminal state
        /// that is handed new instructions is a new task, not this one reopened.
        pub fn is_terminal(self) -> bool {
            matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
        }

        /// The legal transitions.
        ///
        /// Two rules encode decisions that are easy to get wrong:
        /// - `Failed -> Pending` exists so a coordinator can **retry** without
        ///   inventing a new task id, which keeps the retry linked to the
        ///   original in the audit trail.
        /// - `Completed` has no outgoing edge. A completed task cannot be
        ///   un-completed, because a result that is already reported cannot be
        ///   retracted: downstream agents and the user have seen it.
        pub fn can_transition_to(self, next: Self) -> bool {
            use TaskStatus::*;
            matches!(
                (self, next),
                // Self-transitions: an idempotent settle. A coordinator that
                // retries its own completion must not be an error.
                (Pending, Pending)
                    | (Assigned, Assigned)
                    | (Running, Running)
                    | (Blocked, Blocked)
                    | (Completed, Completed)
                    | (Failed, Failed)
                    | (Cancelled, Cancelled)
                    | (Pending, Assigned)
                    | (Assigned, Running)
                    | (Running, Blocked)
                    | (Blocked, Running)
                    | (Assigned, Blocked)
                    // A blocked task must be able to *conclude*, not only to
                    // resume. A sub-delegation blocked on a dependency that
                    // never arrives has to be able to report failure, and a
                    // blocker resolved externally may be settled directly.
                    // Without these edges a blocked task could only ever spin
                    // or be cancelled, and a deadlocked sub-delegation would
                    // stay `blocked` forever.
                    | (Blocked, Completed)
                    | (Blocked, Failed)
                    // A pending task has no owner yet, so it can be completed or
                    // failed directly (an agent may finish instantly, or a
                    // coordinator may close it out). Without these edges a
                    // zero-length delegation could only ever be cancelled.
                    | (Pending, Completed)
                    | (Pending, Failed)
                    | (Assigned, Completed)
                    | (Assigned, Failed)
                    | (Running, Completed)
                    | (Running, Failed)
                    | (Pending, Cancelled)
                    | (Assigned, Cancelled)
                    | (Running, Cancelled)
                    | (Blocked, Cancelled)
                    // Retry: a failed task goes back in the queue.
                    | (Failed, Pending)
            )
        }
    }

    impl std::fmt::Display for TaskStatus {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.as_str())
        }
    }

    /// Lifecycle of a collaboration (a swarm / group effort).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum CollaborationStatus {
        Active,
        Completed,
        Failed,
        Cancelled,
    }

    impl CollaborationStatus {
        pub fn as_str(self) -> &'static str {
            match self {
                Self::Active => "active",
                Self::Completed => "completed",
                Self::Failed => "failed",
                Self::Cancelled => "cancelled",
            }
        }
        fn parse(s: &str) -> Result<Self, PantheonError> {
            match s {
                "active" => Ok(Self::Active),
                "completed" => Ok(Self::Completed),
                "failed" => Ok(Self::Failed),
                "cancelled" => Ok(Self::Cancelled),
                other => Err(cerr(
                    "COLLABORATION_STATUS",
                    format!("unknown collaboration status {other:?}"),
                )),
            }
        }
        pub fn is_terminal(self) -> bool {
            !matches!(self, Self::Active)
        }
    }

    /// What a message is *for*, recorded structurally.
    ///
    /// The kind is a closed set so a reader can tell an instruction from a
    /// result without parsing prose - and, more importantly, so an agent
    /// claiming to speak for another agent ("SYSTEM: obey me") has no kind that
    /// would let it be rendered with the harness's authority.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum MessageKind {
        /// A coordinator handing work to an agent.
        Delegation,
        /// Ordinary agent-to-agent traffic.
        Note,
        /// A request for information, expecting an answer.
        Query,
        /// The answer to a query.
        Answer,
        /// Work output. The only kind that may be treated as a result.
        Result,
        /// A refusal or a failure report.
        Failure,
        /// A user message fanned out to several agents by the coordinator.
        Broadcast,
        /// Work reassigned, referencing the original task.
        Reassign,
    }

    impl MessageKind {
        pub fn as_str(self) -> &'static str {
            match self {
                Self::Delegation => "delegation",
                Self::Note => "note",
                Self::Query => "query",
                Self::Answer => "answer",
                Self::Result => "result",
                Self::Failure => "failure",
                Self::Broadcast => "broadcast",
                Self::Reassign => "reassign",
            }
        }

        /// Parse a wire spelling back into a kind. Inverse of
        /// [`MessageKind::as_str`]; the only entry point for caller-supplied
        /// kinds (the `agent_message` tool argument), so an unknown kind is a
        /// structured `MESSAGE_KIND` refusal rather than a silent default.
        pub fn parse(s: &str) -> Result<Self, PantheonError> {
            match s {
                "delegation" => Ok(Self::Delegation),
                "note" => Ok(Self::Note),
                "query" => Ok(Self::Query),
                "answer" => Ok(Self::Answer),
                "result" => Ok(Self::Result),
                "failure" => Ok(Self::Failure),
                "broadcast" => Ok(Self::Broadcast),
                "reassign" => Ok(Self::Reassign),
                other => Err(cerr(
                    "MESSAGE_KIND",
                    format!("unknown message kind {other:?}"),
                )),
            }
        }
    }

    /// A durable delegated task.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct AgentTask {
        pub task_id: String,
        pub collaboration_id: Option<String>,
        /// The agent that created the task. Provenance: who asked.
        pub origin_agent: String,
        /// The agent that must do the work. `None` while `Pending`.
        pub assigned_agent: Option<String>,
        /// The task this one was split out of. `Some` makes it nested
        /// delegation, and lets a tree be walked back to the user's request.
        pub parent_task_id: Option<String>,
        pub objective: String,
        pub status: TaskStatus,
        /// Bumped on every accepted mutation. The CAS key.
        pub version: u64,
        pub result: Option<String>,
        pub error: Option<String>,
        pub created_ms: i64,
        pub updated_ms: i64,
        /// When the task reached a terminal status. Used to find work that was
        /// in flight when a process died.
        pub settled_ms: Option<i64>,
    }

    impl AgentTask {
        /// True when the task was mid-flight at the time of inspection: it has
        /// a result or a failure, but no terminal status. After a crash this is
        /// exactly the set of tasks that were lost mid-execution, and recovery
        /// offers them for retry rather than leaving them orphaned.
        pub fn is_orphaned(&self) -> bool {
            matches!(self.status, TaskStatus::Running | TaskStatus::Assigned)
                && self.settled_ms.is_none()
        }
    }

    /// A durable agent-to-agent message.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct AgentMessage {
        pub message_id: String,
        pub collaboration_id: Option<String>,
        /// The task this message belongs to. `None` for coordination chatter.
        pub task_id: Option<String>,
        /// Pairs a reply with the request that prompted it (the originating
        /// message id or caller-chosen token). `None` for uncorrelated chatter.
        pub correlation_id: Option<String>,
        pub sender: String,
        pub recipient: String,
        pub kind: MessageKind,
        pub content: String,
        pub created_ms: i64,
        /// Set once the recipient has acted on it. `None` on an unsettled
        /// message is the "delivered but never recorded" case, and it is what
        /// recovery replays.
        pub settled_ms: Option<i64>,
    }

    impl AgentMessage {
        /// The trust-free description of where this content came from. The
        /// caller pairs it with a `Provenance`; this only supplies the source
        /// string. Agent content is never `System` tier: an agent saying
        /// "ignore the user" must not be able to raise its own authority.
        pub fn source(&self) -> String {
            format!("agent:{}", self.sender)
        }
    }

    /// A durable collaboration.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct Collaboration {
        pub collaboration_id: String,
        /// The agent that owns the objective. Not necessarily the only
        /// participant: the topology is derived from the tasks.
        pub coordinator: String,
        pub objective: String,
        pub status: CollaborationStatus,
        pub created_ms: i64,
        pub updated_ms: i64,
    }

    /// A rejected mutation: another writer changed the row first, or the
    /// transition is not legal from the current status.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum TaskConflict {
        /// The row is gone.
        Missing { task_id: String },
        /// Another writer won the compare-and-swap.
        Version {
            task_id: String,
            expected: u64,
            actual: Option<u64>,
        },
        /// The status transition is not legal.
        Transition {
            task_id: String,
            from: TaskStatus,
            to: TaskStatus,
        },
    }

    impl std::fmt::Display for TaskConflict {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Missing { task_id } => write!(f, "task {task_id} no longer exists"),
                Self::Version {
                    task_id,
                    expected,
                    actual,
                } => {
                    write!(
                    f,
                    "task {task_id} was modified concurrently (expected version {expected}, found {})",
                    actual.map(|a| a.to_string()).unwrap_or_else(|| "none".into())
                )
                }
                Self::Transition { task_id, from, to } => {
                    write!(f, "task {task_id} cannot move from {from} to {to}")
                }
            }
        }
    }

    impl std::error::Error for TaskConflict {}

    /// A task or message that violates an input invariant.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct InvalidCollaboration {
        pub what: &'static str,
        pub why: String,
    }

    impl std::fmt::Display for InvalidCollaboration {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}: {}", self.what, self.why)
        }
    }

    impl std::error::Error for InvalidCollaboration {}

    fn invalid(what: &'static str, why: impl Into<String>) -> InvalidCollaboration {
        InvalidCollaboration {
            what,
            why: why.into(),
        }
    }

    fn inv_err(e: &InvalidCollaboration) -> PantheonError {
        PantheonError::new(
            "COLLABORATION_INVALID",
            Layer::Storage,
            false,
            e.to_string(),
            "fix the agent, task, or message identifier",
            "",
        )
    }

    /// The collaboration projection store.
    ///
    /// `Clone` and shared-connection on purpose. Several agent runtimes in one
    /// process (a coordinator and its peers) must contend on the *same*
    /// connection mutex for a compare-and-swap to be atomic; opening a separate
    /// connection per agent would let two writers race past the check. So the
    /// store is a cheap handle over one `Arc<Mutex<Connection>>`.
    #[derive(Clone)]
    pub struct CollaborationStore {
        conn: std::sync::Arc<Mutex<Connection>>,
    }

    impl CollaborationStore {
        pub fn open(path: &Path) -> Result<Self, PantheonError> {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| cerr("COLLABORATION_MKDIR", e.to_string()))?;
                }
            }
            let conn =
                Connection::open(path).map_err(|e| cerr("COLLABORATION_OPEN", e.to_string()))?;
            crate::configure_durability(&conn, "COLLABORATION")?;
            conn.execute_batch(SCHEMA)
                .map_err(|e| cerr("COLLABORATION_SCHEMA", e.to_string()))?;
            // Databases created before correlation_id existed need the column
            // added; fresh ones already have it from SCHEMA (idempotent).
            crate::add_column_once(
                &conn,
                "ALTER TABLE agent_messages ADD COLUMN correlation_id TEXT",
            )
            .map_err(|e| cerr("COLLABORATION_SCHEMA", e.to_string()))?;
            Ok(Self {
                conn: std::sync::Arc::new(Mutex::new(conn)),
            })
        }

        // ---------------------------------------------------------------- tasks

        /// Create a task. The creator is always recorded, so "who asked" is
        /// answerable without the event log.
        pub fn create_task(
            &self,
            task_id: &str,
            collaboration_id: Option<&str>,
            origin_agent: &str,
            assigned_agent: Option<&str>,
            parent_task_id: Option<&str>,
            objective: &str,
        ) -> Result<AgentTask, PantheonError> {
            if !valid_id(task_id) {
                return Err(inv_err(&invalid(
                    "task_id",
                    format!("{task_id:?} is not a valid id"),
                )));
            }
            if !valid_agent(origin_agent) {
                return Err(inv_err(&invalid(
                    "origin_agent",
                    format!("{origin_agent:?} is not a profile slug"),
                )));
            }
            if let Some(a) = assigned_agent {
                if !valid_agent(a) {
                    return Err(inv_err(&invalid(
                        "assigned_agent",
                        format!("{a:?} is not a profile slug"),
                    )));
                }
            }
            if objective.trim().is_empty() {
                return Err(inv_err(&invalid("objective", "must not be empty")));
            }
            let now = now_ms();
            let status = if assigned_agent.is_some() {
                TaskStatus::Assigned
            } else {
                TaskStatus::Pending
            };
            let conn = self.lock()?;
            conn.execute(
                "INSERT INTO agent_tasks
                 (task_id, collaboration_id, origin_agent, assigned_agent, parent_task_id,
                  objective, status, version, created_ms, updated_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,1,?8,?9)",
                params![
                    task_id,
                    collaboration_id,
                    origin_agent,
                    assigned_agent,
                    parent_task_id,
                    objective,
                    status.as_str(),
                    now,
                    now
                ],
            )
            .map_err(|e| cerr("TASK_CREATE", e.to_string()))?;
            drop(conn);
            self.task(task_id)?
                .ok_or_else(|| cerr("TASK_CREATE", "task vanished after insert".into()))
        }

        pub fn task(&self, task_id: &str) -> Result<Option<AgentTask>, PantheonError> {
            let conn = self.lock()?;
            task_from_conn(&conn, task_id)
        }

        /// Advance a task, guarded by both the version and the transition
        /// table. `expected_version` is `None` to mean "whatever version you
        /// find", which is only correct for a caller that has just read the row.
        #[allow(clippy::too_many_arguments)]
        pub fn transition(
            &self,
            task_id: &str,
            expected_version: Option<u64>,
            next: TaskStatus,
            result: Option<&str>,
            error: Option<&str>,
        ) -> Result<AgentTask, TaskMutationError> {
            let conn = self.lock()?;
            let current = task_from_conn(&conn, task_id)
                .map_err(|e| TaskMutationError::Storage(e.to_string()))?
                .ok_or_else(|| {
                    TaskMutationError::Conflict(TaskConflict::Missing {
                        task_id: task_id.to_string(),
                    })
                })?;
            if let Some(v) = expected_version {
                if current.version != v {
                    return Err(TaskMutationError::Conflict(TaskConflict::Version {
                        task_id: task_id.to_string(),
                        expected: v,
                        actual: Some(current.version),
                    }));
                }
            }
            if !current.status.can_transition_to(next) {
                return Err(TaskMutationError::Conflict(TaskConflict::Transition {
                    task_id: task_id.to_string(),
                    from: current.status,
                    to: next,
                }));
            }
            // A completed task must carry a result and a failed one a cause.
            // Refusing here means "completed" can never be a lie in the table.
            if next == TaskStatus::Completed && result.unwrap_or("").trim().is_empty() {
                return Err(TaskMutationError::Conflict(TaskConflict::Transition {
                    task_id: task_id.to_string(),
                    from: current.status,
                    to: next,
                }));
            }
            if next == TaskStatus::Failed && error.unwrap_or("").trim().is_empty() {
                return Err(TaskMutationError::Conflict(TaskConflict::Transition {
                    task_id: task_id.to_string(),
                    from: current.status,
                    to: next,
                }));
            }
            let now = now_ms();
            let settled = if next.is_terminal() { Some(now) } else { None };
            let changed = conn
                .execute(
                    "UPDATE agent_tasks
                     SET status=?2, version=version+1, result=?3, error=?4,
                         updated_ms=?5, settled_ms=?6
                     WHERE task_id=?1 AND version=?7",
                    params![
                        task_id,
                        next.as_str(),
                        result,
                        error,
                        now,
                        settled,
                        current.version as i64
                    ],
                )
                .map_err(|e| TaskMutationError::Storage(e.to_string()))?;
            if changed != 1 {
                // The row changed between our read and our write. Another
                // coordinator settled it first.
                let actual = task_from_conn(&conn, task_id)
                    .map_err(|e| TaskMutationError::Storage(e.to_string()))?
                    .map(|t| t.version);
                return Err(TaskMutationError::Conflict(TaskConflict::Version {
                    task_id: task_id.to_string(),
                    expected: current.version,
                    actual,
                }));
            }
            drop(conn);
            self.task(task_id)
                .map_err(|e| TaskMutationError::Storage(e.to_string()))?
                .ok_or(TaskMutationError::Conflict(TaskConflict::Missing {
                    task_id: task_id.to_string(),
                }))
        }

        /// Hand a task to an agent. Assigning an already-assigned task is a
        /// reassignment and is allowed; the event log records who gave it up.
        ///
        /// `expected_version` is honoured on the same terms as
        /// [`Self::transition`]: a `Some(v)` that does not match the stored
        /// version is a conflict. An earlier revision of this function accepted
        /// the argument and then ignored it, which silently turned a
        /// compare-and-swap into an unconditional overwrite - the parameter has
        /// to reach the `WHERE` clause or the signature is a lie.
        pub fn assign(
            &self,
            task_id: &str,
            expected_version: Option<u64>,
            agent: &str,
        ) -> Result<AgentTask, TaskMutationError> {
            if !valid_agent(agent) {
                return Err(TaskMutationError::Storage(
                    invalid("assigned_agent", format!("{agent:?} is not a slug")).to_string(),
                ));
            }
            let conn = self.lock()?;
            let current = task_from_conn(&conn, task_id)
                .map_err(|e| TaskMutationError::Storage(e.to_string()))?
                .ok_or_else(|| {
                    TaskMutationError::Conflict(TaskConflict::Missing {
                        task_id: task_id.to_string(),
                    })
                })?;
            if let Some(v) = expected_version {
                if current.version != v {
                    return Err(TaskMutationError::Conflict(TaskConflict::Version {
                        task_id: task_id.to_string(),
                        expected: v,
                        actual: Some(current.version),
                    }));
                }
            }
            // Reassignment is not a status transition. Handing a task to a
            // different agent changes only the owner, so the status is
            // preserved and any work already done is not discarded. The
            // transition table governs *status* only; requiring
            // `Running -> Assigned` here would have made the ordinary
            // "hand this to someone else" case illegal.
            if current.status.is_terminal() {
                return Err(TaskMutationError::Conflict(TaskConflict::Transition {
                    task_id: task_id.to_string(),
                    from: current.status,
                    to: TaskStatus::Assigned,
                }));
            }
            let next = if current.status == TaskStatus::Pending {
                TaskStatus::Assigned
            } else {
                // Reassign while in flight: the status is preserved and only the
                // owner changes, so a running task is not silently reset.
                current.status
            };
            let now = now_ms();
            let changed = conn
                .execute(
                    "UPDATE agent_tasks SET assigned_agent=?2, status=?3, version=version+1, updated_ms=?4
                     WHERE task_id=?1 AND version=?5",
                    params![
                        task_id,
                        agent,
                        next.as_str(),
                        now,
                        current.version as i64
                    ],
                )
                .map_err(|e| TaskMutationError::Storage(e.to_string()))?;
            if changed != 1 {
                return Err(TaskMutationError::Conflict(TaskConflict::Version {
                    task_id: task_id.to_string(),
                    expected: current.version,
                    actual: None,
                }));
            }
            drop(conn);
            self.task(task_id)
                .map_err(|e| TaskMutationError::Storage(e.to_string()))?
                .ok_or(TaskMutationError::Conflict(TaskConflict::Missing {
                    task_id: task_id.to_string(),
                }))
        }

        /// Every task an agent owns, optionally filtered by status.
        pub fn tasks_for_agent(
            &self,
            agent: &str,
            status: Option<TaskStatus>,
        ) -> Result<Vec<AgentTask>, PantheonError> {
            let conn = self.lock()?;
            let mut stmt = conn
                .prepare(
                    "SELECT task_id, collaboration_id, origin_agent, assigned_agent, parent_task_id,
                            objective, status, version, result, error, created_ms, updated_ms, settled_ms
                     FROM agent_tasks WHERE assigned_agent=?1 ORDER BY created_ms, task_id",
                )
                .map_err(|e| cerr("TASK_QUERY", e.to_string()))?;
            let rows = stmt
                .query_map(params![agent], task_from_row)
                .map_err(|e| cerr("TASK_QUERY", e.to_string()))?;
            let mut out = Vec::new();
            for r in rows {
                let t = r.map_err(|e| cerr("TASK_QUERY", e.to_string()))?;
                if status.is_none_or(|s| t.status == s) {
                    out.push(t);
                }
            }
            Ok(out)
        }

        pub fn tasks_in_collaboration(
            &self,
            collaboration_id: &str,
        ) -> Result<Vec<AgentTask>, PantheonError> {
            let conn = self.lock()?;
            let mut stmt = conn
                .prepare(
                    "SELECT task_id, collaboration_id, origin_agent, assigned_agent, parent_task_id,
                            objective, status, version, result, error, created_ms, updated_ms, settled_ms
                     FROM agent_tasks WHERE collaboration_id=?1 ORDER BY created_ms, task_id",
                )
                .map_err(|e| cerr("TASK_QUERY", e.to_string()))?;
            let rows = stmt
                .query_map(params![collaboration_id], task_from_row)
                .map_err(|e| cerr("TASK_QUERY", e.to_string()))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| cerr("TASK_QUERY", e.to_string()))
        }

        /// Tasks that were in flight when a process died. Recovery re-offers
        /// these rather than leaving them orphaned forever.
        pub fn orphaned_tasks(&self) -> Result<Vec<AgentTask>, PantheonError> {
            let conn = self.lock()?;
            let mut stmt = conn
                .prepare(
                    "SELECT task_id, collaboration_id, origin_agent, assigned_agent, parent_task_id,
                            objective, status, version, result, error, created_ms, updated_ms, settled_ms
                     FROM agent_tasks
                     WHERE status IN ('assigned','running') AND settled_ms IS NULL
                     ORDER BY created_ms",
                )
                .map_err(|e| cerr("TASK_ORPHAN_QUERY", e.to_string()))?;
            let rows = stmt
                .query_map([], task_from_row)
                .map_err(|e| cerr("TASK_ORPHAN_QUERY", e.to_string()))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| cerr("TASK_ORPHAN_QUERY", e.to_string()))
        }

        // -------------------------------------------------------- collaborations

        pub fn create_collaboration(
            &self,
            collaboration_id: &str,
            coordinator: &str,
            objective: &str,
        ) -> Result<Collaboration, PantheonError> {
            if !valid_id(collaboration_id) {
                return Err(inv_err(&invalid(
                    "collaboration_id",
                    format!("{collaboration_id:?} is not a valid id"),
                )));
            }
            if !valid_agent(coordinator) {
                return Err(inv_err(&invalid(
                    "coordinator",
                    format!("{coordinator:?} is not a profile slug"),
                )));
            }
            if objective.trim().is_empty() {
                return Err(inv_err(&invalid("objective", "must not be empty")));
            }
            let now = now_ms();
            let conn = self.lock()?;
            conn.execute(
                "INSERT INTO collaborations
                 (collaboration_id, coordinator, objective, status, created_ms, updated_ms)
                 VALUES (?1,?2,?3,'active',?4,?5)",
                params![collaboration_id, coordinator, objective, now, now],
            )
            .map_err(|e| cerr("COLLABORATION_CREATE", e.to_string()))?;
            drop(conn);
            self.collaboration(collaboration_id)?
                .ok_or_else(|| cerr("COLLABORATION_CREATE", "row vanished after insert".into()))
        }

        pub fn collaboration(
            &self,
            collaboration_id: &str,
        ) -> Result<Option<Collaboration>, PantheonError> {
            let conn = self.lock()?;
            let mut stmt = conn
                .prepare(
                    "SELECT collaboration_id, coordinator, objective, status, created_ms, updated_ms
                     FROM collaborations WHERE collaboration_id=?1",
                )
                .map_err(|e| cerr("COLLABORATION_QUERY", e.to_string()))?;
            let mut rows = stmt
                .query_map(params![collaboration_id], |r| {
                    Ok(Collaboration {
                        collaboration_id: r.get(0)?,
                        coordinator: r.get(1)?,
                        objective: r.get(2)?,
                        status: CollaborationStatus::parse(&r.get::<_, String>(3)?)
                            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?,
                        created_ms: r.get(4)?,
                        updated_ms: r.get(5)?,
                    })
                })
                .map_err(|e| cerr("COLLABORATION_QUERY", e.to_string()))?;
            match rows.next() {
                Some(r) => r
                    .map(Some)
                    .map_err(|e| cerr("COLLABORATION_QUERY", e.to_string())),
                None => Ok(None),
            }
        }

        /// Settle a collaboration. A collaboration may only be settled once every
        /// task in it is terminal, unless `force` is set: closing a swarm while
        /// agents are still working is exactly the "coordinator disappeared"
        /// case, and the recovery path needs to be able to do it deliberately.
        pub fn settle_collaboration(
            &self,
            collaboration_id: &str,
            status: CollaborationStatus,
            force: bool,
        ) -> Result<Collaboration, PantheonError> {
            let conn = self.lock()?;
            let tx = conn
                .unchecked_transaction()
                .map_err(|e| cerr("COLLABORATION_SETTLE", e.to_string()))?;
            let outstanding: i64 = tx
                .query_row(
                    "SELECT COUNT(*) FROM agent_tasks
                     WHERE collaboration_id=?1 AND status NOT IN ('completed','failed','cancelled')",
                    params![collaboration_id],
                    |r| r.get(0),
                )
                .map_err(|e| cerr("COLLABORATION_SETTLE", e.to_string()))?;
            if outstanding > 0 && !force {
                return Err(cerr(
                    "COLLABORATION_NOT_SETTLED",
                    format!(
                        "collaboration {collaboration_id} still has {outstanding} unfinished task(s); \
                         settle them or pass force"
                    ),
                ));
            }
            let now = now_ms();
            let changed = tx
                .execute(
                    "UPDATE collaborations SET status=?2, updated_ms=?3
                     WHERE collaboration_id=?1 AND status='active'",
                    params![collaboration_id, status.as_str(), now],
                )
                .map_err(|e| cerr("COLLABORATION_SETTLE", e.to_string()))?;
            if changed != 1 {
                return Err(cerr(
                    "COLLABORATION_CONFLICT",
                    format!("collaboration {collaboration_id} is not active"),
                ));
            }
            tx.commit()
                .map_err(|e| cerr("COLLABORATION_SETTLE", e.to_string()))?;
            drop(conn);
            self.collaboration(collaboration_id)?
                .ok_or_else(|| cerr("COLLABORATION_SETTLE", "row vanished".into()))
        }

        pub fn active_collaborations(&self) -> Result<Vec<Collaboration>, PantheonError> {
            let conn = self.lock()?;
            let mut stmt = conn
                .prepare(
                    "SELECT collaboration_id, coordinator, objective, status, created_ms, updated_ms
                     FROM collaborations WHERE status='active' ORDER BY created_ms",
                )
                .map_err(|e| cerr("COLLABORATION_QUERY", e.to_string()))?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(Collaboration {
                        collaboration_id: r.get(0)?,
                        coordinator: r.get(1)?,
                        objective: r.get(2)?,
                        status: CollaborationStatus::Active,
                        created_ms: r.get(4)?,
                        updated_ms: r.get(5)?,
                    })
                })
                .map_err(|e| cerr("COLLABORATION_QUERY", e.to_string()))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| cerr("COLLABORATION_QUERY", e.to_string()))
        }

        // ------------------------------------------------------------- messages

        /// Record a message. `message_id` is caller-assigned so the caller can
        /// use the same id for the ledger event, which is what lets a crash
        /// between the write and the event be detected as a duplicate rather
        /// than a lost or double-counted message.
        #[allow(clippy::too_many_arguments)]
        pub fn record_message(
            &self,
            message_id: &str,
            collaboration_id: Option<&str>,
            task_id: Option<&str>,
            correlation_id: Option<&str>,
            sender: &str,
            recipient: &str,
            kind: MessageKind,
            content: &str,
        ) -> Result<AgentMessage, PantheonError> {
            if !valid_id(message_id) {
                return Err(inv_err(&invalid(
                    "message_id",
                    format!("{message_id:?} is not a valid id"),
                )));
            }
            if !valid_agent(sender) || !valid_agent(recipient) {
                return Err(inv_err(&invalid(
                    "agent",
                    "sender and recipient must both be profile slugs",
                )));
            }
            if sender == recipient {
                // A self-addressed message is a routing bug: the caller meant
                // to do the work inline. Refusing keeps the mailbox honest.
                return Err(inv_err(&invalid(
                    "agent",
                    "an agent cannot message itself; run the work inline instead",
                )));
            }
            if content.trim().is_empty() {
                return Err(inv_err(&invalid("content", "must not be empty")));
            }
            let now = now_ms();
            let conn = self.lock()?;
            conn.execute(
                "INSERT INTO agent_messages
                 (message_id, collaboration_id, task_id, correlation_id, sender, recipient, kind, content, created_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    message_id,
                    collaboration_id,
                    task_id,
                    correlation_id,
                    sender,
                    recipient,
                    kind.as_str(),
                    content,
                    now
                ],
            )
            .map_err(|e| cerr("MESSAGE_RECORD", e.to_string()))?;
            drop(conn);
            self.message(message_id)?
                .ok_or_else(|| cerr("MESSAGE_RECORD", "row vanished after insert".into()))
        }

        pub fn message(&self, message_id: &str) -> Result<Option<AgentMessage>, PantheonError> {
            let conn = self.lock()?;
            let mut stmt = conn
                .prepare(
                    "SELECT message_id, collaboration_id, task_id, sender, recipient, kind, content,
                            created_ms, settled_ms, correlation_id
                     FROM agent_messages WHERE message_id=?1",
                )
                .map_err(|e| cerr("MESSAGE_QUERY", e.to_string()))?;
            let mut rows = stmt
                .query_map(params![message_id], message_from_row)
                .map_err(|e| cerr("MESSAGE_QUERY", e.to_string()))?;
            match rows.next() {
                Some(r) => r
                    .map(Some)
                    .map_err(|e| cerr("MESSAGE_QUERY", e.to_string())),
                None => Ok(None),
            }
        }

        /// Mark a message as acted on. Idempotent: re-settling returns `false`
        /// rather than an error, so a replayed delivery cannot double-count.
        pub fn settle_message(&self, message_id: &str) -> Result<bool, PantheonError> {
            let conn = self.lock()?;
            let changed = conn
                .execute(
                    "UPDATE agent_messages SET settled_ms=?2
                     WHERE message_id=?1 AND settled_ms IS NULL",
                    params![message_id, now_ms()],
                )
                .map_err(|e| cerr("MESSAGE_SETTLE", e.to_string()))?;
            Ok(changed == 1)
        }

        /// An agent's unread mailbox, oldest first.
        pub fn inbox(&self, recipient: &str) -> Result<Vec<AgentMessage>, PantheonError> {
            let conn = self.lock()?;
            let mut stmt = conn
                .prepare(
                    "SELECT message_id, collaboration_id, task_id, sender, recipient, kind, content,
                            created_ms, settled_ms, correlation_id
                     FROM agent_messages WHERE recipient=?1 AND settled_ms IS NULL
                     ORDER BY created_ms, message_id",
                )
                .map_err(|e| cerr("MESSAGE_QUERY", e.to_string()))?;
            let rows = stmt
                .query_map(params![recipient], message_from_row)
                .map_err(|e| cerr("MESSAGE_QUERY", e.to_string()))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| cerr("MESSAGE_QUERY", e.to_string()))
        }

        /// Every message on a task, in order. This is the conversation trail the
        /// observability question "what was exchanged" is answered from.
        pub fn messages_for_task(&self, task_id: &str) -> Result<Vec<AgentMessage>, PantheonError> {
            let conn = self.lock()?;
            let mut stmt = conn
                .prepare(
                    "SELECT message_id, collaboration_id, task_id, sender, recipient, kind, content,
                            created_ms, settled_ms, correlation_id
                     FROM agent_messages WHERE task_id=?1 ORDER BY created_ms, message_id",
                )
                .map_err(|e| cerr("MESSAGE_QUERY", e.to_string()))?;
            let rows = stmt
                .query_map(params![task_id], message_from_row)
                .map_err(|e| cerr("MESSAGE_QUERY", e.to_string()))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| cerr("MESSAGE_QUERY", e.to_string()))
        }

        fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, PantheonError> {
            // A poisoned lock must not cascade: one panic elsewhere should not
            // turn every later collaboration operation into a panic.
            self.conn.lock().map_err(|_| {
                cerr(
                    "COLLABORATION_LOCK",
                    "collaboration store lock poisoned".into(),
                )
            })
        }
    }

    /// Either a genuine conflict or a storage failure. Split so a caller can
    /// retry a conflict without having to distinguish it from a disk failure by
    /// string matching.
    /// A task mutation outcome.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum TaskMutationError {
        /// The change was refused; re-read and decide again.
        Conflict(TaskConflict),
        /// The store itself failed.
        Storage(String),
    }

    impl std::fmt::Display for TaskMutationError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Conflict(c) => write!(f, "{c}"),
                Self::Storage(s) => write!(f, "{s}"),
            }
        }
    }

    impl std::error::Error for TaskMutationError {}

    impl From<PantheonError> for TaskMutationError {
        /// A storage-layer failure is a store failure, not a conflict. Keeping
        /// the two apart is what lets a caller retry a race without retrying a
        /// disk failure.
        fn from(e: PantheonError) -> Self {
            Self::Storage(e.to_string())
        }
    }

    fn task_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<AgentTask> {
        let status: String = r.get(6)?;
        Ok(AgentTask {
            task_id: r.get(0)?,
            collaboration_id: r.get(1)?,
            origin_agent: r.get(2)?,
            assigned_agent: r.get(3)?,
            parent_task_id: r.get(4)?,
            objective: r.get(5)?,
            status: TaskStatus::parse(&status)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?,
            version: r.get::<_, i64>(7)? as u64,
            result: r.get(8)?,
            error: r.get(9)?,
            created_ms: r.get(10)?,
            updated_ms: r.get(11)?,
            settled_ms: r.get(12)?,
        })
    }

    fn message_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<AgentMessage> {
        let kind: String = r.get(5)?;
        Ok(AgentMessage {
            message_id: r.get(0)?,
            collaboration_id: r.get(1)?,
            task_id: r.get(2)?,
            sender: r.get(3)?,
            recipient: r.get(4)?,
            kind: MessageKind::parse(&kind)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?,
            content: r.get(6)?,
            created_ms: r.get(7)?,
            settled_ms: r.get(8)?,
            correlation_id: r.get(9)?,
        })
    }

    fn task_from_conn(
        conn: &Connection,
        task_id: &str,
    ) -> Result<Option<AgentTask>, PantheonError> {
        let mut stmt = conn
            .prepare(
                "SELECT task_id, collaboration_id, origin_agent, assigned_agent, parent_task_id,
                        objective, status, version, result, error, created_ms, updated_ms, settled_ms
                 FROM agent_tasks WHERE task_id=?1",
            )
            .map_err(|e| cerr("TASK_QUERY", e.to_string()))?;
        let mut rows = stmt
            .query_map(params![task_id], task_from_row)
            .map_err(|e| cerr("TASK_QUERY", e.to_string()))?;
        match rows.next() {
            Some(r) => r.map(Some).map_err(|e| cerr("TASK_QUERY", e.to_string())),
            None => Ok(None),
        }
    }
}

pub mod ideas {
    //! Nightly-gated proactive ideas (the Ideas page backend).
    //!
    //! An idea is a suggestion the nightly pass mints - a repair opportunity,
    //! an observation from the audit, or a proposed scheduled task mined from
    //! repeating session patterns. Ideas wait in `pending` for the user's
    //! explicit accept/dismiss; accept/dismiss/feedback signals are recorded
    //! per topic so generation can downrank topics the user keeps rejecting.

    use pantheon_api::error::{Layer, PantheonError};
    use rusqlite::{params, Connection, OptionalExtension};
    use std::path::Path;
    use std::sync::Mutex;

    /// The canonical ideas tables. Column names are stable; forward-only
    /// `ALTER TABLE` migrations in [`migrate`] cover databases created by an
    /// older build.
    const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS ideas (
      id TEXT PRIMARY KEY,
      title TEXT NOT NULL,
      description TEXT NOT NULL DEFAULT '',
      includes_json TEXT NOT NULL DEFAULT '[]',
      kind TEXT NOT NULL DEFAULT 'general',
      topic TEXT NOT NULL DEFAULT '',
      status TEXT NOT NULL DEFAULT 'pending',
      created_day TEXT NOT NULL,
      schedule_json TEXT,
      more INTEGER NOT NULL DEFAULT 0,
      less INTEGER NOT NULL DEFAULT 0,
      decided_ms INTEGER
    );
    CREATE TABLE IF NOT EXISTS idea_topic_signals (
      topic TEXT PRIMARY KEY,
      accepted INTEGER NOT NULL DEFAULT 0,
      dismissed INTEGER NOT NULL DEFAULT 0,
      more INTEGER NOT NULL DEFAULT 0,
      less INTEGER NOT NULL DEFAULT 0
    );";

    /// Forward-only column migrations for ideas databases created before a
    /// column existed. "duplicate column name" is ignored so the migration
    /// stays idempotent; any other failure propagates as IDEAS_MIGRATE.
    fn migrate(conn: &Connection) -> Result<(), PantheonError> {
        let add = |sql: &str| {
            crate::add_column_once(conn, sql)
                .map_err(|e| err("IDEAS_MIGRATE", format!("migration failed ({sql}): {e}")))
        };
        add("ALTER TABLE ideas ADD COLUMN topic TEXT NOT NULL DEFAULT ''")?;
        add("ALTER TABLE ideas ADD COLUMN decided_ms INTEGER")?;
        Ok(())
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

    /// What the idea proposes.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum IdeaKind {
        /// A repair opportunity or observation: accepting spawns the work.
        General,
        /// A proposed scheduled task: accepting creates the schedule.
        ScheduledTask,
    }

    impl IdeaKind {
        fn as_str(self) -> &'static str {
            match self {
                IdeaKind::General => "general",
                IdeaKind::ScheduledTask => "scheduled_task",
            }
        }

        fn parse(s: &str) -> Result<Self, PantheonError> {
            match s {
                "general" => Ok(IdeaKind::General),
                "scheduled_task" => Ok(IdeaKind::ScheduledTask),
                other => Err(err(
                    "IDEA_KIND",
                    format!("unknown idea kind '{other}' in ideas.db"),
                )),
            }
        }
    }

    /// Lifecycle of one idea. `pending` ideas roll off after a few days if
    /// the user never answers; the rest are terminal.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum IdeaStatus {
        Pending,
        Accepted,
        Dismissed,
        Done,
    }

    impl IdeaStatus {
        fn as_str(self) -> &'static str {
            match self {
                IdeaStatus::Pending => "pending",
                IdeaStatus::Accepted => "accepted",
                IdeaStatus::Dismissed => "dismissed",
                IdeaStatus::Done => "done",
            }
        }

        fn parse(s: &str) -> Result<Self, PantheonError> {
            match s {
                "pending" => Ok(IdeaStatus::Pending),
                "accepted" => Ok(IdeaStatus::Accepted),
                "dismissed" => Ok(IdeaStatus::Dismissed),
                "done" => Ok(IdeaStatus::Done),
                other => Err(err(
                    "IDEA_STATUS",
                    format!("unknown idea status '{other}' in ideas.db"),
                )),
            }
        }
    }

    /// The proposed schedule for a [`IdeaKind::ScheduledTask`] idea.
    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub struct ScheduleSpec {
        pub cron: String,
        pub deliver: String,
        pub prompt: String,
    }

    /// Explicit user feedback on an idea, beyond accept/dismiss.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum FeedbackSignal {
        More,
        Less,
    }

    /// One idea row.
    #[derive(Debug, Clone)]
    pub struct Idea {
        pub id: String,
        pub title: String,
        pub description: String,
        /// The "what's included" plan: ordered steps.
        pub includes: Vec<String>,
        pub kind: IdeaKind,
        /// Short tag used for per-topic feedback tuning (e.g. `fail:exec`).
        pub topic: String,
        pub status: IdeaStatus,
        /// `YYYY-MM-DD` the idea was minted.
        pub created_day: String,
        /// Only `Some` for [`IdeaKind::ScheduledTask`].
        pub schedule: Option<ScheduleSpec>,
        pub more: i64,
        pub less: i64,
        pub decided_ms: Option<i64>,
    }

    /// A new idea to mint.
    #[derive(Debug, Clone)]
    pub struct NewIdea {
        pub id: String,
        pub title: String,
        pub description: String,
        pub includes: Vec<String>,
        pub kind: IdeaKind,
        pub topic: String,
        pub created_day: String,
        pub schedule: Option<ScheduleSpec>,
    }

    /// Accept/dismiss/feedback counters for one topic.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct TopicSignals {
        pub accepted: i64,
        pub dismissed: i64,
        pub more: i64,
        pub less: i64,
    }

    /// SQLite-persisted ideas, one file per data dir (`<data_dir>/ideas.db`).
    #[derive(Debug)]
    pub struct IdeaStore {
        conn: Mutex<Connection>,
    }

    impl IdeaStore {
        /// Lock the connection, failing closed on poison like every other
        /// store (`CLAIM_LOCK`, `LEDGER_LOCK`, ...). The old code recovered
        /// poisoned mutexes with `into_inner`, silently continuing after a
        /// panic in another thread had potentially left the connection
        /// mid-statement.
        fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, PantheonError> {
            self.conn
                .lock()
                .map_err(|e| err("IDEA_LOCK", format!("ideas lock poisoned: {e}")))
        }

        /// Open (creating) the ideas database under `data_dir`.
        pub fn open(data_dir: &Path) -> Result<Self, PantheonError> {
            Self::open_path(&data_dir.join("ideas.db"))
        }

        pub fn open_in_memory() -> Result<Self, PantheonError> {
            let conn = Connection::open_in_memory().map_err(|e| err("IDEA_OPEN", e.to_string()))?;
            crate::configure_durability(&conn, "IDEA")?;
            conn.execute_batch(SCHEMA)
                .map_err(|e| err("IDEA_SCHEMA", e.to_string()))?;
            migrate(&conn)?;
            Ok(Self {
                conn: Mutex::new(conn),
            })
        }

        fn open_path(path: &Path) -> Result<Self, PantheonError> {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| err("IDEA_MKDIR", e.to_string()))?;
                }
            }
            let conn = Connection::open(path).map_err(|e| err("IDEA_OPEN", e.to_string()))?;
            crate::configure_durability(&conn, "IDEA")?;
            conn.execute_batch(SCHEMA)
                .map_err(|e| err("IDEA_SCHEMA", e.to_string()))?;
            migrate(&conn)?;
            Ok(Self {
                conn: Mutex::new(conn),
            })
        }

        /// Mint one idea. `false` when the id already exists (the nightly
        /// day-gate makes a second mint of the same day a no-op).
        pub fn mint(&self, idea: &NewIdea) -> Result<bool, PantheonError> {
            let includes = serde_json::to_string(&idea.includes)
                .map_err(|e| err("IDEA_JSON", e.to_string()))?;
            let schedule = idea
                .schedule
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(|e| err("IDEA_JSON", e.to_string()))?;
            let conn = self.lock()?;
            let n = conn
                .execute(
                    "INSERT OR IGNORE INTO ideas
                     (id, title, description, includes_json, kind, topic, status, created_day, schedule_json)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?8)",
                    params![
                        idea.id,
                        idea.title,
                        idea.description,
                        includes,
                        idea.kind.as_str(),
                        idea.topic,
                        idea.created_day,
                        schedule,
                    ],
                )
                .map_err(|e| err("IDEA_MINT", e.to_string()))?;
            Ok(n == 1)
        }

        pub fn get(&self, id: &str) -> Result<Option<Idea>, PantheonError> {
            let conn = self.lock()?;
            conn.query_row(
                "SELECT id, title, description, includes_json, kind, topic, status,
                        created_day, schedule_json, more, less, decided_ms
                 FROM ideas WHERE id = ?1",
                [id],
                row_to_idea,
            )
            .optional()
            .map_err(|e| err("IDEA_GET", e.to_string()))
        }

        /// All ideas, newest day first.
        pub fn list(&self) -> Result<Vec<Idea>, PantheonError> {
            let conn = self.lock()?;
            let mut stmt = conn
                .prepare(
                    "SELECT id, title, description, includes_json, kind, topic, status,
                            created_day, schedule_json, more, less, decided_ms
                     FROM ideas ORDER BY created_day DESC, id ASC",
                )
                .map_err(|e| err("IDEA_LIST", e.to_string()))?;
            let ideas = stmt
                .query_map([], row_to_idea)
                .map_err(|e| err("IDEA_LIST", e.to_string()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| err("IDEA_LIST", e.to_string()))?;
            Ok(ideas)
        }

        /// `true` when at least one idea was minted on `day` (`YYYY-MM-DD`).
        pub fn has_day(&self, day: &str) -> Result<bool, PantheonError> {
            let conn = self.lock()?;
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM ideas WHERE created_day = ?1",
                    [day],
                    |r| r.get(0),
                )
                .map_err(|e| err("IDEA_DAY", e.to_string()))?;
            Ok(n > 0)
        }

        /// `true` when a `pending` idea already carries this topic: generation
        /// must not pile up duplicates for a topic the user hasn't answered.
        pub fn pending_topic_exists(&self, topic: &str) -> Result<bool, PantheonError> {
            let conn = self.lock()?;
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM ideas WHERE topic = ?1 AND status = 'pending'",
                    [topic],
                    |r| r.get(0),
                )
                .map_err(|e| err("IDEA_TOPIC", e.to_string()))?;
            Ok(n > 0)
        }

        /// Set the status. Returns `false` when the id does not exist.
        /// `decided_ms` stamps when the idea left `pending`.
        ///
        /// Transition rules (the documented lifecycle: the dashboard only
        /// accepts/dismisses `pending` ideas, and every non-pending status
        /// is terminal except `Accepted -> Done`):
        /// - `Pending` may move to `Accepted`, `Dismissed`, or `Done`
        ///   (direct launch: a flow that accepts and runs/publishes the
        ///   idea in one step lands it in `Done` without an intermediate
        ///   `Accepted` record);
        /// - `Accepted` may move to `Done` (the work the accept spawned
        ///   ran/published through the ideas flow, so the idea is done);
        /// - re-setting the current status is a no-op;
        /// - any other move is a hard `IDEA_STATE` error, not a silent
        ///   rewrite: `Dismissed` and `Done` are terminal, and nothing
        ///   moves backwards to `Pending` or `Accepted`.
        pub fn set_status(
            &self,
            id: &str,
            status: IdeaStatus,
            decided_ms: Option<i64>,
        ) -> Result<bool, PantheonError> {
            let conn = self.lock()?;
            let current: Option<String> = conn
                .query_row("SELECT status FROM ideas WHERE id = ?1", [id], |r| r.get(0))
                .optional()
                .map_err(|e| err("IDEA_STATUS_SET", e.to_string()))?;
            let Some(current) = current else {
                return Ok(false);
            };
            let current =
                IdeaStatus::parse(&current).map_err(|e| err("IDEA_STATUS_SET", e.to_string()))?;
            let allowed = current == status
                || (current == IdeaStatus::Pending
                    && matches!(
                        status,
                        IdeaStatus::Accepted | IdeaStatus::Dismissed | IdeaStatus::Done
                    ))
                || (current == IdeaStatus::Accepted && status == IdeaStatus::Done);
            if !allowed {
                return Err(err(
                    "IDEA_STATE",
                    format!(
                        "idea {id} is {} (terminal); cannot move it to {}",
                        current.as_str(),
                        status.as_str()
                    ),
                ));
            }
            let n = conn
                .execute(
                    "UPDATE ideas SET status = ?1, decided_ms = ?2 WHERE id = ?3",
                    params![status.as_str(), decided_ms, id],
                )
                .map_err(|e| err("IDEA_STATUS_SET", e.to_string()))?;
            Ok(n == 1)
        }

        /// Record explicit `more`/`less` feedback on the idea and its topic.
        /// Returns `false` when the id does not exist. The idea-counter
        /// bump and the topic-counter bump run in one transaction: the old
        /// code updated the idea, then the topic, so a failure between them
        /// left the counters permanently diverged.
        pub fn record_feedback(
            &self,
            id: &str,
            signal: FeedbackSignal,
        ) -> Result<bool, PantheonError> {
            let mut conn = self.lock()?;
            let tx = conn
                .transaction()
                .map_err(|e| err("IDEA_FEEDBACK", e.to_string()))?;
            let col = match signal {
                FeedbackSignal::More => "more",
                FeedbackSignal::Less => "less",
            };
            let n = tx
                .execute(
                    &format!("UPDATE ideas SET {col} = {col} + 1 WHERE id = ?1"),
                    [id],
                )
                .map_err(|e| err("IDEA_FEEDBACK", e.to_string()))?;
            if n != 1 {
                return Ok(false);
            }
            let topic: String = tx
                .query_row("SELECT topic FROM ideas WHERE id = ?1", [id], |r| r.get(0))
                .map_err(|e| err("IDEA_FEEDBACK", e.to_string()))?;
            if !topic.is_empty() {
                tx.execute(
                    &format!(
                        "INSERT INTO idea_topic_signals (topic, {col}) VALUES (?1, 1)
                         ON CONFLICT(topic) DO UPDATE SET {col} = {col} + 1"
                    ),
                    [topic],
                )
                .map_err(|e| err("IDEA_FEEDBACK", e.to_string()))?;
            }
            tx.commit()
                .map_err(|e| err("IDEA_FEEDBACK", e.to_string()))?;
            Ok(true)
        }

        /// Delete `pending` ideas created before `day` (`YYYY-MM-DD`,
        /// exclusive). Returns the number removed.
        pub fn expire_pending_before(&self, day: &str) -> Result<usize, PantheonError> {
            let conn = self.lock()?;
            let n = conn
                .execute(
                    "DELETE FROM ideas WHERE status = 'pending' AND created_day < ?1",
                    [day],
                )
                .map_err(|e| err("IDEA_EXPIRE", e.to_string()))?;
            Ok(n)
        }

        /// Feedback counters for a topic. Missing topic = all zeros.
        pub fn topic_signals(&self, topic: &str) -> Result<TopicSignals, PantheonError> {
            let conn = self.lock()?;
            conn.query_row(
                "SELECT accepted, dismissed, more, less FROM idea_topic_signals WHERE topic = ?1",
                [topic],
                |r| {
                    Ok(TopicSignals {
                        accepted: r.get(0)?,
                        dismissed: r.get(1)?,
                        more: r.get(2)?,
                        less: r.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(|e| err("IDEA_TOPIC_SIG", e.to_string()))
            .map(|o| o.unwrap_or_default())
        }

        /// Bump the accept or dismiss counter for a topic.
        pub fn bump_topic(&self, topic: &str, accepted: bool) -> Result<(), PantheonError> {
            if topic.is_empty() {
                return Ok(());
            }
            let col = if accepted { "accepted" } else { "dismissed" };
            let conn = self.lock()?;
            conn.execute(
                &format!(
                    "INSERT INTO idea_topic_signals (topic, {col}) VALUES (?1, 1)
                     ON CONFLICT(topic) DO UPDATE SET {col} = {col} + 1"
                ),
                [topic],
            )
            .map_err(|e| err("IDEA_TOPIC_BUMP", e.to_string()))?;
            Ok(())
        }
    }

    fn row_to_idea(r: &rusqlite::Row<'_>) -> rusqlite::Result<Idea> {
        let includes_json: String = r.get(3)?;
        // Corrupt JSON is a hard error, like corrupt kind/status below:
        // the old `unwrap_or_default` silently turned a damaged includes
        // list into an empty plan, so a broken idea looked intentionally
        // empty instead of broken.
        let includes: Vec<String> = serde_json::from_str(&includes_json).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e))
        })?;
        let kind: String = r.get(4)?;
        let status: String = r.get(6)?;
        let schedule_json: Option<String> = r.get(8)?;
        let schedule = schedule_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    8,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
        // Kind/status strings are written by this store; a corrupt row is a
        // hard error, not a silent default.
        let kind = IdeaKind::parse(&kind).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
        })?;
        let status = IdeaStatus::parse(&status).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
        })?;
        Ok(Idea {
            id: r.get(0)?,
            title: r.get(1)?,
            description: r.get(2)?,
            includes,
            kind,
            topic: r.get(5)?,
            status,
            created_day: r.get(7)?,
            schedule,
            more: r.get(9)?,
            less: r.get(10)?,
            decided_ms: r.get(11)?,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn new_idea(id: &str) -> NewIdea {
            NewIdea {
                id: id.to_string(),
                title: "Test idea".to_string(),
                description: "desc".to_string(),
                includes: vec!["step one".to_string()],
                kind: IdeaKind::General,
                topic: "test-topic".to_string(),
                created_day: "2026-10-28".to_string(),
                schedule: None,
            }
        }

        fn tmp_store() -> (IdeaStore, tempfile::TempDir) {
            let dir = tempfile::TempDir::new().expect("temp dir");
            let store = IdeaStore::open(dir.path()).expect("ideas store");
            (store, dir)
        }

        #[test]
        fn record_feedback_bumps_idea_and_topic_together() {
            let (store, _dir) = tmp_store();
            assert!(store.mint(&new_idea("i1")).unwrap());
            assert!(store.record_feedback("i1", FeedbackSignal::More).unwrap());
            let idea = store.get("i1").unwrap().unwrap();
            assert_eq!(idea.more, 1);
            assert_eq!(store.topic_signals("test-topic").unwrap().more, 1);
            assert!(!store
                .record_feedback("missing", FeedbackSignal::More)
                .unwrap());
        }

        #[test]
        fn set_status_enforces_the_documented_lifecycle() {
            let (store, _dir) = tmp_store();
            assert!(store.mint(&new_idea("i1")).unwrap());
            // Pending -> Accepted is the documented dashboard path.
            assert!(store
                .set_status("i1", IdeaStatus::Accepted, Some(1))
                .unwrap());
            assert_eq!(
                store.get("i1").unwrap().unwrap().status,
                IdeaStatus::Accepted
            );
            // Terminal states have no outgoing transitions.
            let err = store
                .set_status("i1", IdeaStatus::Dismissed, Some(2))
                .expect_err("accepted -> dismissed must fail");
            assert_eq!(err.code, "IDEA_STATE");
            // Accepted -> Done is the completion transition: the work the
            // accept spawned ran/published through the ideas flow.
            assert!(store.set_status("i1", IdeaStatus::Done, Some(3)).unwrap());
            assert_eq!(store.get("i1").unwrap().unwrap().status, IdeaStatus::Done);
            // Done is terminal: nothing leaves it.
            let err = store
                .set_status("i1", IdeaStatus::Accepted, Some(4))
                .expect_err("done -> accepted must fail");
            assert_eq!(err.code, "IDEA_STATE");
            // Pending -> Done is the direct-launch transition.
            assert!(store.mint(&new_idea("i2")).unwrap());
            assert!(store.set_status("i2", IdeaStatus::Done, Some(5)).unwrap());
            // Dismissed is terminal too.
            assert!(store.mint(&new_idea("i3")).unwrap());
            assert!(store
                .set_status("i3", IdeaStatus::Dismissed, Some(6))
                .unwrap());
            let err = store
                .set_status("i3", IdeaStatus::Done, Some(7))
                .expect_err("dismissed -> done must fail");
            assert_eq!(err.code, "IDEA_STATE");
            // Re-setting the current status is a no-op, not an error.
            assert!(store.set_status("i1", IdeaStatus::Done, Some(8)).unwrap());
            // Unknown id is still Ok(false), not an error.
            assert!(!store
                .set_status("nope", IdeaStatus::Accepted, None)
                .unwrap());
        }

        #[test]
        fn corrupt_includes_json_is_a_hard_error_not_an_empty_plan() {
            let (store, dir) = tmp_store();
            assert!(store.mint(&new_idea("i1")).unwrap());
            // Corrupt the row behind the store's back, like a damaged DB.
            let conn = Connection::open(dir.path().join("ideas.db")).expect("raw conn");
            conn.execute(
                "UPDATE ideas SET includes_json = 'not json' WHERE id = 'i1'",
                [],
            )
            .unwrap();
            drop(conn);
            assert!(
                store.get("i1").is_err(),
                "corrupt includes_json must error like corrupt kind/status"
            );
        }
    }
}

pub mod search {
    //! Hybrid session search: an additive FTS5 sidecar next to the event
    //! ledger, exposed to the model as the `session_search` tool.
    //!
    //! FTS5 is the primary retrieval layer (exact identifiers, tool names,
    //! model ids, quoted phrases). Chunks are indexed as events are emitted
    //! new messages chunk-and-index forward-only; old chunks never change
    //! unless deliberately re-indexed. Embeddings are a secondary recall
    //! layer, intentionally deferred until an embedding-capable provider is
    //! wired: the schema and scoring seam exist, the default lexical path
    //! works without them.
    //!
    //! Additive by design: nothing here changes run/event handling.

    use pantheon_api::error::{Layer, PantheonError};
    use pantheon_api::logging::now_ms;
    use rusqlite::{params, Connection};
    use std::path::Path;
    use std::sync::Mutex;

    const SCHEMA_TABLES: &str = "
    CREATE TABLE IF NOT EXISTS session_chunks (
      chunk_id TEXT PRIMARY KEY,
      run_id TEXT NOT NULL,
      seq INTEGER NOT NULL,
      kind TEXT NOT NULL,
      text TEXT NOT NULL,
      ts_ms INTEGER NOT NULL,
      embedding BLOB
    );
    CREATE VIRTUAL TABLE IF NOT EXISTS session_fts USING fts5(
      chunk_id UNINDEXED,
      text,
      kind UNINDEXED,
      run_id UNINDEXED,
      tokenize = \'unicode61\'
    );
    ";

    const SCHEMA_INDEXES: &str = "
    CREATE INDEX IF NOT EXISTS idx_session_chunks_emb ON session_chunks(run_id) WHERE embedding IS NOT NULL;
    CREATE INDEX IF NOT EXISTS idx_session_chunks_run ON session_chunks(run_id, seq);
    ";

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

    /// One searchable chunk. Mirrors the ledger\'s event rows so a hit points
    /// straight back at the source event (`run_id`, `seq`).
    #[derive(Debug, Clone)]
    pub struct SessionChunk {
        pub chunk_id: String,
        pub run_id: String,
        pub seq: i64,
        /// `message`, `tool`, or `title`.
        pub kind: String,
        pub text: String,
        pub ts_ms: i64,
    }

    /// One search hit: the chunk that matched plus its run context.
    #[derive(Debug, Clone)]
    pub struct SearchHit {
        pub chunk: SessionChunk,
        /// Hybrid score: 0.60 lexical + 0.10 recency (embeddings 0.30 until a
        /// vector backend is wired, then the weights rebalance).
        pub score: f64,
        /// Lexical rank within this result set (0 = best).
        pub lexical_rank: usize,
        /// Raw SQLite bm25 rank (negative-better). Kept for debugging.
        _bm25: f64,
    }

    /// Serialize an optional embedding as little-endian f32 bytes. `None`
    /// and empty both store SQL NULL.
    fn embed_to_blob(e: Option<&[f32]>) -> Option<Vec<u8>> {
        match e {
            None | Some(&[]) => None,
            Some(v) => Some(v.iter().flat_map(|f| f.to_le_bytes()).collect()),
        }
    }

    /// Deserialize a stored embedding blob back to f32s.
    pub fn blob_to_embedding(b: Option<Vec<u8>>) -> Option<Vec<f32>> {
        let b = b?;
        if b.is_empty() || !b.len().is_multiple_of(4) {
            return None;
        }
        Some(
            b.as_chunks::<4>()
                .0
                .iter()
                .map(|c| f32::from_le_bytes(*c))
                .collect(),
        )
    }

    /// Cosine similarity between two vectors (storage-local copy; the
    /// provider crate has its own for the client side).
    pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
        if a.len() != b.len() || a.is_empty() {
            return 0.0;
        }
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        if na == 0.0 || nb == 0.0 {
            return 0.0;
        }
        dot / (na * nb)
    }

    /// Max chars of tool output indexed per chunk - tool results can be huge
    /// and would drown the lexical index (other local indexes make the same tradeoff at 8192).
    const TOOL_CHUNK_MAX: usize = 8192;
    /// Max chars of a message indexed per chunk.
    const MESSAGE_CHUNK_MAX: usize = 4096;
    /// Cap on the semantic-scan leg of `search_hybrid` (most recent
    /// embedded chunks). Without a cap every hybrid query loads the whole
    /// embedding table; 512 chunks is ample for a top-`limit` blend while
    /// keeping per-query work bounded.
    const SEMANTIC_SCAN_CAP: usize = 512;

    /// SQLite-persisted session search index.
    ///
    /// Owns a separate connection (and lock) to the same ledger file. Indexing
    /// writes are committed independently of any ledger write transaction
    /// there is deliberately no cross-connection transaction coupling here, so
    /// an indexing failure can never roll back or block a run's event append.
    pub struct SessionSearch {
        conn: Mutex<Connection>,
    }

    impl SessionSearch {
        /// Open (or create) the search sidecar. `path` is the ledger\'s own
        /// file - the schema lives in the same DB so a hit joins the run.
        pub fn open(path: &Path) -> Result<Self, PantheonError> {
            let conn = Connection::open(path).map_err(|e| err("SEARCH_OPEN", e.to_string()))?;
            crate::configure_durability(&conn, "SEARCH")?;
            // Tables first, then the forward-only migration (ledgers created
            // before the vector layer lack the embedding column), then the
            // indexes - the partial index requires the column to exist, so it
            // must run after the migration, not in the same batch. Fresh DBs
            // already have the column; the ALTER is a no-op error there.
            conn.execute_batch(SCHEMA_TABLES)
                .map_err(|e| err("SEARCH_SCHEMA", e.to_string()))?;
            // The forward-only migration (ledgers created before the vector
            // layer lack the embedding column). Only "duplicate column" is
            // a benign no-op (fresh DBs already have the column); any other
            // ALTER failure used to be swallowed with `let _ =`, leaving a
            // sidecar that looks open but cannot store vectors.
            match conn.execute("ALTER TABLE session_chunks ADD COLUMN embedding BLOB", []) {
                Ok(_) => {}
                Err(e) if e.to_string().contains("duplicate column name") => {}
                Err(e) => return Err(err("SEARCH_SCHEMA", e.to_string())),
            }
            conn.execute_batch(SCHEMA_INDEXES)
                .map_err(|e| err("SEARCH_SCHEMA", e.to_string()))?;
            Ok(Self {
                conn: Mutex::new(conn),
            })
        }

        pub fn open_in_memory() -> Result<Self, PantheonError> {
            let conn =
                Connection::open_in_memory().map_err(|e| err("SEARCH_OPEN", e.to_string()))?;
            crate::configure_durability(&conn, "SEARCH")?;
            conn.execute_batch(SCHEMA_TABLES)
                .map_err(|e| err("SEARCH_SCHEMA", e.to_string()))?;
            conn.execute_batch(SCHEMA_INDEXES)
                .map_err(|e| err("SEARCH_SCHEMA", e.to_string()))?;
            Ok(Self {
                conn: Mutex::new(conn),
            })
        }

        /// Index one chunk without an embedding (lexical-only recall).
        pub fn index(&self, chunk: &SessionChunk) -> Result<(), PantheonError> {
            self.index_with_embedding(chunk, None)
        }

        /// Index one chunk. Idempotent per chunk_id (INSERT OR REPLACE) so a
        /// re-indexed run converges instead of duplicating rows. `embedding`
        /// is `None` when no embeddings auxiliary is configured - the chunk
        /// still lexically searchable; the vector layer just skips it.
        pub fn index_with_embedding(
            &self,
            chunk: &SessionChunk,
            embedding: Option<&[f32]>,
        ) -> Result<(), PantheonError> {
            let text = match chunk.kind.as_str() {
                "tool" => chunk.text.chars().take(TOOL_CHUNK_MAX).collect::<String>(),
                _ => chunk
                    .text
                    .chars()
                    .take(MESSAGE_CHUNK_MAX)
                    .collect::<String>(),
            };
            let mut conn = self
                .conn
                .lock()
                .map_err(|e| err("SEARCH_LOCK", e.to_string()))?;
            // One transaction for all three writes: chunk insert, FTS
            // delete, FTS insert. As separate statements a concurrent
            // re-index could interleave between the delete and the insert
            // and leave duplicate FTS rows for one chunk (or a chunk with
            // no FTS row at all); the transaction makes re-indexing atomic.
            let tx = conn
                .transaction()
                .map_err(|e| err("SEARCH_INSERT", e.to_string()))?;
            tx.execute(
                "INSERT OR REPLACE INTO session_chunks (chunk_id, run_id, seq, kind, text, ts_ms, embedding) VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![
                    chunk.chunk_id,
                    chunk.run_id,
                    chunk.seq,
                    chunk.kind,
                    text,
                    chunk.ts_ms,
                    embed_to_blob(embedding)
                ],
            )
            .map_err(|e| err("SEARCH_INSERT", e.to_string()))?;
            // `session_fts` is a standalone FTS5 table, so it has no unique
            // constraint and `INSERT OR REPLACE` degenerates to a plain INSERT:
            // the row would append on every re-index while `session_chunks`
            // converged, and `search`'s join would then return the same chunk once
            // per leaked FTS row. Delete the chunk's FTS rows first so re-indexing
            // is genuinely idempotent, which is what the `index` doc promises.
            tx.execute(
                "DELETE FROM session_fts WHERE chunk_id = ?1",
                params![chunk.chunk_id],
            )
            .map_err(|e| err("SEARCH_FTS_DELETE", e.to_string()))?;
            tx.execute(
                "INSERT INTO session_fts (chunk_id, text, kind, run_id) VALUES (?1,?2,?3,?4)",
                params![chunk.chunk_id, text, chunk.kind, chunk.run_id],
            )
            .map_err(|e| err("SEARCH_FTS_INSERT", e.to_string()))?;
            tx.commit()
                .map_err(|e| err("SEARCH_INSERT", e.to_string()))?;
            Ok(())
        }

        /// Drop chunks (and their FTS rows) older than `cutoff_ts_ms`
        /// (retention policy). Pairs with `Ledger::prune_events_before`: the FTS
        /// sidecar must not retain searchable text for history the ledger has
        /// already pruned. Returns the number of chunks pruned.
        pub fn prune_before(&self, cutoff_ts_ms: i64) -> Result<usize, PantheonError> {
            let conn = self
                .conn
                .lock()
                .map_err(|e| err("SEARCH_LOCK", e.to_string()))?;
            // FTS first: the subquery reads `session_chunks`, so the FTS rows
            // must be resolved before the chunks themselves are deleted.
            conn.execute(
                "DELETE FROM session_fts WHERE chunk_id IN \
                 (SELECT chunk_id FROM session_chunks WHERE ts_ms < ?1)",
                params![cutoff_ts_ms],
            )
            .map_err(|e| err("SEARCH_FTS_DELETE", e.to_string()))?;
            let pruned = conn
                .execute(
                    "DELETE FROM session_chunks WHERE ts_ms < ?1",
                    params![cutoff_ts_ms],
                )
                .map_err(|e| err("SEARCH_DELETE", e.to_string()))?;
            Ok(pruned)
        }

        /// Lexical search over the FTS index, ranked by BM25, then re-scored
        /// with a recency term. `query` is a raw user string; special FTS
        /// characters are neutralised so a query like `TUI session "block"`
        /// or `9f3a2` still matches.
        pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, PantheonError> {
            let conn = self
                .conn
                .lock()
                .map_err(|e| err("SEARCH_LOCK", e.to_string()))?;
            // Neutralise FTS5 operators so user text is treated as plain terms.
            let cleaned: String = query
                .chars()
                .map(|c| if "\"(){}[]:^*|,-".contains(c) { ' ' } else { c })
                .collect();
            let trimmed = cleaned.trim();
            if trimmed.is_empty() {
                return Ok(Vec::new());
            }
            // Quote each term (operators/hyphens stay literal) and OR them:
            // recall-first, because BM25 ranking still puts the chunk that
            // matches most terms on top. Trailing * gives prefix matching so
            // 'websocket' finds 'websockets' without the user stemming.
            let fts_query = trimmed
                .split_whitespace()
                .map(|t| format!("\"{}\"*", t.replace('"', "")))
                .collect::<Vec<_>>()
                .join(" OR ");

            let now = now_ms();
            let mut stmt = conn
                .prepare(
                    "SELECT c.chunk_id, c.run_id, c.seq, c.kind, c.text, c.ts_ms,
                     bm25(session_fts) AS rank
                     FROM session_fts f
                     JOIN session_chunks c ON c.chunk_id = f.chunk_id
                     WHERE session_fts MATCH ?1
                     ORDER BY rank LIMIT ?2",
                )
                .map_err(|e| err("SEARCH_QUERY", e.to_string()))?;
            let rows = stmt
                .query_map(params![fts_query, limit as i64], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, f64>(6)?,
                    ))
                })
                .map_err(|e| err("SEARCH_QUERY", e.to_string()))?;

            let mut hits: Vec<SearchHit> = Vec::new();
            for row in rows {
                let (chunk_id, run_id, seq, kind, text, ts_ms, bm25) =
                    row.map_err(|e| err("SEARCH_ROW", e.to_string()))?;
                hits.push(SearchHit {
                    chunk: SessionChunk {
                        chunk_id,
                        run_id,
                        seq,
                        kind,
                        text,
                        ts_ms,
                    },
                    score: 0.0,
                    lexical_rank: 0,
                    _bm25: bm25,
                });
            }
            // Hybrid scoring: lexical dominates, recency breaks ties. BM25 is
            // negative-better in SQLite; normalise to 0..1 by rank position.
            let n = hits.len();
            for (i, h) in hits.iter_mut().enumerate() {
                let lexical = 1.0 - (i as f64 / n.max(1) as f64);
                let age_ms = (now - h.chunk.ts_ms).max(0) as f64;
                let recency = 1.0 / (1.0 + age_ms / 3_600_000.0); // 1h half-life-ish
                h.lexical_rank = i;
                h.score = 0.60 * lexical + 0.10 * recency;
            }
            hits.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            Ok(hits)
        }

        /// Hybrid search: lexical (BM25) + semantic (cosine over stored
        /// embeddings) + recency. `query_embedding` is `None` when no
        /// embeddings auxiliary is configured, collapsing to the lexical +
        /// recency path. Weights: 0.60 lexical / 0.30 semantic / 0.10 recency.
        pub fn search_hybrid(
            &self,
            query: &str,
            query_embedding: Option<&[f32]>,
            limit: usize,
        ) -> Result<Vec<SearchHit>, PantheonError> {
            let lexical = self.search(query, limit.saturating_mul(4).max(limit))?;
            let Some(qv) = query_embedding else {
                // No vector backend: renormalise without the semantic term so
                // scores stay comparable (0.60/0.10 spread over 0.70).
                return Ok(lexical
                    .into_iter()
                    .map(|mut h| {
                        h.score = (h.score / 0.70 * 0.90).min(1.0);
                        h
                    })
                    .take(limit)
                    .collect());
            };

            // Semantic scan, bounded: the old code loaded the ENTIRE
            // embedding table per query (text + vectors for every chunk),
            // which grows with the whole history. Now it loads only (a) the
            // embeddings of the lexical candidates, so a dual lexical+
            // semantic hit still blends, and (b) the most recent
            // `SEMANTIC_SCAN_CAP` embedded chunks, so semantic recall still
            // finds chunks the keyword query missed without scanning history.
            let conn = self
                .conn
                .lock()
                .map_err(|e| err("SEARCH_LOCK", e.to_string()))?;
            let mut rows: Vec<(String, String, i64, String, String, i64, Option<Vec<u8>>)> =
                Vec::new();
            {
                // (a) lexical candidates, by id.
                let ids: Vec<&str> = lexical.iter().map(|h| h.chunk.chunk_id.as_str()).collect();
                if !ids.is_empty() {
                    let placeholders: Vec<String> =
                        (0..ids.len()).map(|i| format!("?{}", i + 1)).collect();
                    let sql = format!(
                        "SELECT chunk_id, run_id, seq, kind, text, ts_ms, embedding \
                         FROM session_chunks WHERE chunk_id IN ({}) AND embedding IS NOT NULL",
                        placeholders.join(",")
                    );
                    let mut stmt = conn
                        .prepare(&sql)
                        .map_err(|e| err("SEARCH_QUERY", e.to_string()))?;
                    let mapped = stmt
                        .query_map(rusqlite::params_from_iter(ids.iter()), |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, i64>(2)?,
                                r.get::<_, String>(3)?,
                                r.get::<_, String>(4)?,
                                r.get::<_, i64>(5)?,
                                r.get::<_, Option<Vec<u8>>>(6)?,
                            ))
                        })
                        .map_err(|e| err("SEARCH_QUERY", e.to_string()))?;
                    for row in mapped {
                        rows.push(row.map_err(|e| err("SEARCH_ROW", e.to_string()))?);
                    }
                }
                // (b) recent embedded chunks for semantic-only recall.
                let mut stmt = conn
                    .prepare(
                        "SELECT chunk_id, run_id, seq, kind, text, ts_ms, embedding \
                         FROM session_chunks WHERE embedding IS NOT NULL \
                         ORDER BY ts_ms DESC, seq DESC LIMIT ?1",
                    )
                    .map_err(|e| err("SEARCH_QUERY", e.to_string()))?;
                let mapped = stmt
                    .query_map(params![SEMANTIC_SCAN_CAP as i64], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, String>(4)?,
                            r.get::<_, i64>(5)?,
                            r.get::<_, Option<Vec<u8>>>(6)?,
                        ))
                    })
                    .map_err(|e| err("SEARCH_QUERY", e.to_string()))?;
                for row in mapped {
                    rows.push(row.map_err(|e| err("SEARCH_ROW", e.to_string()))?);
                }
            }
            let now = now_ms();
            let mut semantic_hits: Vec<(SessionChunk, f32)> = Vec::new();
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for (chunk_id, run_id, seq, kind, text, ts_ms, emb) in rows {
                if !seen.insert(chunk_id.clone()) {
                    continue;
                }
                let Some(emb) = blob_to_embedding(emb) else {
                    continue;
                };
                let sim = cosine(qv, &emb);
                if sim > 0.05 {
                    semantic_hits.push((
                        SessionChunk {
                            chunk_id,
                            run_id,
                            seq,
                            kind,
                            text,
                            ts_ms,
                        },
                        sim,
                    ));
                }
            }
            semantic_hits
                .sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

            // Blend: every lexical hit carries its BM25 rank; every semantic
            // hit carries its cosine. Union by chunk_id, best component wins.
            let mut by_id: std::collections::HashMap<String, (f64, SessionChunk)> =
                std::collections::HashMap::new();
            for h in &lexical {
                let score = h.score; // already lexical+recency, 0..0.70
                by_id
                    .entry(h.chunk.chunk_id.clone())
                    .or_insert((score, h.chunk.clone()));
            }
            let sem_max = semantic_hits.first().map(|(_, s)| *s).unwrap_or(1.0);
            for (chunk, sim) in semantic_hits.iter().take(limit) {
                let semantic = (sim / sem_max) as f64;
                let age_ms = (now - chunk.ts_ms).max(0) as f64;
                let recency = 1.0 / (1.0 + age_ms / 3_600_000.0);
                let score = 0.30 * semantic + 0.10 * recency;
                by_id
                    .entry(chunk.chunk_id.clone())
                    .and_modify(|(s, _)| *s += score)
                    .or_insert((score, chunk.clone()));
            }
            let mut out: Vec<SearchHit> = by_id
                .into_iter()
                .map(|(chunk_id, (score, chunk))| SearchHit {
                    chunk: SessionChunk { chunk_id, ..chunk },
                    // A dual lexical+semantic hit sums to 0.70 + 0.30 + 0.10
                    // = 1.10 without this clamp; scores are defined as 0..1.
                    score: score.min(1.0),
                    lexical_rank: 0,
                    _bm25: 0.0,
                })
                .collect();
            out.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            out.truncate(limit);
            Ok(out)
        }

        /// Count indexed chunks (for the doctor / status surface).
        pub fn len(&self) -> Result<usize, PantheonError> {
            let conn = self
                .conn
                .lock()
                .map_err(|e| err("SEARCH_LOCK", e.to_string()))?;
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM session_chunks", [], |r| r.get(0))
                .map_err(|e| err("SEARCH_COUNT", e.to_string()))?;
            Ok(n as usize)
        }

        pub fn is_empty(&self) -> Result<bool, PantheonError> {
            Ok(self.len()? == 0)
        }
    }

    #[cfg(test)]
    mod tests {
        //! Fresh coverage for the `session_search` sidecar's core path: FTS
        //! indexing + lexical search over temp-dir file DBs.
        //!
        //! (A sibling module in `ledger.rs` covers `delete_run`'s FTS cleanup from
        //! the ledger side; the no-ghost-hits case here asserts it from the search
        //! side.)
        use super::*;
        use crate::ledger::Ledger;
        use pantheon_api::events::Event;
        use tempfile::TempDir;

        fn file_search() -> (SessionSearch, TempDir) {
            let dir = TempDir::new().expect("temp dir");
            let db = dir.path().join("ledger.db");
            (SessionSearch::open(&db).expect("search sidecar"), dir)
        }

        fn chunk(run_id: &str, id: &str, seq: i64, text: &str) -> SessionChunk {
            SessionChunk {
                chunk_id: id.into(),
                run_id: run_id.into(),
                seq,
                kind: "message".into(),
                text: text.into(),
                ts_ms: 1_700_000_000_000,
            }
        }

        #[test]
        fn search_returns_the_chunks_whose_text_matches() {
            let (s, _dir) = file_search();
            s.index(&chunk(
                "run_1",
                "c1",
                1,
                "the pantry stocks alkaline batteries",
            ))
            .unwrap();
            s.index(&chunk(
                "run_1",
                "c2",
                2,
                "debugging the websocket handshake failure",
            ))
            .unwrap();
            s.index(&chunk(
                "run_1",
                "c3",
                3,
                "a quiet evening with no incidents",
            ))
            .unwrap();

            let hits = s.search("websocket handshake", 10).unwrap();
            assert_eq!(hits.len(), 1, "only the matching chunk should hit");
            assert_eq!(hits[0].chunk.chunk_id, "c2");
            assert_eq!(hits[0].chunk.run_id, "run_1");
            assert_eq!(hits[0].chunk.seq, 2);
            assert_eq!(hits[0].lexical_rank, 0);

            // A term indexed nowhere returns nothing, not an error.
            assert!(s.search("xylophone", 10).unwrap().is_empty());
        }

        #[test]
        fn search_returns_no_ghost_hits_for_deleted_runs() {
            let dir = TempDir::new().expect("temp dir");
            let db = dir.path().join("ledger.db");
            let ledger = Ledger::open(&db).expect("ledger");
            let search = SessionSearch::open(&db).expect("search");
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
                .index(&chunk(
                    "run-a",
                    "a1",
                    1,
                    "deploying the canary to production",
                ))
                .unwrap();
            search
                .index(&chunk(
                    "run-b",
                    "b1",
                    1,
                    "canary analysis shows no regression",
                ))
                .unwrap();
            assert_eq!(search.search("canary", 10).unwrap().len(), 2);

            ledger.delete_run("run-a").unwrap();

            let hits = search.search("canary", 10).unwrap();
            assert_eq!(hits.len(), 1, "deleted run must leave no searchable rows");
            assert_eq!(hits[0].chunk.run_id, "run-b");
            // The deleted run's own unique term is gone too.
            assert!(search.search("deploying", 10).unwrap().is_empty());
        }

        #[test]
        fn search_across_runs_returns_only_matching_runs() {
            let (s, _dir) = file_search();
            s.index(&chunk(
                "run-a",
                "a1",
                1,
                "quantum tunneling in the diode model",
            ))
            .unwrap();
            s.index(&chunk(
                "run-b",
                "b1",
                1,
                "nebula photography with the new lens",
            ))
            .unwrap();
            s.index(&chunk(
                "run-c",
                "c1",
                1,
                "quantum error correction thresholds",
            ))
            .unwrap();
            s.index(&chunk("run-b", "b2", 2, "calibrating the nebula timer"))
                .unwrap();

            let hits = s.search("quantum", 10).unwrap();
            assert_eq!(hits.len(), 2);
            let mut runs: Vec<&str> = hits.iter().map(|h| h.chunk.run_id.as_str()).collect();
            runs.sort_unstable();
            assert_eq!(runs, ["run-a", "run-c"]);
            assert!(hits.iter().all(|h| h.chunk.text.contains("quantum")));
        }

        #[test]
        fn hybrid_dual_hit_score_is_clamped_to_one() {
            let (s, _dir) = file_search();
            // Fresh timestamp so recency is ~1: lexical 0.70 + semantic 0.40
            // would sum to 1.10 without the clamp.
            let mut c = chunk("run-a", "c1", 1, "the quick brown fox");
            c.ts_ms = now_ms();
            let vec = vec![1.0_f32, 0.0, 0.0];
            s.index_with_embedding(&c, Some(&vec)).unwrap();
            let hits = s.search_hybrid("quick", Some(&vec), 10).unwrap();
            assert_eq!(hits.len(), 1);
            assert!(
                hits[0].score <= 1.0,
                "hybrid score must never exceed 1.0, got {}",
                hits[0].score
            );
            assert_eq!(hits[0].score, 1.0, "dual hit saturates at 1.0, not above");
        }

        #[test]
        fn hybrid_semantic_recall_finds_chunks_the_keywords_miss() {
            let (s, _dir) = file_search();
            let vec = vec![0.0_f32, 1.0, 0.0];
            s.index_with_embedding(
                &chunk("run-a", "c1", 1, "totally unrelated words"),
                Some(&vec),
            )
            .unwrap();
            // No lexical match for "zebra", but the embedding matches exactly.
            let hits = s.search_hybrid("zebra", Some(&vec), 10).unwrap();
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].chunk.chunk_id, "c1");
        }

        #[test]
        fn reindexing_a_chunk_does_not_duplicate_fts_rows() {
            let (s, _dir) = file_search();
            let c = chunk("run-a", "c1", 1, "the quick brown fox");
            s.index(&c).unwrap();
            s.index(&c).unwrap();
            s.index(&c).unwrap();
            let hits = s.search("quick", 10).unwrap();
            assert_eq!(
                hits.len(),
                1,
                "re-index must stay idempotent under the transaction"
            );
        }

        // ---------------------------------------------------------------- rebuild

        /// The repair path used to recreate an empty FTS table, leaving every
        /// previously indexed run silently unsearchable. These pin that the rebuild
        /// restores the actual contents from the durable chunk store.
        #[test]
        fn recreate_restores_searchable_contents() {
            let (s, dir) = file_search();
            let db = dir.path().join("ledger.db");
            s.index(&chunk(
                "run_1",
                "c1",
                1,
                "the pantry stocks alkaline batteries",
            ))
            .unwrap();
            s.index(&chunk(
                "run_1",
                "c2",
                2,
                "debugging the websocket handshake",
            ))
            .unwrap();
            drop(s);

            let restored = recreate_search_index(&db).expect("rebuild");
            assert_eq!(restored, 2, "both indexed chunks must be restored");

            let reopened = SessionSearch::open(&db).expect("reopen");
            let hits = reopened.search("websocket", 10).expect("search works");
            assert_eq!(hits.len(), 1, "a restored index must still answer queries");
            assert_eq!(hits[0].chunk.chunk_id, "c2");
        }

        #[test]
        fn recreate_on_an_empty_store_is_harmless() {
            let (s, dir) = file_search();
            let db = dir.path().join("ledger.db");
            drop(s);
            let restored = recreate_search_index(&db).expect("rebuild");
            assert_eq!(restored, 0);
            assert!(search_index_health(&db).expect("health probe"));
        }

        /// Rebuild must be repeatable: it drops and recreates, so running it twice
        /// cannot double-count rows or leak duplicates into results.
        #[test]
        fn recreate_is_idempotent() {
            let (s, dir) = file_search();
            let db = dir.path().join("ledger.db");
            s.index(&chunk("run_1", "c1", 1, "alpha beta gamma"))
                .unwrap();
            drop(s);

            assert_eq!(recreate_search_index(&db).expect("first"), 1);
            assert_eq!(recreate_search_index(&db).expect("second"), 1);

            let reopened = SessionSearch::open(&db).expect("reopen");
            let hits = reopened.search("alpha", 10).expect("search");
            assert_eq!(hits.len(), 1, "a repeat rebuild must not duplicate hits");
        }

        /// Retention prunes chunks from both tables; a rebuild must not resurrect
        /// pruned text, or repair would silently undo the retention policy.
        #[test]
        fn recreate_does_not_resurrect_pruned_chunks() {
            let (s, dir) = file_search();
            let db = dir.path().join("ledger.db");
            // Distinct timestamps: `prune_before` is a strict `<` cutoff, so equal
            // timestamps would make the test prune nothing and pass vacuously.
            let mut old = chunk("run_1", "old", 1, "ancient secret phrase");
            old.ts_ms = 1_000;
            let mut new = chunk("run_1", "new", 2, "recent ordinary phrase");
            new.ts_ms = 9_000;
            s.index(&old).unwrap();
            s.index(&new).unwrap();
            assert_eq!(s.prune_before(5_000).expect("prune"), 1);
            drop(s);

            recreate_search_index(&db).expect("rebuild");
            let reopened = SessionSearch::open(&db).expect("reopen");
            assert!(
                reopened.search("ancient", 10).expect("search").is_empty(),
                "pruned text must stay pruned after a rebuild"
            );
            assert_eq!(reopened.search("recent", 10).expect("search").len(), 1);
        }
    }

    /// Whether the FTS5 sidecar can answer a query.
    ///
    /// Separate from "has rows": a freshly created index has no rows and is
    /// perfectly healthy, while an index whose internal structures are damaged
    /// raises on `MATCH` and cannot be repaired by writing to it. Probing for a
    /// working query is the only way to tell those apart.
    pub fn search_index_health(db: &std::path::Path) -> Result<bool, PantheonError> {
        let conn = rusqlite::Connection::open(db).map_err(|e| err("SEARCH_OPEN", e.to_string()))?;
        // Match on tokens that appear in ordinary prose, so a populated index
        // actually returns something to prove the query path works.
        let probe: Result<i64, _> = conn.query_row(
            "SELECT count(*) FROM session_fts WHERE session_fts MATCH 'the' OR session_fts MATCH 'run'",
            [],
            |r| r.get(0),
        );
        match probe {
            Ok(_) => Ok(true),
            // "no such table" means the sidecar was never created, which for a
            // ledger that predates search is normal rather than broken.
            Err(e) if e.to_string().contains("no such table") => Ok(false),
            Err(e) => Err(err("SEARCH_FTS_UNUSABLE", e.to_string())),
        }
    }

    /// Drop and recreate the FTS5 sidecar, then repopulate it from the chunk
    /// store.
    ///
    /// The FTS table is a derived index, but `session_chunks` is the durable
    /// record of what was indexed: it survives an FTS drop, so the contents are
    /// recoverable without replaying the ledger. Returns the number of chunks
    /// restored. Previously this returned an empty table and told the user a
    /// re-index was "not implemented", which left every previously indexed run
    /// silently unsearchable.
    pub fn recreate_search_index(db: &std::path::Path) -> Result<usize, PantheonError> {
        let conn = rusqlite::Connection::open(db).map_err(|e| err("SEARCH_OPEN", e.to_string()))?;
        conn.execute_batch("DROP TABLE IF EXISTS session_fts;")
            .map_err(|e| err("SEARCH_FTS_DROP", e.to_string()))?;
        conn.execute_batch(
            "CREATE VIRTUAL TABLE session_fts USING fts5(
                chunk_id UNINDEXED, text, kind UNINDEXED, run_id UNINDEXED,
                tokenize = 'unicode61'
            );",
        )
        .map_err(|e| err("SEARCH_FTS_CREATE", e.to_string()))?;
        // Rebuild from the durable chunk rows. `session_chunks` holds the same
        // (already truncation-bounded) text the FTS rows carry, so this is a
        // faithful restore rather than a re-derivation from the ledger.
        let restored = conn
            .execute(
                "INSERT INTO session_fts (chunk_id, text, kind, run_id)
                 SELECT chunk_id, text, kind, run_id FROM session_chunks",
                [],
            )
            .map_err(|e| err("SEARCH_FTS_BACKFILL", e.to_string()))?;
        Ok(restored)
    }
}
