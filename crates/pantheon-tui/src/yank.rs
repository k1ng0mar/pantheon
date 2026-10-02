//! Yank: copy the last assistant message - or one of its code blocks
//! to the system clipboard.
//!
//! Clipboard backends are probed in order and the first present wins:
//! `wl-copy` (Wayland), `xclip`, `xsel` (X11), `pbcopy` (macOS). All
//! best-effort: when none is installed the caller reports it instead of
//! failing.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// One fenced code block: optional language tag and the code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeBlock {
    pub language: Option<String>,
    pub code: String,
}

/// Extract ``` fenced code blocks from markdown, in order. An unclosed
/// fence runs to the end of the text. Inline backticks are ignored.
pub fn extract_code_blocks(text: &str) -> Vec<CodeBlock> {
    let mut out = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("```") {
            let language = {
                let lang = rest.trim();
                if lang.is_empty() {
                    None
                } else {
                    Some(lang.to_string())
                }
            };
            let mut code = String::new();
            for l in lines.by_ref() {
                if l.trim_start().starts_with("```") {
                    break;
                }
                code.push_str(l);
                code.push('\n');
            }
            out.push(CodeBlock { language, code });
        }
    }
    out
}

/// Backend probing, split from PATH lookup for testability.
pub fn find_clipboard(search_dirs: &[PathBuf]) -> Option<(&'static str, PathBuf)> {
    use std::os::unix::fs::PermissionsExt as _;
    // (binary, argv): wl-copy and pbcopy read stdin; xclip/xsel need flags.
    for (bin, _args) in [
        ("wl-copy", &[][..]),
        ("xclip", &["-selection", "clipboard"][..]),
        ("xsel", &["--clipboard", "--input"][..]),
        ("pbcopy", &[][..]),
    ] {
        let found = search_dirs.iter().map(|d| d.join(bin)).find(|p| {
            p.metadata()
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        });
        if let Some(path) = found {
            return Some((bin, path));
        }
    }
    None
}

fn path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

/// Build the clipboard command for an explicit backend, or rather for
/// the `(name, path)` pair discovery returns. Split from PATH lookup so
/// tests assert argv without executing anything. The payload travels
/// over stdin (never argv), which the caller writes.
pub fn clipboard_command_for(bin: &str, path: &Path) -> Command {
    let mut cmd = Command::new(path);
    match bin {
        "xclip" => {
            cmd.arg("-selection").arg("clipboard");
        }
        "xsel" => {
            cmd.arg("--clipboard").arg("--input");
        }
        _ => {}
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

/// Build the clipboard command, or `None` when no backend exists.
pub fn clipboard_command() -> Option<Command> {
    let (bin, path) = find_clipboard(&path_dirs())?;
    Some(clipboard_command_for(bin, &path))
}

/// Copy `text` to the clipboard via the first available backend.
/// Returns the backend name, or `None` when nothing is installed.
pub fn copy_to_clipboard(text: &str) -> Option<&'static str> {
    let (bin, path) = find_clipboard(&path_dirs())?;
    let mut child = clipboard_command_for(bin, &path).spawn().ok()?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    match child.wait() {
        Ok(s) if s.success() => Some(bin),
        _ => None,
    }
}
