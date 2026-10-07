//! Git-undo toolset: repo-level snapshot/restore for batch workspaces.
//!
//! `safewrite` checkpoints individual file edits. These tools snapshot the
//! entire git working tree before a batch of edits and restore all changed
//! files at once. The engine lives in `pantheon-exec::gitundo`; this
//! module registers the tools on a registry (capability != tool).
//!
//! Tools:
//!   - gitundo.snapshot  create a labeled undo point
//!   - gitundo.list      list stored undo points, newest first
//!   - gitundo.restore   restore one undo point by id
//!   - gitundo.delete    drop one undo point and its ref
use crate::tools::{parse_args, ToolRegistry};
use pantheon_api::capability::Capability;
use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::message::ToolSchema;
use pantheon_exec::gitundo::{repo_root, GitUndo};
use std::path::PathBuf;
use std::sync::Arc;

fn err(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Execution,
        false,
        cause,
        "check tool arguments",
        "",
    )
}

/// Options for `register_gitundo_with`.
pub struct GitUndoOptions {
    /// State dir for stored patches + manifests (`<state>/git_undo`).
    pub state_dir: PathBuf,
    /// The git working tree to snapshot/restore. `None` = process cwd.
    pub repo_dir: Option<PathBuf>,
}

/// Register the git-undo toolset on a registry.
pub fn register_gitundo(reg: &mut ToolRegistry, state_dir: PathBuf) {
    register_gitundo_with(
        reg,
        GitUndoOptions {
            state_dir,
            repo_dir: None,
        },
    );
}

/// Register with options.
pub fn register_gitundo_with(reg: &mut ToolRegistry, opts: GitUndoOptions) {
    let sdir: Arc<PathBuf> = Arc::new(opts.state_dir);
    let rdir: Arc<Option<PathBuf>> = Arc::new(opts.repo_dir);

    /// Resolve the GitUndo handle for a call. When `repo_dir` is unset
    /// (or points at a non-repo), fall back to the process cwd's repo
    /// root; if that is also not a repo, report a clean error.
    fn handle(
        v: &serde_json::Value,
        sdir: &Arc<PathBuf>,
        rdir: &Arc<Option<PathBuf>>,
    ) -> Result<GitUndo, String> {
        let state_dir = if let Some(d) = v.get("state_dir").and_then(|x| x.as_str()) {
            PathBuf::from(d)
        } else {
            (**sdir).clone()
        };
        let repo = if let Some(d) = v.get("repo_dir").and_then(|x| x.as_str()) {
            PathBuf::from(d)
        } else {
            rdir.as_ref()
                .clone()
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        };
        let root = repo_root(&repo).ok_or_else(|| {
            "no git repo found at the target dir; git undo is a no-op".to_string()
        })?;
        GitUndo::new(state_dir, root).map_err(|e| e.to_string())
    }

    let mk = |name: &str,
              desc: &str,
              props: serde_json::Value,
              required: Vec<&str>|
     -> ToolSchema {
        ToolSchema {
            name: name.into(),
            description: desc.into(),
            parameters: serde_json::json!({"type":"object","properties":props,"required":required}),
        }
    };

    let s1 = sdir.clone();
    let r1 = rdir.clone();
    reg.register(
        mk(
            "gitundo.snapshot",
            "Snapshot the whole git working tree (tracked + untracked) into a labeled undo point. The user's branch is never moved. Returns the undo point id.",
            serde_json::json!({
                "label": {"type":"string","description":"Short label for the snapshot"},
                "repo_dir": {"type":"string","description":"Git working tree (default: process cwd)"},
                "state_dir": {"type":"string","description":"State dir (default: registration value)"},
            }),
            vec!["label"],
        ),
        Capability::GitWrite,
        move |args| {
            let v = parse_args(args)?;
            let h = handle(&v, &s1, &r1).map_err(|m| err("UNDO_NO_REPO", m))?;
            let label = v
                .get("label")
                .and_then(|x| x.as_str())
                .ok_or_else(|| err("TOOL_BAD_ARGS", "missing 'label'".to_string()))?;
            let point = h.snapshot(label)?;
            Ok(serde_json::to_string_pretty(&point).unwrap_or_else(|_| format!("undo point {}", point.id)))
        },
    );

    let s2 = sdir.clone();
    let r2 = rdir.clone();
    reg.register(
        mk(
            "gitundo.list",
            "List all stored git undo points, newest first.",
            serde_json::json!({
                "repo_dir": {"type":"string"},
                "state_dir": {"type":"string"},
            }),
            vec![],
        ),
        Capability::GitRead,
        move |args| {
            let v = parse_args(args)?;
            let h = handle(&v, &s2, &r2).map_err(|m| err("UNDO_NO_REPO", m))?;
            let points = h.list()?;
            serde_json::to_string_pretty(&points).map_err(|e| err("UNDO_LIST_JSON", e.to_string()))
        },
    );

    let s3 = sdir.clone();
    let r3 = rdir.clone();
    reg.register(
        mk(
            "gitundo.restore",
            "Restore the working tree to a previous git undo point. Tries the stored patch first, falls back to a snapshot checkout. Refuses a corrupt patch.",
            serde_json::json!({
                "undo_id": {"type":"string","description":"Undo point id to restore"},
                "repo_dir": {"type":"string"},
                "state_dir": {"type":"string"},
            }),
            vec!["undo_id"],
        ),
        Capability::GitWrite,
        move |args| {
            let v = parse_args(args)?;
            let h = handle(&v, &s3, &r3).map_err(|m| err("UNDO_NO_REPO", m))?;
            let id = v
                .get("undo_id")
                .and_then(|x| x.as_str())
                .ok_or_else(|| err("TOOL_BAD_ARGS", "missing 'undo_id'".to_string()))?;
            let point = h
                .list()?
                .into_iter()
                .find(|p| p.id == id)
                .ok_or_else(|| err("UNDO_NOT_FOUND", format!("no undo point '{id}' stored")))?;
            let res = h.restore(&point)?;
            serde_json::to_string_pretty(&res).map_err(|e| err("UNDO_RESTORE_JSON", e.to_string()))
        },
    );

    let s4 = sdir.clone();
    let r4 = rdir.clone();
    reg.register(
        mk(
            "gitundo.delete",
            "Delete one git undo point and its ref + patch + manifest.",
            serde_json::json!({
                "undo_id": {"type":"string","description":"Undo point id to delete"},
                "repo_dir": {"type":"string"},
                "state_dir": {"type":"string"},
            }),
            vec!["undo_id"],
        ),
        Capability::GitWrite,
        move |args| {
            let v = parse_args(args)?;
            let h = handle(&v, &s4, &r4).map_err(|m| err("UNDO_NO_REPO", m))?;
            let id = v
                .get("undo_id")
                .and_then(|x| x.as_str())
                .ok_or_else(|| err("TOOL_BAD_ARGS", "missing 'undo_id'".to_string()))?;
            let point = h
                .list()?
                .into_iter()
                .find(|p| p.id == id)
                .ok_or_else(|| err("UNDO_NOT_FOUND", format!("no undo point '{id}' stored")))?;
            h.delete(&point)?;
            Ok(format!("deleted undo point {id}"))
        },
    );
}
