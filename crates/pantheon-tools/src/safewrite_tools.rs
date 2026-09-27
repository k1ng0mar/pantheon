//! Safe-write toolset: preview/stage/apply/checkpoint/rollback tools.
//!
//! Moved out of `pantheon-exec::safewrite` in the tool-layer split
//! (capability ≠ tool): the write engine (checkpoints, hash-chained
//! journal, atomic apply) stays in `pantheon-exec`; registering the tools
//! on a registry is a tool-layer concern.

use crate::tools::{parse_args, ToolRegistry};
use pantheon_api::capability::Capability;
use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::message::ToolSchema;
use pantheon_exec::safewrite::{json_out, preview_edit, serr, FileEdit, SafeWriter};
use std::path::{Path, PathBuf};
fn jstr(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
}
fn tool_err(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Execution,
        false,
        cause,
        "check tool arguments",
        "",
    )
}
fn parse_edit_list(v: &serde_json::Value) -> Result<Vec<FileEdit>, PantheonError> {
    let arr = v
        .get("edits")
        .and_then(|x| x.as_array())
        .ok_or_else(|| tool_err("TOOL_BAD_ARGS", "missing array 'edits'".into()))?;
    let mut out = vec![];
    for item in arr {
        let path = item
            .get("path")
            .and_then(|x| x.as_str())
            .ok_or_else(|| tool_err("TOOL_BAD_ARGS", "each edit needs string 'path'".into()))?;
        let content = item
            .get("content")
            .and_then(|x| x.as_str())
            .ok_or_else(|| tool_err("TOOL_BAD_ARGS", "each edit needs string 'content'".into()))?;
        out.push(FileEdit {
            path: PathBuf::from(path),
            new_content: content.as_bytes().to_vec(),
            expected_hash: item
                .get("expected_hash")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()),
        });
    }
    Ok(out)
}
fn state_dir_from(args: &serde_json::Value) -> Result<PathBuf, PantheonError> {
    if let Some(d) = jstr(args, "state_dir") {
        return Ok(PathBuf::from(d));
    }
    if let Ok(d) = std::env::var("PANTHEON_DATA_DIR") {
        return Ok(PathBuf::from(d).join("safewrite"));
    }
    Err(tool_err(
        "TOOL_BAD_ARGS",
        "missing 'state_dir' (or set PANTHEON_DATA_DIR)".into(),
    ))
}
/// Register the safe-write toolset on a registry. All mutating tools
/// snapshot a checkpoint first; preview/checkpoints/rollback never mutate targets.
/// Tools: preview_file, stage_files, apply_files, apply_staged,
/// checkpoint_files, list_checkpoints, rollback_checkpoint, rollback_seq.
pub fn register_safewrite(reg: &mut ToolRegistry, default_state_dir: PathBuf) {
    let dir: std::sync::Arc<PathBuf> = std::sync::Arc::new(default_state_dir);
    let mk = |name: &str, desc: &str, props: serde_json::Value, required: Vec<&str>| ToolSchema {
        name: name.into(),
        description: desc.into(),
        parameters: serde_json::json!({"type":"object","properties":props,"required":required}),
    };
    let d0 = dir.clone();
    reg.register(
        mk(
            "preview_file",
            "Preview a file write: hashes, line counts, 40-line excerpt. Read-only.",
            serde_json::json!({"path":{"type":"string"},"content":{"type":"string"}}),
            vec!["path", "content"],
        ),
        Capability::FilesystemRead,
        move |args| {
            let v = parse_args(args)?;
            let path = jstr(&v, "path")
                .ok_or_else(|| tool_err("TOOL_BAD_ARGS", "missing 'path'".into()))?;
            let content = jstr(&v, "content")
                .ok_or_else(|| tool_err("TOOL_BAD_ARGS", "missing 'content'".into()))?;
            let _ = &d0;
            let pv = preview_edit(Path::new(&path), content.as_bytes())?;
            serde_json::to_string_pretty(&pv).map_err(|e| serr("SAFE_WRITE_JSON", e.to_string()))
        },
    );
    let d1 = dir.clone();
    reg.register(
        mk(
            "stage_files",
            "Stage file edits without touching targets. Returns stage id + baselines.",
            serde_json::json!({"edits":{"type":"array"},"state_dir":{"type":"string"}}),
            vec!["edits"],
        ),
        Capability::FilesystemWrite,
        move |args| {
            let v = parse_args(args)?;
            let sd = state_dir_from(&v).unwrap_or_else(|_| (*d1).clone());
            let w = SafeWriter::new(sd)?;
            let batch = w.stage_edits(parse_edit_list(&v)?)?;
            serde_json::to_string_pretty(&batch).map_err(|e| serr("SAFE_WRITE_JSON", e.to_string()))
        },
    );
    let d2 = dir.clone();
    reg.register(mk("apply_files", "Validate + checkpoint + atomically apply edits. Fails on stale expected_hash.",
        serde_json::json!({"edits":{"type":"array"},"ledger_seq":{"type":"integer"},"state_dir":{"type":"string"}}), vec!["edits"]),
        Capability::FilesystemWrite, move |args| {
            let v = parse_args(args)?;
            let sd = state_dir_from(&v).unwrap_or_else(|_| (*d2).clone());
            let w = SafeWriter::new(sd)?;
            let seq = v.get("ledger_seq").and_then(|x| x.as_i64()).unwrap_or(-1);
            let r = w.apply_edits(parse_edit_list(&v)?, seq)?;
            json_out(&r)
        });
    let d3 = dir.clone();
    reg.register(mk("apply_staged", "Atomically apply a staged batch by id.",
        serde_json::json!({"stage_id":{"type":"string"},"ledger_seq":{"type":"integer"},"state_dir":{"type":"string"}}), vec!["stage_id"]),
        Capability::FilesystemWrite, move |args| {
            let v = parse_args(args)?;
            let sd = state_dir_from(&v).unwrap_or_else(|_| (*d3).clone());
            let w = SafeWriter::new(sd)?;
            let sid = jstr(&v, "stage_id").ok_or_else(|| tool_err("TOOL_BAD_ARGS", "missing 'stage_id'".into()))?;
            let seq = v.get("ledger_seq").and_then(|x| x.as_i64()).unwrap_or(-1);
            let r = w.apply_staged(&sid, seq)?;
            json_out(&r)
        });
    let d4 = dir.clone();
    reg.register(mk("checkpoint_files", "Snapshot pre-images for paths, anchored to a ledger seq.",
        serde_json::json!({"paths":{"type":"array"},"ledger_seq":{"type":"integer"},"state_dir":{"type":"string"}}), vec!["paths"]),
        Capability::FilesystemWrite, move |args| {
            let v = parse_args(args)?;
            let sd = state_dir_from(&v).unwrap_or_else(|_| (*d4).clone());
            let w = SafeWriter::new(sd)?;
            let paths: Vec<PathBuf> = v.get("paths").and_then(|x| x.as_array()).map(|a| a.iter()
                .filter_map(|x| x.as_str()).map(PathBuf::from).collect()).unwrap_or_default();
            let seq = v.get("ledger_seq").and_then(|x| x.as_i64()).unwrap_or(-1);
            let cp = w.checkpoint(&paths, seq)?;
            json_out(&cp)
        });
    let d5 = dir.clone();
    reg.register(
        mk(
            "list_checkpoints",
            "List checkpoints with ledger anchors.",
            serde_json::json!({"state_dir":{"type":"string"}}),
            vec![],
        ),
        Capability::FilesystemRead,
        move |args| {
            let v = parse_args(args)?;
            let sd = state_dir_from(&v).unwrap_or_else(|_| (*d5).clone());
            let w = SafeWriter::new(sd)?;
            let list = w.list_checkpoints()?;
            json_out(&list)
        },
    );
    let d6 = dir.clone();
    reg.register(
        mk(
            "rollback_checkpoint",
            "Restore a checkpoint by id (atomic per file).",
            serde_json::json!({"checkpoint_id":{"type":"string"},"state_dir":{"type":"string"}}),
            vec!["checkpoint_id"],
        ),
        Capability::FilesystemWrite,
        move |args| {
            let v = parse_args(args)?;
            let sd = state_dir_from(&v).unwrap_or_else(|_| (*d6).clone());
            let w = SafeWriter::new(sd)?;
            let id = jstr(&v, "checkpoint_id")
                .ok_or_else(|| tool_err("TOOL_BAD_ARGS", "missing 'checkpoint_id'".into()))?;
            let restored = w.restore_checkpoint(&id)?;
            Ok(serde_json::to_string_pretty(
                &serde_json::json!({"checkpoint": id, "restored": restored}),
            )
            .unwrap())
        },
    );
    let d7 = dir;
    reg.register(
        mk(
            "rollback_seq",
            "Restore the latest checkpoint at or before a ledger seq.",
            serde_json::json!({"ledger_seq":{"type":"integer"},"state_dir":{"type":"string"}}),
            vec!["ledger_seq"],
        ),
        Capability::FilesystemWrite,
        move |args| {
            let v = parse_args(args)?;
            let sd = state_dir_from(&v).unwrap_or_else(|_| (*d7).clone());
            let w = SafeWriter::new(sd)?;
            let seq = v
                .get("ledger_seq")
                .and_then(|x| x.as_i64())
                .ok_or_else(|| tool_err("TOOL_BAD_ARGS", "missing integer 'ledger_seq'".into()))?;
            let (id, restored) = w.rollback_to_seq(seq)?;
            Ok(serde_json::to_string_pretty(
                &serde_json::json!({"checkpoint": id, "restored": restored}),
            )
            .unwrap())
        },
    );
}
