//! Codebase graph tools: a native, cacheable symbol/call index.
//!
//! The parser and graph live in `pantheon-exec::codegraph`; this module
//! exposes orientation queries to the agent:
//!
//!   - codebase.index     build/refresh the index
//!   - codebase.search    find symbols by name, qualified name, or file
//!   - codebase.callers   lexical call sites for a symbol
//!   - codebase.callees   lexical calls made by a symbol
//!   - codebase.overview  counts and top directories
//!
//! Lexical, not compiler-grade: see the engine docs. That is deliberate;
//! it works without a language server and is honest about ambiguity.

use crate::tools::{parse_args, ToolRegistry};
use pantheon_api::capability::Capability;
use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::message::ToolSchema;
use pantheon_exec::codegraph::CodeGraph;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn err(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Execution,
        false,
        cause,
        "check the repository path and index it with codebase.index",
        "",
    )
}

fn schema(
    name: &str,
    description: &str,
    props: serde_json::Value,
    required: Vec<&str>,
) -> ToolSchema {
    ToolSchema {
        name: name.into(),
        description: description.into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": props,
            "required": required,
        }),
    }
}

fn root_of(v: &serde_json::Value) -> PathBuf {
    v.get("path")
        .and_then(|x| x.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

fn canonical_root(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

fn graph(root: &Path, state_dir: &Path, force: bool) -> Result<CodeGraph, PantheonError> {
    let root = canonical_root(root);
    let cache = CodeGraph::cache_path_for(state_dir, &root);
    if !force {
        if let Some(g) = CodeGraph::load(&cache) {
            if canonical_root(Path::new(&g.root)) == root && g.is_current_for(&root) {
                return Ok(g);
            }
        }
    }
    let g = CodeGraph::build(&root)?;
    // A cache write is best-effort: a read-only data dir must not make
    // orientation queries fail.
    let _ = g.save(&cache);
    Ok(g)
}

fn limit_of(v: &serde_json::Value) -> usize {
    v.get("limit")
        .and_then(|x| x.as_u64())
        .unwrap_or(50)
        .clamp(1, 500) as usize
}

/// Register the codebase graph toolset. `state_dir` is the Pantheon data
/// dir; the cache lives at `<state_dir>/codegraph/index.json`.
pub fn register_codegraph(reg: &mut ToolRegistry, state_dir: PathBuf) {
    let sd = Arc::new(state_dir);

    let s = Arc::clone(&sd);
    reg.register(
        schema(
            "codebase.index",
            "Build or refresh a native Rust symbol/call graph for a repository, cache it, and return counts. Lexical (syn-based), not compiler-grade.",
            serde_json::json!({
                "path": {"type":"string","description":"Repository root (default: process cwd)"},
                "force": {"type":"boolean","description":"Rebuild even when a cache for this root exists"},
            }),
            vec![],
        ),
        Capability::FilesystemWrite,
        move |args| {
            let v = parse_args(args)?;
            let root = root_of(&v);
            let force = v.get("force").and_then(|x| x.as_bool()).unwrap_or(true);
            let g = graph(&root, &s, force)?;
            serde_json::to_string_pretty(&g.overview())
                .map_err(|e| err("CODEGRAPH_JSON", e.to_string()))
        },
    );

    let s = Arc::clone(&sd);
    reg.register(
        schema(
            "codebase.search",
            "Search the cached codebase graph for symbols whose name, qualified name, or file contains the query.",
            serde_json::json!({
                "query": {"type":"string","description":"Case-insensitive substring"},
                "path": {"type":"string","description":"Repository root (default: process cwd)"},
                "limit": {"type":"integer","description":"Max results (default 50, max 500)"},
            }),
            vec!["query"],
        ),
        Capability::FilesystemRead,
        move |args| {
            let v = parse_args(args)?;
            let query = v
                .get("query")
                .and_then(|x| x.as_str())
                .ok_or_else(|| err("CODEGRAPH_BAD_ARGS", "missing 'query'".to_string()))?;
            let g = graph(&root_of(&v), &s, false)?;
            let results = g.search(query, limit_of(&v));
            serde_json::to_string_pretty(&serde_json::json!({
                "query": query,
                "root": g.root,
                "results": results,
            }))
            .map_err(|e| err("CODEGRAPH_JSON", e.to_string()))
        },
    );

    let s = Arc::clone(&sd);
    reg.register(
        schema(
            "codebase.callers",
            "Find lexical call sites whose callee name matches a symbol (exact final path segment).",
            serde_json::json!({
                "symbol": {"type":"string","description":"Function/method name"},
                "path": {"type":"string","description":"Repository root (default: process cwd)"},
                "limit": {"type":"integer","description":"Max hits (default 50, max 500)"},
            }),
            vec!["symbol"],
        ),
        Capability::FilesystemRead,
        move |args| {
            let v = parse_args(args)?;
            let symbol = v
                .get("symbol")
                .and_then(|x| x.as_str())
                .ok_or_else(|| err("CODEGRAPH_BAD_ARGS", "missing 'symbol'".to_string()))?;
            let g = graph(&root_of(&v), &s, false)?;
            let hits = g.callers(symbol, limit_of(&v));
            serde_json::to_string_pretty(&serde_json::json!({
                "symbol": symbol,
                "root": g.root,
                "callers": hits,
            }))
            .map_err(|e| err("CODEGRAPH_JSON", e.to_string()))
        },
    );

    let s = Arc::clone(&sd);
    reg.register(
        schema(
            "codebase.callees",
            "List the lexical calls made by a symbol (exact name or qualified suffix).",
            serde_json::json!({
                "symbol": {"type":"string","description":"Function/method name"},
                "path": {"type":"string","description":"Repository root (default: process cwd)"},
                "limit": {"type":"integer","description":"Max hits (default 50, max 500)"},
            }),
            vec!["symbol"],
        ),
        Capability::FilesystemRead,
        move |args| {
            let v = parse_args(args)?;
            let symbol = v
                .get("symbol")
                .and_then(|x| x.as_str())
                .ok_or_else(|| err("CODEGRAPH_BAD_ARGS", "missing 'symbol'".to_string()))?;
            let g = graph(&root_of(&v), &s, false)?;
            let hits = g.callees(symbol, limit_of(&v));
            serde_json::to_string_pretty(&serde_json::json!({
                "symbol": symbol,
                "root": g.root,
                "callees": hits,
            }))
            .map_err(|e| err("CODEGRAPH_JSON", e.to_string()))
        },
    );

    let s = Arc::clone(&sd);
    reg.register(
        schema(
            "codebase.overview",
            "Return codebase graph counts by symbol kind and the top directories by symbol count.",
            serde_json::json!({
                "path": {"type":"string","description":"Repository root (default: process cwd)"},
            }),
            vec![],
        ),
        Capability::FilesystemRead,
        move |args| {
            let v = parse_args(args)?;
            let g = graph(&root_of(&v), &s, false)?;
            serde_json::to_string_pretty(&g.overview())
                .map_err(|e| err("CODEGRAPH_JSON", e.to_string()))
        },
    );
}
