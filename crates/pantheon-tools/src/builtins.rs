//! Built-in tools: shell, read_file, write_file, list_dir.
//! Each maps to its capability and compacts output before it hits context.
//!
//! `write_file` is NOT a plain atomic write — it routes through the
//! safe-writer so every write is checkpointed, journaled, and recoverable.
//! That makes the safe path the default, not an opt-in side door.

use crate::tools::{parse_args, ToolRegistry};
use pantheon_api::capability::Capability;
use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::message::ToolSchema;
use pantheon_exec::compact_output;
use pantheon_exec::confine::confine;
use pantheon_exec::safewrite::{atomic_write, FileEdit, SafeWriter};
use pantheon_sandbox::{SandboxLevel, SandboxProfile};
use std::path::{Path, PathBuf};

/// Options for `register_builtins`. Empty defaults keep the old call site
/// working; supplying `safewrite_state_dir` routes `write_file` through the
/// SafeWriter instead of leaving the safe path as an opt-in side door.
#[derive(Default)]
pub struct BuiltinOptions {
    pub safewrite_state_dir: Option<std::path::PathBuf>,
    /// Workspace root for path confinement of the fs tools. The tool layer
    /// has no workspace concept of its own, so when this is `None` the
    /// process working directory is captured here, at registration time.
    /// Every `read_file`/`write_file`/`list_dir` path is confined with
    /// `pantheon_exec::confine` (deny globs, then containment) before any
    /// read or write — a granted `FilesystemRead`/`FilesystemWrite`
    /// capability never widens it.
    pub workspace_root: Option<std::path::PathBuf>,
}

fn berr(code: &str, cause: String, retryable: bool) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Execution,
        retryable,
        cause,
        "check tool arguments",
        "",
    )
}

fn arg_str(v: &serde_json::Value, key: &str) -> Result<String, PantheonError> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            berr(
                "TOOL_BAD_ARGS",
                format!("missing string arg '{key}'"),
                false,
            )
        })
}

/// Register the default toolset on a registry.
pub fn register_builtins(reg: &mut ToolRegistry) {
    register_builtins_with(reg, BuiltinOptions::default());
}

