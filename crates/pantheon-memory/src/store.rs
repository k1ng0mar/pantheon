//! SQLite-backed memory provider with FTS recall and provenance.
//!
//! Two tables: `memories` (the record) and `memories_fts` (search index).
//! Every recalled row carries its provenance so the agent (and `pantheon logs`)
//! can see where a belief came from, including its trust tier.
//!
//! The `trust` column is tiered per pantheon_core::provenance::TrustTier.
//! Rows written before tiers existed backfill as `memory` tier on open:
//! they were human/import authored in practice, and treating legacy data
//! as mid-trust is safer than treating it as authoritative.
use crate::{LayerKind, MemoryRecord, Proposal, Provenance};
use pantheon_core::error::{Layer, PantheonError};
use pantheon_core::provenance::TrustTier;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

fn serr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Memory,
        false,
        cause,
        "check the memory database path and disk space",
        "",
    )
}

/// A recall hit: the record plus its rank.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recalled {
    pub record: MemoryRecord,
    pub rank: f64,
}

#[derive(Debug)]
pub struct MemoryStore {
    conn: Mutex<Connection>,
    path: Option<std::path::PathBuf>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS memories (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  layer TEXT NOT NULL,
  namespace TEXT NOT NULL,
  key TEXT NOT NULL,
  value TEXT NOT NULL,
  source TEXT NOT NULL,
  origin TEXT NOT NULL,
  trust TEXT NOT NULL DEFAULT 'memory',
  recorded_at_ms INTEGER NOT NULL,
  UNIQUE(layer, namespace, key)
);
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
  key, value, namespace, layer, origin, content='memories', content_rowid='id'
);
CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
  INSERT INTO memories_fts(rowid, key, value, namespace, layer, origin)
  VALUES (new.id, new.key, new.value, new.namespace, new.layer, new.origin);
END;
CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
  INSERT INTO memories_fts(memories_fts, rowid, key, value, namespace, layer, origin)
  VALUES ('delete', old.id, old.key, old.value, old.namespace, old.layer, old.origin);
  INSERT INTO memories_fts(rowid, key, value, namespace, layer, origin)
  VALUES (new.id, new.key, new.value, new.namespace, new.layer, new.origin);
END;";

/// Backfill for stores created before trust tiers. Legacy rows become
/// `memory` tier: mid-trust, informative, never authoritative. Idempotent.
const MIGRATE_TRUST: &str = "
ALTER TABLE memories ADD COLUMN trust TEXT NOT NULL DEFAULT 'memory';";

/// Add the trust column if the table predates it. `ALTER TABLE ... ADD
/// COLUMN` with a NOT NULL DEFAULT is instant in SQLite (no table rewrite).
fn migrate_trust_column(conn: &Connection) -> Result<(), PantheonError> {
    let has_trust: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('memories') WHERE name='trust'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .map_err(|e| serr("MEM_SCHEMA", e.to_string()))?;
    if !has_trust {
        conn.execute_batch(MIGRATE_TRUST)
            .map_err(|e| serr("MEM_SCHEMA", e.to_string()))?;
    }
    Ok(())
}

fn trust_str(t: TrustTier) -> &'static str {
    t.as_str()
}

fn trust_from(s: &str) -> TrustTier {
    TrustTier::parse(s).unwrap_or(TrustTier::Memory)
}

/// SQL rank of a stored trust tier. `col` is a column reference in the
/// upsert (`memories.trust` = the row already stored, `excluded.trust` =
/// the incoming one). Mirrors `TrustTier::rank` in pantheon-core.
fn sql_rank(col: &str) -> String {
    format!(
        "(CASE WHEN {col} = 'system' THEN 3          WHEN {col} = 'user' THEN 2          WHEN {col} = 'memory' THEN 1          ELSE 0 END)"
    )
}

/// The `put` upsert. Trust is a ceiling on the write, not a field the
/// writer sets: a low-trust (model-origin) write must not clobber the value
/// of a record a human already confirmed, nor reset its tier.
///
/// The guard is `existing_rank > incoming_rank`, strictly greater, so a
/// repeat write at the same tier still overwrites (an agent re-asserting
/// its own note works) and only a strictly-higher-trust write wins. On a
/// blocked write every column is held, so the caller's `put` still returns
/// a record and the outcome is a clean no-op rather than a silent
/// downgrade the user never consented to.
fn upsert_memory_sql() -> String {
    let held = sql_rank("memories.trust");
    let incoming = sql_rank("excluded.trust");
    let guard = format!("{held} > {incoming}");
    let keep = |col: &str| format!("CASE WHEN {guard} THEN memories.{col} ELSE excluded.{col} END");
    format!(
        "INSERT INTO memories (layer, namespace, key, value, source, origin, trust, recorded_at_ms)\n         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)\n         ON CONFLICT(layer, namespace, key) DO UPDATE SET\n           value={v}, source={s}, origin={o}, trust={t}, recorded_at_ms={ts}",
        v = keep("value"),
        s = keep("source"),
        o = keep("origin"),
        t = keep("trust"),
        ts = keep("recorded_at_ms"),
    )
}

