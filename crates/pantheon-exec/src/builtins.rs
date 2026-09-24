//! Built-in tools: shell, read_file, write_file, list_dir.
//! Each maps to its capability and compacts output before it hits context.
//!
//! `write_file` is NOT a plain atomic write — it routes through the
//! safe-writer so every write is checkpointed, journaled, and recoverable.
//! That makes the safe path the default, not an opt-in side door.

use crate::compact_output;
use crate::safewrite::{atomic_write, FileEdit, SafeWriter};
use crate::tools::{parse_args, ToolRegistry};
use pantheon_core::capability::Capability;
use pantheon_core::error::{Layer, PantheonError};
use pantheon_core::message::ToolSchema;
use pantheon_sandbox::{SandboxLevel, SandboxProfile};
use std::path::{Path, PathBuf};

/// Options for `register_builtins`. Empty defaults keep the old call site
/// working; supplying `safewrite_state_dir` routes `write_file` through the
/// SafeWriter instead of leaving the safe path as an opt-in side door.
#[derive(Default)]
pub struct BuiltinOptions {
    pub safewrite_state_dir: Option<std::path::PathBuf>,
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
    reg.register(
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
            crate::danger::gate(&command)?;
            run_shell(args)
        },
    );
    reg.register(
        ToolSchema {
            name: "read_file".into(),
            description: "Read a text file (compacted if very large).".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
        },
        Capability::FilesystemRead,
        |args| {
            let v = parse_args(args)?;
            let path = arg_str(&v, "path")?;
            let raw = std::fs::read_to_string(&path)
                .map_err(|e| berr("TOOL_FS", format!("read {path}: {e}"), false))?;
            Ok(compact_output(&raw, &Default::default()).text)
        },
    );
    reg.register(
        ToolSchema {
            name: "write_file".into(),
            description: "Write a file safely: preview, checkpoint, atomic publish. Fails on stale expected_hash.".into(),
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
            let content = arg_str(&v, "content")?;
            let expected = v
                .get("expected_hash")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            if let Some(ref dir) = sd {
                // Safe path: checkpoint + atomic publish + journal.
                let w = SafeWriter::new(dir.clone()).map_err(|e| {
                    berr("TOOL_FS", format!("open safewrite state {dir:?}: {e}"), false)
                })?;
                // Capture the current fingerprint so stale edits get
                // rejected by apply_edits when no explicit hash is given.
                let expected = expected.or_else(|| {
                    crate::safewrite::fingerprint_of(Path::new(&path))
                        .ok()
                        .map(|fp| fp.hash)
                });
                let receipt = w
                    .apply_edits(
                        vec![FileEdit {
                            path: PathBuf::from(&path),
                            new_content: content.as_bytes().to_vec(),
                            expected_hash: expected,
                        }],
                        -1,
                    )
                    .map_err(|e| berr("TOOL_FS", format!("safewrite apply {path}: {e}"), false))?;
                Ok(format!(
                    "wrote {} bytes to {path}; checkpoint={}",
                    content.len(),
                    receipt.checkpoint_id
                ))
            } else {
                // Unsafe fallback: caller opted out of the safe path.
                atomic_write(Path::new(&path), content.as_bytes())
                    .map_err(|e| berr("TOOL_FS", format!("write {path}: {e}"), false))?;
                Ok(format!("wrote {} bytes to {path}", content.len()))
            }
        },
    );
    reg.register(
        ToolSchema {
            name: "list_dir".into(),
            description: "List a directory's entries, one per line.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
        },
        Capability::FilesystemRead,
        |args| {
            let v = parse_args(args)?;
            let path = arg_str(&v, "path")?;
            let mut names: Vec<String> = std::fs::read_dir(&path)
                .map_err(|e| berr("TOOL_FS", format!("list {path}: {e}"), false))?
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

    let result = pantheon_sandbox::runner::run_sandboxed(
        &profile,
        "sh",
        &["-c", &command],
        &cwd,
    )?;

    let code = result.exit_code;
    let raw = if result.output.is_empty() {
        format!("(exit {code})")
    } else {
        format!("{}\n(exit {code})", result.output)
    };
    let compacted = compact_output(&raw, &Default::default());
    Ok(compacted.text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolRegistry;

    fn fresh(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-builtins-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_file_routes_through_safewrite_when_state_dir_given() {
        let work = fresh("work");
        let state = fresh("state");
        let p = work.join("f.txt");
        std::fs::write(&p, "v1\n").unwrap();
        let mut reg = ToolRegistry::new();
        register_builtins_with(
            &mut reg,
            BuiltinOptions {
                safewrite_state_dir: Some(state.clone()),
            },
        );
        let out = reg
            .execute(
                "write_file",
                &format!(r#"{{"path":"{}","content":"v2\n"}}"#, p.display()),
            )
            .unwrap();
        assert!(out.contains("checkpoint="), "got: {out}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "v2\n");
        // The checkpoint dir must now have a manifest for the file.
        let ckpts: Vec<_> = std::fs::read_dir(state.join("checkpoints"))
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(!ckpts.is_empty());
    }

    #[test]
    fn write_file_stale_check_rejects_mismatch() {
        let work = fresh("work-stale");
        let state = fresh("state-stale");
        let p = work.join("f.txt");
        std::fs::write(&p, "v1\n").unwrap();
        let mut reg = ToolRegistry::new();
        register_builtins_with(
            &mut reg,
            BuiltinOptions {
                safewrite_state_dir: Some(state.clone()),
            },
        );
        // Pretend the file is still at "v0"; the safe path must reject.
        let err = reg
            .execute(
                "write_file",
                &format!(
                    r#"{{"path":"{}","content":"v2\n","expected_hash":"deadbeef"}}"#,
                    p.display()
                ),
            )
            .unwrap_err();
        // Safewrite errors are wrapped in TOOL_FS at the tool boundary.
        // The code on the wire is what callers test against.
        assert_eq!(err.code, "TOOL_FS");
        assert!(
            err.cause.contains("safewrite") || err.cause.contains("stale"),
            "cause should mention stale: {}",
            err.cause
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "v1\n");
    }

    #[test]
    fn write_file_unsafe_fallback_when_no_state_dir() {
        let work = fresh("work-unsafe");
        let p = work.join("f.txt");
        let mut reg = ToolRegistry::new();
        register_builtins(&mut reg);
        let out = reg
            .execute(
                "write_file",
                &format!(r#"{{"path":"{}","content":"hi\n"}}"#, p.display()),
            )
            .unwrap();
        assert!(!out.contains("checkpoint="), "got: {out}");
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hi\n");
    }
}
