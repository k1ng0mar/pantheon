//! Hybrid session search: an additive FTS5 sidecar next to the event
//! ledger, exposed to the model as the `session_search` tool.
//!
//! FTS5 is the primary retrieval layer (exact identifiers, tool names,
//! model ids, quoted phrases). Chunks are indexed as events are emitted —
//! new messages chunk-and-index forward-only; old chunks never change
//! unless deliberately re-indexed. Embeddings are a secondary recall
//! layer, intentionally deferred until an embedding-capable provider is
//! wired: the schema and scoring seam exist, the default lexical path
//! works without them.
//!
//! Additive by design: nothing here changes run/event handling.

use pantheon_api::error::{Layer, PantheonError};
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

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
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
    /// Hybrid score: 0.60 lexical + 0.30 recency (embeddings 0.30 until a
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

/// Max chars of tool output indexed per chunk — tool results can be huge
/// and would drown the lexical index (same tradeoff Hermes makes at 8192).
const TOOL_CHUNK_MAX: usize = 8192;
/// Max chars of a message indexed per chunk.
const MESSAGE_CHUNK_MAX: usize = 4096;

/// SQLite-persisted session search index.
///
/// Owns its own connection to the same ledger file, so indexing happens
/// inside the ledger transaction path rather than in a second process.
pub struct SessionSearch {
    conn: Mutex<Connection>,
}

impl SessionSearch {
    /// Open (or create) the search sidecar. `path` is the ledger\'s own
    /// file — the schema lives in the same DB so a hit joins the run.
    pub fn open(path: &Path) -> Result<Self, PantheonError> {
        let conn = Connection::open(path).map_err(|e| err("SEARCH_OPEN", e.to_string()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| err("SEARCH_BUSY", e.to_string()))?;
        // Tables first, then the forward-only migration (ledgers created
        // before the vector layer lack the embedding column), then the
        // indexes — the partial index requires the column to exist, so it
        // must run after the migration, not in the same batch. Fresh DBs
        // already have the column; the ALTER is a no-op error there.
        conn.execute_batch(SCHEMA_TABLES)
            .map_err(|e| err("SEARCH_SCHEMA", e.to_string()))?;
        let _ = conn.execute("ALTER TABLE session_chunks ADD COLUMN embedding BLOB", []);
        conn.execute_batch(SCHEMA_INDEXES)
            .map_err(|e| err("SEARCH_SCHEMA", e.to_string()))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn open_in_memory() -> Result<Self, PantheonError> {
        let conn = Connection::open_in_memory().map_err(|e| err("SEARCH_OPEN", e.to_string()))?;
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
    /// is `None` when no embeddings auxiliary is configured — the chunk
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
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("SEARCH_LOCK", e.to_string()))?;
        conn.execute(
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
        conn.execute(
            "DELETE FROM session_fts WHERE chunk_id = ?1",
            params![chunk.chunk_id],
        )
        .map_err(|e| err("SEARCH_FTS_DELETE", e.to_string()))?;
        conn.execute(
            "INSERT INTO session_fts (chunk_id, text, kind, run_id) VALUES (?1,?2,?3,?4)",
            params![chunk.chunk_id, text, chunk.kind, chunk.run_id],
        )
        .map_err(|e| err("SEARCH_FTS_INSERT", e.to_string()))?;
        Ok(())
    }

    /// Remove every chunk for one run (used when a run is deleted or
    /// re-indexed from scratch).
    pub fn drop_run(&self, run_id: &str) -> Result<(), PantheonError> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("SEARCH_LOCK", e.to_string()))?;
        conn.execute(
            "DELETE FROM session_chunks WHERE run_id = ?1",
            params![run_id],
        )
        .map_err(|e| err("SEARCH_DELETE", e.to_string()))?;
        conn.execute("DELETE FROM session_fts WHERE run_id = ?1", params![run_id])
            .map_err(|e| err("SEARCH_FTS_DELETE", e.to_string()))?;
        Ok(())
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
                    h.score = h.score / 0.70 * 0.90;
                    h
                })
                .take(limit)
                .collect());
        };

        // Load embeddings for the lexical candidates plus a sample of
        // never-lexically-matched chunks (semantic recall's whole point:
        // find chunks the keyword query missed).
        let conn = self
            .conn
            .lock()
            .map_err(|e| err("SEARCH_LOCK", e.to_string()))?;
        let mut stmt = conn
            .prepare("SELECT chunk_id, run_id, seq, kind, text, ts_ms, embedding FROM session_chunks WHERE embedding IS NOT NULL")
            .map_err(|e| err("SEARCH_QUERY", e.to_string()))?;
        let rows = stmt
            .query_map([], |r| {
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
        let now = now_ms();
        let mut semantic_hits: Vec<(SessionChunk, f32)> = Vec::new();
        for row in rows {
            let (chunk_id, run_id, seq, kind, text, ts_ms, emb) =
                row.map_err(|e| err("SEARCH_ROW", e.to_string()))?;
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
        semantic_hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

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
        for (i, (chunk, sim)) in semantic_hits.iter().enumerate().take(limit) {
            let semantic = (sim / sem_max) as f64;
            let age_ms = (now - chunk.ts_ms).max(0) as f64;
            let recency = 1.0 / (1.0 + age_ms / 3_600_000.0);
            let score = 0.30 * semantic + 0.10 * recency;
            by_id
                .entry(chunk.chunk_id.clone())
                .and_modify(|(s, _)| *s += score)
                .or_insert((score, chunk.clone()));
            let _ = i;
        }
        let mut out: Vec<SearchHit> = by_id
            .into_iter()
            .map(|(chunk_id, (score, chunk))| SearchHit {
                chunk: SessionChunk { chunk_id, ..chunk },
                score,
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
#[path = "search_tests.rs"]
mod tests;

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

/// Drop and recreate the FTS5 sidecar as an empty table.
///
/// This restores the *ability to search*; it does not restore the *contents*.
/// Re-deriving rows from the ledger is not implemented, so previously indexed
/// runs are not searchable until they are re-indexed. Callers must say so.
pub fn recreate_search_index(db: &std::path::Path) -> Result<(), PantheonError> {
    let conn = rusqlite::Connection::open(db).map_err(|e| err("SEARCH_OPEN", e.to_string()))?;
    conn.execute_batch("DROP TABLE IF EXISTS session_fts;")
        .map_err(|e| err("SEARCH_FTS_DROP", e.to_string()))?;
    conn.execute_batch(
        "CREATE VIRTUAL TABLE session_fts USING fts5(
            chunk_id UNINDEXED, text, kind UNINDEXED, run_id UNINDEXED
        );",
    )
    .map_err(|e| err("SEARCH_FTS_CREATE", e.to_string()))?;
    Ok(())
}