/// Map a `memories` row (selected in the order used by `get`) to a record.
/// Shared by `get` and `put` so the re-read in `put` is identical to a normal
/// read.
fn record_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryRecord> {
    Ok(MemoryRecord {
        layer: layer_from(&r.get::<_, String>(0)?),
        namespace: r.get(1)?,
        key: r.get(2)?,
        value: r.get(3)?,
        provenance: Provenance {
            source: r.get(4)?,
            origin: r.get(5)?,
            trust: trust_from(&r.get::<_, String>(6)?),
            recorded_at_ms: r.get(7)?,
        },
    })
}

fn layer_str(l: LayerKind) -> &'static str {
    match l {
        LayerKind::Global => "global",
        LayerKind::Agent => "agent",
        LayerKind::Project => "project",
        LayerKind::TaskSession => "task_session",
        LayerKind::EphemeralTurn => "ephemeral_turn",
    }
}

fn layer_from(s: &str) -> LayerKind {
    match s {
        "global" => LayerKind::Global,
        "agent" => LayerKind::Agent,
        "project" => LayerKind::Project,
        "task_session" => LayerKind::TaskSession,
        _ => LayerKind::EphemeralTurn,
    }
}

impl MemoryStore {
    pub fn open(path: &Path) -> Result<Self, PantheonError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| serr("MEM_MKDIR", e.to_string()))?;
            }
        }
        let conn = Connection::open(path).map_err(|e| serr("MEM_OPEN", e.to_string()))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| serr("MEM_SCHEMA", e.to_string()))?;
        migrate_trust_column(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            path: Some(path.to_path_buf()),
        })
    }

    pub fn open_in_memory() -> Result<Self, PantheonError> {
        let conn = Connection::open_in_memory().map_err(|e| serr("MEM_OPEN", e.to_string()))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| serr("MEM_SCHEMA", e.to_string()))?;
        migrate_trust_column(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            path: None,
        })
    }

    /// Delete a record. Returns whether a row was removed.
    pub fn forget(
        &self,
        layer: LayerKind,
        namespace: &str,
        key: &str,
    ) -> Result<bool, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| serr("MEM_LOCK", e.to_string()))?;
        let n = conn
            .execute(
                "DELETE FROM memories WHERE layer=?1 AND namespace=?2 AND key=?3",
                params![layer_str(layer), namespace, key],
            )
            .map_err(|e| serr("MEM_DELETE", e.to_string()))?;
        Ok(n > 0)
    }

    /// List Agent-layer records for a namespace in stable write order,
    /// including each record's trust tier. The plain `list_agent` stays
    /// for callers that only need key/value (the memory_list tool);
    /// anything that round-trips records through files must use this so
    /// trust survives the trip.
    pub fn list_agent_meta(
        &self,
        namespace: &str,
    ) -> Result<Vec<(String, String, TrustTier)>, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| serr("MEM_LOCK", e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT key, value, trust FROM memories
                 WHERE layer='agent' AND namespace=?1
                 ORDER BY recorded_at_ms, id",
            )
            .map_err(|e| serr("MEM_QUERY", e.to_string()))?;
        let rows = stmt
            .query_map([namespace], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    trust_from(&r.get::<_, String>(2)?),
                ))
            })
            .map_err(|e| serr("MEM_QUERY", e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| serr("MEM_QUERY", e.to_string()))?);
        }
        Ok(out)
    }

    /// List Agent-layer records for a namespace in stable write order.
    pub fn list_agent(&self, namespace: &str) -> Result<Vec<(String, String)>, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| serr("MEM_LOCK", e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT key, value FROM memories
                 WHERE layer='agent' AND namespace=?1
                 ORDER BY recorded_at_ms, id",
            )
            .map_err(|e| serr("MEM_QUERY", e.to_string()))?;
        let rows = stmt
            .query_map([namespace], |r| Ok((r.get(0)?, r.get(1)?)))
            .map_err(|e| serr("MEM_QUERY", e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| serr("MEM_QUERY", e.to_string()))?);
        }
        Ok(out)
    }

    /// Return the backing path, if this store is file-backed.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Promote one record's trust tier. Only raises the tier (the
    /// `derived <= source` invariant runs in reverse only through
    /// explicit promotion); demotion goes through delete + rewrite.
    pub fn promote(
        &self,
        namespace: &str,
        key: &str,
        tier: pantheon_core::provenance::TrustTier,
    ) -> Result<MemoryRecord, PantheonError> {
        let updated = {
            let conn = self
                .conn
                .lock()
                .map_err(|e| serr("MEM_LOCK", e.to_string()))?;
            // Only upgrade: an existing higher tier wins. The WHERE clause
            // constrains what can change, so a record already at or above
            // the requested tier is untouched.
            conn.execute(
                "UPDATE memories SET trust=?3
                 WHERE namespace=?1 AND key=?2
                   AND trust IN ('untrusted','memory')",
                params![namespace, key, trust_str(tier)],
            )
            .map_err(|e| serr("MEM_PUT", e.to_string()))?
        };
        if updated == 0 {
            // Distinguish "no such record" from "already at or above the
            // requested tier" so confirm on a user record is a clear
            // no-op message, not a missing-row error.
            return match self.get(namespace, key)? {
                Some(rec) => Ok(rec),
                None => Err(serr(
                    "MEM_NOT_FOUND",
                    format!("no promotable record {key} in {namespace}"),
                )),
            };
        }
        self.get(namespace, key)?
            .ok_or_else(|| serr("MEM_NOT_FOUND", format!("no record {key} in {namespace}")))
    }

    /// Fetch one record by namespace + key.
    pub fn get(&self, namespace: &str, key: &str) -> Result<Option<MemoryRecord>, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| serr("MEM_LOCK", e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT layer, namespace, key, value, source, origin, trust, recorded_at_ms
                 FROM memories WHERE namespace=?1 AND key=?2",
            )
            .map_err(|e| serr("MEM_QUERY", e.to_string()))?;
        let mut rows = stmt
            .query_map(params![namespace, key], record_from_row)
            .map_err(|e| serr("MEM_QUERY", e.to_string()))?;
        match rows.next() {
            Some(row) => row.map(Some).map_err(|e| serr("MEM_QUERY", e.to_string())),
            None => Ok(None),
        }
    }

    pub fn put(&self, p: &Proposal) -> Result<MemoryRecord, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| serr("MEM_LOCK", e.to_string()))?;
        conn.execute(
            // Trust is a ceiling on the write, not a field the writer sets.
            // A low-trust (model-origin) write must not clobber the value of
            // a record a human already confirmed, nor reset its tier. Ranks:
            // untrusted(0) < memory(1) < user(2) < system(3). Equal rank is
            // treated as "existing wins" so a repeat write at the same tier
            // still updates (an agent re-asserting its own note works), and
            // only a strictly-higher-trust write overwrites.
            &upsert_memory_sql(),
            params![
                layer_str(p.layer),
                p.namespace,
                p.key,
                p.value,
                p.provenance.source,
                p.provenance.origin,
                trust_str(p.provenance.trust),
                p.provenance.recorded_at_ms
            ],
        )
        .map_err(|e| serr("MEM_PUT", e.to_string()))?;
        // Re-read rather than echo the proposal: when a higher-trust row
        // held, the stored value is the old one, and returning `p` would tell
        // the caller its write landed when it did not.
        //
        // The read runs on the connection already checked out above, NOT via
        // `self.get`: `conn` is a `std::sync::Mutex` guard and `get` locks it
        // again, so calling it here deadlocks on a non-reentrant mutex.
        let mut stmt = conn
            .prepare(
                "SELECT layer, namespace, key, value, source, origin, trust, recorded_at_ms
                 FROM memories WHERE namespace=?1 AND key=?2",
            )
            .map_err(|e| serr("MEM_QUERY", e.to_string()))?;
        let mut rows = stmt
            .query_map(params![p.namespace, p.key], record_from_row)
            .map_err(|e| serr("MEM_QUERY", e.to_string()))?;
        match rows.next() {
            Some(row) => row.map_err(|e| serr("MEM_QUERY", e.to_string())),
            None => Err(serr(
                "MEM_PUT",
                format!("record vanished after write: {}/{}", p.namespace, p.key),
            )),
        }
    }

    /// FTS recall across the given layers, narrowest-first ordering applied
    /// by the caller passing layers in priority order.
    pub fn search(
        &self,
        layers: &[LayerKind],
        query: &str,
        limit: usize,
    ) -> Result<Vec<Recalled>, PantheonError> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let conn = self
            .conn
            .lock()
            .map_err(|e| serr("MEM_LOCK", e.to_string()))?;
        let mut stmt = conn
            .prepare(
                "SELECT m.layer, m.namespace, m.key, m.value, m.source, m.origin,
                    m.trust, m.recorded_at_ms, bm25(memories_fts) AS rank
             FROM memories_fts f
             JOIN memories m ON m.id = f.rowid
             WHERE memories_fts MATCH ?1
             ORDER BY rank LIMIT ?2",
            )
            .map_err(|e| serr("MEM_SEARCH", e.to_string()))?;
        let fts_query = query
            .split_whitespace()
            .map(|token| {
                token
                    .chars()
                    .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-')
                    .collect::<String>()
            })
            .filter(|token| !token.is_empty())
            .map(|token| format!("\"{token}\""))
            .collect::<Vec<_>>()
            .join(" OR ");
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }
        let rows = stmt
            .query_map(params![fts_query, limit as i64], |r| {
                Ok(Recalled {
                    record: MemoryRecord {
                        layer: layer_from(&r.get::<_, String>(0)?),
                        namespace: r.get(1)?,
                        key: r.get(2)?,
                        value: r.get(3)?,
                        provenance: Provenance {
                            source: r.get(4)?,
                            origin: r.get(5)?,
                            trust: trust_from(&r.get::<_, String>(6)?),
                            recorded_at_ms: r.get(7)?,
                        },
                    },
                    rank: r.get(8)?,
                })
            })
            .map_err(|e| serr("MEM_SEARCH", e.to_string()))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(|e| serr("MEM_SEARCH", e.to_string()))?);
        }
        // Narrowest layers first: stable sort by layer priority.
        out.sort_by_key(|r| {
            layers
                .iter()
                .position(|l| *l == r.record.layer)
                .unwrap_or(usize::MAX)
        });
        Ok(out)
    }
}

