//! `session_search` tool: the agent searches its own prior conversation
//! ledger. FTS5 is the primary retrieval layer (exact identifiers, tool
//! names, quoted phrases); recency breaks ties. Chunked, not whole
//! sessions — a hit points at the exact source event (`run_id`, `seq`).
//!
//! Gated on `FilesystemRead`: the index is a local SQLite file, same
//! trust level as reading the ledger directly.

use crate::tools::{parse_args, ToolRegistry};
use pantheon_core::capability::Capability;
use pantheon_core::error::{Layer, PantheonError};
use pantheon_core::message::ToolSchema;
use pantheon_providers::embeddings::EmbedderClient;
use std::sync::Arc;

fn serr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Execution,
        false,
        cause,
        "check tool arguments",
        "",
    )
}

fn arg_str(v: &serde_json::Value, key: &str) -> Result<String, PantheonError> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| serr("TOOL_BAD_ARGS", format!("missing string arg '{key}'")))
}

/// Options for `register_session_search`. The store is shared with the
/// indexing path so the tool reads exactly what the session wrote.
#[derive(Clone)]
pub struct SessionSearchOptions {
    pub store: Arc<pantheon_storage::search::SessionSearch>,
    /// The same client the indexer used. Without it the query cannot be
    /// embedded, semantic recall is unreachable, and every embedding
    /// already on disk would be silently ignored.
    pub embedder: Option<Arc<pantheon_providers::embeddings::EmbedClient>>,
}

/// Register the `session_search` tool on a registry.
pub fn register_session_search(reg: &mut ToolRegistry, opts: SessionSearchOptions) {
    let store = opts.store;
    let embedder = opts.embedder;
    reg.register(
        ToolSchema {
            name: "session_search".into(),
            description: "Search through previous and current conversation sessions. Finds exact identifiers, tool names, file paths, model ids, and quoted phrases inside past sessions. Returns matching chunks with their run id, sequence, and age. Use this when the user asks about earlier work ('that session where I was debugging X') or to recall a specific detail from a past conversation.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search query. Plain terms, quoted phrases, or identifiers. Examples: Figma MCP debugging, 9f3a2, \"Read TUI session creation block\"."},
                    "limit": {"type": "integer", "description": "Max results (default 8)."}
                },
                "required": ["query"]
            }),
        },
        Capability::FilesystemRead,
        move |args| {
            let v = parse_args(args)?;
            let query = arg_str(&v, "query")?;
            let limit = v.get("limit").and_then(|x| x.as_u64()).unwrap_or(8) as usize;
            // Embed the query with the indexer's own client so the vector
            // term is comparable. A failed embed is not an error: lexical
            // plus recency is a complete answer, just a narrower one.
            let qvec: Option<Vec<f32>> = embedder
                .as_ref()
                .and_then(|c| c.embed(std::slice::from_ref(&query)).ok())
                .and_then(|mut v| v.drain(..).next())
                .map(|e| e.vec);
            let hits = match qvec.as_deref() {
                Some(q) => store
                    .search_hybrid(&query, Some(q), limit)
                    .map_err(|e| serr("TOOL_SEARCH", e.cause.clone()))?,
                None => store
                    .search(&query, limit)
                    .map_err(|e| serr("TOOL_SEARCH", e.cause.clone()))?,
            };
            if hits.is_empty() {
                return Ok(format!("no sessions matched '{query}'"));
            }
            let mut out = String::new();
            for h in &hits {
                let c = &h.chunk;
                let snippet: String = c.text.chars().take(240).collect();
                out.push_str(&format!(
                    "[{}] run {} seq {} · {} · {}\n{}\n\n",
                    h.score,
                    c.run_id,
                    c.seq,
                    c.kind,
                    fmt_age(c.ts_ms),
                    snippet
                ));
            }
            Ok(out.trim_end().to_string())
        },
    );
}

/// Human-readable age for a hit (mirrors the CLI\'s fmt_created).
fn fmt_age(ms: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let s = (now - ms).max(0) / 1000;
    if s < 60 {
        format!("{s}s ago")
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 86400 {
        format!("{}h ago", s / 3600)
    } else {
        format!("{}d ago", s / 86400)
    }
}
