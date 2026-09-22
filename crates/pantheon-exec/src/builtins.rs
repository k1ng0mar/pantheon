//! Built-in tools: shell, read_file, write_file, list_dir.
//! Each maps to its capability and compacts output before it hits context.

use crate::compact_output;
use crate::safewrite::atomic_write;
use crate::tools::{parse_args, ToolRegistry};
use pantheon_core::capability::Capability;
use pantheon_core::error::{Layer, PantheonError};
use pantheon_core::message::ToolSchema;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

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
        |args| run_shell(args),
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
            description: "Write a text file atomically (tmp+fsync+rename, creates parents).".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        },
        Capability::FilesystemWrite,
        |args| {
            let v = parse_args(args)?;
            let path = arg_str(&v, "path")?;
            let content = arg_str(&v, "content")?;
            atomic_write(Path::new(&path), content.as_bytes())?;
            Ok(format!("wrote {} bytes to {path}", content.len()))
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
    // sh -c, output captured, 60s timeout via wait_timeout-style polling.
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&command)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| berr("TOOL_SPAWN", format!("spawn: {e}"), false))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                if let Some(mut s) = child.stdout.take() {
                    use std::io::Read;
                    s.read_to_string(&mut out).ok();
                }
                if let Some(mut s) = child.stderr.take() {
                    use std::io::Read;
                    let mut e = String::new();
                    s.read_to_string(&mut e).ok();
                    out.push_str(&e);
                }
                let code = status.code().unwrap_or(-1);
                let raw = if out.is_empty() {
                    format!("(exit {code})")
                } else {
                    format!("{out}\n(exit {code})")
                };
                let compacted = compact_output(&raw, &Default::default());
                return Ok(compacted.text);
            }
            Ok(None) => {
                if std::time::Instant::now() > deadline {
                    let _ = child.kill();
                    return Err(berr(
                        "TOOL_TIMEOUT",
                        format!("command exceeded 60s: {command}"),
                        true,
                    ));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(berr("TOOL_WAIT", e.to_string(), false)),
        }
    }
}