/// Register with options. When `safewrite_state_dir` is set, `write_file`
/// routes through the SafeWriter (checkpoint + journal + atomic publish +
/// stale-hash rejection). Without it, `write_file` falls back to a plain
/// atomic write — kept for callers that explicitly want the unsafe path.
pub fn register_builtins_with(reg: &mut ToolRegistry, opts: BuiltinOptions) {
    let sd = opts.safewrite_state_dir.clone();
    // Workspace root for confinement, captured once at registration: no
    // workspace concept exists in the tool layer, so this is the process
    // cwd unless the caller overrides it. Confine errors propagate with
    // their CONFINE_* codes (deny globs are evaluated before containment,
    // and independently of the capability gate).
    let root: std::sync::Arc<PathBuf> = std::sync::Arc::new(
        opts.workspace_root
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))),
    );
    reg.register_with(
        ToolSchema {
            name: "shell".into(),
            description: "Run a shell command with a timeout. Returns compacted stdout+stderr.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "command": { "type": "string", "description": "The command to run" } },
                "required": ["command"]
            }),
        },
        Capability::ShellExecute,
        |args| {
            // Dangerous-pattern pre-gate: deterministic, in-process, runs
            // before any spawn. Not the security boundary (policy is), but
            // it fails fast on `rm -rf /` class commands and keeps them
            // out of the audit trail as executed calls. run_shell parses
            // the args itself; parse here only for the gate.
            let v = parse_args(args)?;
            let command = arg_str(&v, "command")?;
            pantheon_exec::danger::gate(&command)?;
            run_shell(args)
        },
        Some(Box::new(|args: &str| {
            // `git push` is the one shell operation the policies single
            // out (coder marks git.push Approval). The static ShellExecute
            // capability alone let every push through unapproved, so the
            // command is inspected and the call picks up GitPush.
            let Ok(v) = parse_args(args) else {
                return Vec::new();
            };
            let Some(cmd) = v.get("command").and_then(|c| c.as_str()) else {
                return Vec::new();
            };
            if pantheon_exec::danger::is_git_push(cmd) {
                vec![Capability::GitPush]
            } else {
                Vec::new()
            }
        })),
    );
    let r0 = root.clone();
    reg.register(
        ToolSchema {
            name: "read_file".into(),
            description: "Read a text file (compacted if very large). Confined to the workspace."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
        },
        Capability::FilesystemRead,
        move |args| {
            let v = parse_args(args)?;
            let path = arg_str(&v, "path")?;
            let cpath = confine(Path::new(&path), &r0)?;
            let raw = std::fs::read_to_string(&cpath)
                .map_err(|e| berr("TOOL_FS", format!("read {}: {e}", cpath.display()), false))?;
            Ok(compact_output(&raw, &Default::default()).text)
        },
    );
    let r1 = root.clone();
    reg.register(
        ToolSchema {
            name: "write_file".into(),
            description: "Write a file safely: preview, checkpoint, atomic publish. Fails on stale expected_hash. Confined to the workspace.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" },
                    "expected_hash": { "type": "string", "description": "Optional stale-edit guard. When omitted, a fresh fingerprint is captured first." }
                },
                "required": ["path", "content"]
            }),
        },
        Capability::FilesystemWrite,
        move |args| {
            let v = parse_args(args)?;
            let path = arg_str(&v, "path")?;
            // Confine before either write path (safe or unsafe fallback).
            let cpath = confine(Path::new(&path), &r1)?;
            let content = arg_str(&v, "content")?;
            let expected = v
                .get("expected_hash")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            if let Some(ref dir) = sd {
                // Safe path: checkpoint + atomic publish + journal.
                let w = SafeWriter::new(dir.clone())
                    .map_err(|e| {
                        berr("TOOL_FS", format!("open safewrite state {dir:?}: {e}"), false)
                    })?
                    .with_workspace_root((*r1).clone());
                // Capture the current fingerprint so stale edits get
                // rejected by apply_edits when no explicit hash is given.
                let expected = expected.or_else(|| {
                    pantheon_exec::safewrite::fingerprint_of(&cpath)
                        .ok()
                        .map(|fp| fp.hash)
                });
                let receipt = w
                    .apply_edits(
                        vec![FileEdit {
                            path: cpath.clone(),
                            new_content: content.as_bytes().to_vec(),
                            expected_hash: expected,
                        }],
                        -1,
                    )
                    .map_err(|e| {
                        berr(
                            "TOOL_FS",
                            format!("safewrite apply {}: {e}", cpath.display()),
                            false,
                        )
                    })?;
                Ok(format!(
                    "wrote {} bytes to {}; checkpoint={}",
                    content.len(),
                    cpath.display(),
                    receipt.checkpoint_id
                ))
            } else {
                // Unsafe fallback: caller opted out of the safe path.
                atomic_write(&cpath, content.as_bytes())
                    .map_err(|e| {
                        berr("TOOL_FS", format!("write {}: {e}", cpath.display()), false)
                    })?;
                Ok(format!("wrote {} bytes to {}", content.len(), cpath.display()))
            }
        },
    );
    let r2 = root.clone();
    reg.register(
        ToolSchema {
            name: "list_dir".into(),
            description: "List a directory's entries, one per line. Confined to the workspace."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
        },
        Capability::FilesystemRead,
        move |args| {
            let v = parse_args(args)?;
            let path = arg_str(&v, "path")?;
            let cpath = confine(Path::new(&path), &r2)?;
            let mut names: Vec<String> = std::fs::read_dir(&cpath)
                .map_err(|e| berr("TOOL_FS", format!("list {}: {e}", cpath.display()), false))?
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            Ok(names.join("\n"))
        },
    );
}

fn run_shell(args: &str) -> Result<String, PantheonError> {
    let v = parse_args(args)?;
    let command = arg_str(&v, "command")?;
    // Shell runs at HIGH isolation: bwrap with dropped caps + no-new-privs
    // + rlimits. The capability gate already ran before we get here.
    let profile = SandboxProfile::from(SandboxLevel::High);
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "/tmp".to_string());

    let result = pantheon_sandbox::runner::run_sandboxed(&profile, "sh", &["-c", &command], &cwd)?;

    let code = result.exit_code;
    let raw = if result.output.is_empty() {
        format!("(exit {code})")
    } else {
        format!("{}\n(exit {code})", result.output)
    };
    // The runner reports whether isolation actually happened. By default
    // it fails closed: on a host without a working wrapper (a common
    // case — most EC2/container instances block `bwrap`'s uid map) the
    // call above already returned a SANDBOX_UNAVAILABLE error. A
    // `sandboxed == false` result is only possible with the direct
    // fallback explicitly opted in (PANTHEON_SANDBOX_FALLBACK=allow or
    // allow_direct_fallback); say so in the result rather than silently
    // presenting degraded isolation as HIGH.
    let notice = if result.sandboxed {
        String::new()
    } else {
        "[sandbox unavailable on this host: ran WITHOUT namespace isolation]\n".to_string()
    };
    let compacted = compact_output(&format!("{notice}{raw}"), &Default::default());
    Ok(compacted.text)
}

#[cfg(test)]
#[path = "builtins_tests.rs"]
mod tests;
