//! Agent collaboration state (§3 swarm, §2 agent engine).
//!
//! A collaboration is a durable, first-class group of agent profiles working
//! toward a shared objective, decomposed into tasks that survive process
//! death. This is deliberately **not** a subprocess mechanism: a task names
//! an agent *profile*, and the executing agent runs inside this runtime under
//! its own identity and its own policy.
//!
//! # Why this lives beside the ledger
//!
//! Task state is event-sourced like everything else, but task *queries*
//! ("what is Zeus doing right now?") cannot be answered by replaying an
//! event stream. So there are two stores over one truth: the ledger holds
//! the audit trail, this module holds the current-state projection. The
//! projection is written in the same append path that persists the events,
//! and [`CollaborationStore::reconcile`] rebuilds it from the ledger if the
//! projection is ever behind — the same discipline the operation state
//! machine already uses.
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
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

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

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
/// result without parsing prose — and, more importantly, so an agent
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
    fn parse(s: &str) -> Result<Self, PantheonError> {
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
        let conn = Connection::open(path).map_err(|e| cerr("COLLABORATION_OPEN", e.to_string()))?;
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(|e| cerr("COLLABORATION_BUSY_TIMEOUT", e.to_string()))?;
        conn.execute_batch(SCHEMA)
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
                    current.version
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
    /// compare-and-swap into an unconditional overwrite — the parameter has
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
                    current.version
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
             (message_id, collaboration_id, task_id, sender, recipient, kind, content, created_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                message_id,
                collaboration_id,
                task_id,
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
                        created_ms, settled_ms
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
                        created_ms, settled_ms
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
                        created_ms, settled_ms
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
        version: r.get(7)?,
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
    })
}

fn task_from_conn(conn: &Connection, task_id: &str) -> Result<Option<AgentTask>, PantheonError> {
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

#[cfg(test)]
#[path = "collaboration_tests.rs"]
mod collaboration_tests;
