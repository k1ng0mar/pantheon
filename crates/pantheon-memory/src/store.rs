//! SQLite-backed memory provider with FTS recall and provenance.
//!
//! Two tables: `memories` (the record) and `memories_fts` (search index).
//! Every recalled row carries its provenance so the agent (and `/explain`)
//! can see where a belief came from.
use crate::{LayerKind, MemoryRecord, Proposal, Provenance};
use pantheon_core::error::{Layer, PantheonError};
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
        Ok(Self {
            conn: Mutex::new(conn),
            path: Some(path.to_path_buf()),
        })
    }

    pub fn open_in_memory() -> Result<Self, PantheonError> {
        let conn = Connection::open_in_memory().map_err(|e| serr("MEM_OPEN", e.to_string()))?;
        conn.execute_batch(SCHEMA)
            .map_err(|e| serr("MEM_SCHEMA", e.to_string()))?;
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
    pub fn put(&self, p: &Proposal) -> Result<MemoryRecord, PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| serr("MEM_LOCK", e.to_string()))?;
        conn.execute(
            "INSERT INTO memories (layer, namespace, key, value, source, origin, recorded_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(layer, namespace, key) DO UPDATE SET
               value=excluded.value, source=excluded.source,
               origin=excluded.origin, recorded_at_ms=excluded.recorded_at_ms",
            params![
                layer_str(p.layer),
                p.namespace,
                p.key,
                p.value,
                p.provenance.source,
                p.provenance.origin,
                p.provenance.recorded_at_ms
            ],
        )
        .map_err(|e| serr("MEM_PUT", e.to_string()))?;
        Ok(MemoryRecord {
            layer: p.layer,
            namespace: p.namespace.clone(),
            key: p.key.clone(),
            value: p.value.clone(),
            provenance: p.provenance.clone(),
        })
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
                    m.recorded_at_ms, bm25(memories_fts) AS rank
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
                            recorded_at_ms: r.get(6)?,
                        },
                    },
                    rank: r.get(7)?,
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
