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
                std::fs::create_dir_all(parent).map_err(|e| err("IDEA_MKDIR", e.to_string()))?;
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
        let includes =
            serde_json::to_string(&idea.includes).map_err(|e| err("IDEA_JSON", e.to_string()))?;
        let schedule = idea
            .schedule
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| err("IDEA_JSON", e.to_string()))?;
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
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
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
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
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
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
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
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
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
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
    pub fn set_status(
        &self,
        id: &str,
        status: IdeaStatus,
        decided_ms: Option<i64>,
    ) -> Result<bool, PantheonError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let n = conn
            .execute(
                "UPDATE ideas SET status = ?1, decided_ms = ?2 WHERE id = ?3",
                params![status.as_str(), decided_ms, id],
            )
            .map_err(|e| err("IDEA_STATUS_SET", e.to_string()))?;
        Ok(n == 1)
    }

    /// Record explicit `more`/`less` feedback on the idea and its topic.
    /// Returns `false` when the id does not exist.
    pub fn record_feedback(&self, id: &str, signal: FeedbackSignal) -> Result<bool, PantheonError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let col = match signal {
            FeedbackSignal::More => "more",
            FeedbackSignal::Less => "less",
        };
        let n = conn
            .execute(
                &format!("UPDATE ideas SET {col} = {col} + 1 WHERE id = ?1"),
                [id],
            )
            .map_err(|e| err("IDEA_FEEDBACK", e.to_string()))?;
        if n != 1 {
            return Ok(false);
        }
        let topic: String = conn
            .query_row("SELECT topic FROM ideas WHERE id = ?1", [id], |r| r.get(0))
            .map_err(|e| err("IDEA_FEEDBACK", e.to_string()))?;
        if !topic.is_empty() {
            conn.execute(
                &format!(
                    "INSERT INTO idea_topic_signals (topic, {col}) VALUES (?1, 1)
                     ON CONFLICT(topic) DO UPDATE SET {col} = {col} + 1"
                ),
                [topic],
            )
            .map_err(|e| err("IDEA_FEEDBACK", e.to_string()))?;
        }
        Ok(true)
    }

    /// Delete `pending` ideas created before `day` (`YYYY-MM-DD`,
    /// exclusive). Returns the number removed.
    pub fn expire_pending_before(&self, day: &str) -> Result<usize, PantheonError> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
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
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
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
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
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
    let includes: Vec<String> = serde_json::from_str(&includes_json).unwrap_or_default();
    let kind: String = r.get(4)?;
    let status: String = r.get(6)?;
    let schedule_json: Option<String> = r.get(8)?;
    let schedule = schedule_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, Box::new(e))
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