/// Whether the memory FTS5 sidecar can answer a query.
///
/// Same reasoning as the ledger's `search_index_health`: a freshly created
/// index has no rows and is healthy, while a damaged one raises on `MATCH`.
/// Probing for a working query is the only way to tell them apart.
pub fn fts_health(db: &std::path::Path) -> Result<bool, PantheonError> {
    let conn = Connection::open(db).map_err(|e| serr("MEM_FTS_OPEN", e.to_string()))?;
    let probe: Result<i64, _> = conn.query_row(
        "SELECT count(*) FROM memories_fts WHERE memories_fts MATCH 'the' OR memories_fts MATCH 'project'",
        [],
        |r| r.get(0),
    );
    match probe {
        Ok(_) => Ok(true),
        // No table yet is normal for a fresh or pre-search store.
        Err(e) if e.to_string().contains("no such table") => Ok(false),
        Err(e) => Err(serr("MEM_FTS_UNUSABLE", e.to_string())),
    }
}

/// Rebuild the memory FTS5 index from the `memories` table.
///
/// This one *is* a full rebuild, unlike the ledger's sidecar. `memories_fts` is
/// an external-content table (`content='memories'`) kept in sync by the
/// `memories_ai` / `memories_au` triggers, so the canonical rows are already
/// in `memories` and re-deriving is a matter of repopulating the index:
/// drop it, let `SCHEMA` recreate it with its triggers, then `rebuild`.
///
/// Hand-writing the CREATE would be the trap here: an external-content table
/// with the wrong columns still opens and still answers MATCH, so a subtly
/// wrong rebuild passes every "does it work" check while ranking results
/// against a different column set. Re-running `SCHEMA` cannot drift from the
/// writer.
pub fn rebuild_fts(db: &std::path::Path) -> Result<u64, PantheonError> {
    let conn = Connection::open(db).map_err(|e| serr("MEM_FTS_OPEN", e.to_string()))?;
    conn.execute_batch("DROP TABLE IF EXISTS memories_fts;")
        .map_err(|e| serr("MEM_FTS_DROP", e.to_string()))?;
    // SCHEMA is `CREATE ... IF NOT EXISTS` throughout, so re-running it
    // recreates the table *and* the triggers that maintain it.
    conn.execute_batch(SCHEMA)
        .map_err(|e| serr("MEM_FTS_RECREATE", e.to_string()))?;
    // The FTS5 builtin rebuild command repopulates an external-content index
    // from its content table. This is the part a hand-rolled INSERT list would
    // get wrong.
    conn.execute_batch("INSERT INTO memories_fts(memories_fts) VALUES('rebuild');")
        .map_err(|e| serr("MEM_FTS_REBUILD", e.to_string()))?;
    let n: i64 = conn
        .query_row("SELECT count(*) FROM memories", [], |r| r.get(0))
        .map_err(|e| serr("MEM_COUNT", e.to_string()))?;
    Ok(n as u64)
}

/// Drop the FTS sidecar while leaving the triggers in place — the state a
/// partial write or an interrupted migration leaves behind.
///
/// Takes a path rather than exposing a `Connection`, so a consumer can damage
/// a store to test recovery without the public API growing a raw handle.
/// Idempotent.
pub fn damage_fts_for_test(db: &std::path::Path) {
    let conn = Connection::open(db).expect("open memory db");
    conn.execute_batch("DROP TABLE IF EXISTS memories_fts;")
        .expect("drop fts");
}
