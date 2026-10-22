//! Git worktree toolset: first-class isolated working trees.
//!
//! Worktrees are the workspace primitive for parallel work: each one has
//! its own checkout, index, and HEAD while sharing the repository's
//! object store. The engine lives in `pantheon-exec::worktree`; this
//! module only exposes it to the agent.
//!
//! Tools:
//!   - worktree.list    list linked worktrees
//!   - worktree.create  create one (branch `pantheon/<name>`)
//!   - worktree.remove  remove a linked worktree
//!   - worktree.path    print an existing worktree's path (or its managed path)

use crate::tools::{parse_args, ToolRegistry};
use pantheon_api::capability::Capability;
use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::message::ToolSchema;
use pantheon_exec::worktree::WorktreeManager;
use std::path::PathBuf;
use std::sync::Arc;

fn err(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Execution,
        false,
        cause,
        "check the repository path and worktree name",
        "",
    )
}

fn manager(v: &serde_json::Value, state_dir: &Arc<PathBuf>) -> Result<WorktreeManager, String> {
    let repo = v
        .get("repo_dir")
        .and_then(|x| x.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    WorktreeManager::new(&repo, state_dir.as_ref()).map_err(|e| e.to_string())
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

/// Register the worktree toolset. `state_dir` is the Pantheon data dir;
/// managed worktrees land under `<state_dir>/worktrees/<repo>-<hash>/`.
pub fn register_worktrees(reg: &mut ToolRegistry, state_dir: PathBuf) {
    let sd = Arc::new(state_dir);

    let s = Arc::clone(&sd);
    reg.register(
        schema(
            "worktree.list",
            "List the repository's worktrees (main + linked), parsed from `git worktree list --porcelain`.",
            serde_json::json!({
                "repo_dir": {"type":"string","description":"Any path inside the repo (default: process cwd)"},
            }),
            vec![],
        ),
        Capability::GitRead,
        move |args| {
            let v = parse_args(args)?;
            let m = manager(&v, &s).map_err(|e| err("WORKTREE_NO_REPO", e))?;
            let list = m.list().map_err(|e| err("WORKTREE_LIST", e.to_string()))?;
            serde_json::to_string_pretty(&list)
                .map_err(|e| err("WORKTREE_JSON", e.to_string()))
        },
    );

    let s = Arc::clone(&sd);
    reg.register(
        schema(
            "worktree.create",
            "Create a linked git worktree on branch `pantheon/<name>` (from HEAD by default) and return its path. Managed path is under the Pantheon data dir unless `path` is given.",
            serde_json::json!({
                "name": {"type":"string","description":"Short worktree name (letters, digits, -, _, .)"},
                "base": {"type":"string","description":"Git revision to branch from (default: HEAD)"},
                "path": {"type":"string","description":"Explicit worktree directory (default: managed path)"},
                "repo_dir": {"type":"string","description":"Any path inside the repo (default: process cwd)"},
            }),
            vec!["name"],
        ),
        Capability::GitWrite,
        move |args| {
            let v = parse_args(args)?;
            let m = manager(&v, &s).map_err(|e| err("WORKTREE_NO_REPO", e))?;
            let name = v
                .get("name")
                .and_then(|x| x.as_str())
                .ok_or_else(|| err("WORKTREE_BAD_ARGS", "missing 'name'".to_string()))?;
            let base = v.get("base").and_then(|x| x.as_str());
            let path = v.get("path").and_then(|x| x.as_str()).map(PathBuf::from);
            let wt = m
                .create(name, base, path.as_deref())
                .map_err(|e| err("WORKTREE_CREATE", e.to_string()))?;
            serde_json::to_string_pretty(&wt)
                .map_err(|e| err("WORKTREE_JSON", e.to_string()))
        },
    );

    let s = Arc::clone(&sd);
    reg.register(
        schema(
            "worktree.remove",
            "Remove a Pantheon-managed linked worktree by name or path. Refuses the main tree and unrelated linked worktrees; leaves its branch in place. `force` discards uncommitted changes.",
            serde_json::json!({
                "name": {"type":"string","description":"Managed worktree name or path"},
                "force": {"type":"boolean","description":"Discard modifications and untracked files"},
                "repo_dir": {"type":"string","description":"Any path inside the repo (default: process cwd)"},
            }),
            vec!["name"],
        ),
        Capability::GitWrite,
        move |args| {
            let v = parse_args(args)?;
            let m = manager(&v, &s).map_err(|e| err("WORKTREE_NO_REPO", e))?;
            let name = v
                .get("name")
                .and_then(|x| x.as_str())
                .ok_or_else(|| err("WORKTREE_BAD_ARGS", "missing 'name'".to_string()))?;
            let force = v.get("force").and_then(|x| x.as_bool()).unwrap_or(false);
            let wt = m
                .remove(name, force)
                .map_err(|e| err("WORKTREE_REMOVE", e.to_string()))?;
            serde_json::to_string_pretty(&wt)
                .map_err(|e| err("WORKTREE_JSON", e.to_string()))
        },
    );

    let s = Arc::clone(&sd);
    reg.register(
        schema(
            "worktree.path",
            "Return the directory for an existing worktree name, or the managed path it would use without creating it.",
            serde_json::json!({
                "name": {"type":"string"},
                "repo_dir": {"type":"string","description":"Any path inside the repo (default: process cwd)"},
            }),
            vec!["name"],
        ),
        Capability::GitRead,
        move |args| {
            let v = parse_args(args)?;
            let m = manager(&v, &s).map_err(|e| err("WORKTREE_NO_REPO", e))?;
            let name = v
                .get("name")
                .and_then(|x| x.as_str())
                .ok_or_else(|| err("WORKTREE_BAD_ARGS", "missing 'name'".to_string()))?;
            let p = match m.find(name) {
                Ok(Some(w)) => w.path,
                Ok(None) => m
                    .path_for(name)
                    .map_err(|e| err("WORKTREE_BAD_NAME", e.to_string()))?,
                Err(e) => return Err(err("WORKTREE_PATH", e.to_string())),
            };
            Ok(p.display().to_string())
        },
    );
}
