//! Inline unified diffs for file-editing tool calls.
//!
//! The TUI snapshots files named by `apply_files` at `ToolStarted` and
//! diffs them at `ToolCompleted`, so the transcript shows what actually
//! changed - red `-` lines, green `+` lines, `@@` hunk headers - instead
//! of just the tool's argument summary. New files render all-`+`,
//! deletions all-`-`, binaries are skipped, and large diffs truncate with
//! an honest "N more lines" marker.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::path::{Path, PathBuf};

/// Files shown per tool card at most.
pub const MAX_DIFF_FILES: usize = 5;
/// Diff body lines per file at most (headers excluded).
pub const MAX_DIFF_LINES: usize = 60;
/// A single file is never snapshotted past this.
pub const MAX_SNAP_BYTES: usize = 1024 * 1024;

/// Content of one file captured at `ToolStarted`.
#[derive(Debug, Clone)]
pub struct FileSnapshot {
    pub path: PathBuf,
    /// `None` when the file did not exist (it may be created).
    pub content: Option<String>,
}

/// Read `path` for later diffing. Returns `None` for missing files
/// (recorded as `content: None` by the caller), binaries, and files over
/// [`MAX_SNAP_BYTES`].
pub fn snapshot_file(path: &Path) -> Option<FileSnapshot> {
    match std::fs::read(path) {
        Ok(data) => {
            if data.len() > MAX_SNAP_BYTES || data.iter().take(8192).any(|&b| b == 0) {
                None
            } else {
                Some(FileSnapshot {
                    path: path.to_path_buf(),
                    content: Some(String::from_utf8_lossy(&data).into_owned()),
                })
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(FileSnapshot {
            path: path.to_path_buf(),
            content: None,
        }),
        Err(_) => None,
    }
}

/// Parse the `edits` array out of `apply_files` args JSON. Returns the
/// target paths; anything unparseable yields an empty vec (no diff, no
/// error - diffing is best-effort display).
///
/// Only `apply_files` is hooked: `stage_files` stages without touching
/// targets (nothing to diff), and `apply_staged` applies by opaque stage
/// id, so its paths are not visible in the args.
pub fn edit_targets(tool: &str, args: &str) -> Vec<PathBuf> {
    if tool != "apply_files" {
        return Vec::new();
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(args) else {
        return Vec::new();
    };
    v.get("edits")
        .and_then(|e| e.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.get("path").and_then(|p| p.as_str()))
                .map(PathBuf::from)
                .collect()
        })
        .unwrap_or_default()
}

/// One rendered diff line, style-agnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    FileHeader(String),
    Hunk(String),
    Context(String),
    Add(String),
    Del(String),
    Truncated(usize),
}

/// Line-based diff via LCS on lines. Simple and predictable; inputs are
/// capped by [`MAX_SNAP_BYTES`] so the O(n·m) table stays small in
/// practice (typical edits are tens of lines).
pub fn unified_diff(old: &str, new: &str) -> Vec<DiffLine> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    // LCS table.
    let (n, m) = (a.len(), b.len());
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }
    // Walk the table into an edit script.
    #[derive(PartialEq, Clone, Copy)]
    enum Op<'x> {
        Same(&'x str),
        Del(&'x str),
        Add(&'x str),
    }
    let mut ops: Vec<Op> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push(Op::Same(a[i]));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            ops.push(Op::Del(a[i]));
            i += 1;
        } else {
            ops.push(Op::Add(b[j]));
            j += 1;
        }
    }
    while i < n {
        ops.push(Op::Del(a[i]));
        i += 1;
    }
    while j < m {
        ops.push(Op::Add(b[j]));
        j += 1;
    }

    // Group into hunks with 3 lines of context.
    const CTX: usize = 3;
    fn close_hunk<'x>(s: usize, last: usize, ops: &[Op<'x>], ctx: usize) -> (usize, Vec<Op<'x>>) {
        let end = (last + ctx + 1).min(ops.len());
        (s, ops[s..end].to_vec())
    }
    let mut hunks: Vec<(usize, Vec<Op>)> = Vec::new();
    let mut start: Option<usize> = None;
    let mut last_change = 0usize;
    for (k, op) in ops.iter().enumerate() {
        if !matches!(op, Op::Same(_)) {
            if start.is_none() {
                start = Some(k.saturating_sub(CTX));
            }
            last_change = k;
        } else if let Some(s) = start {
            if k - last_change > CTX * 2 {
                hunks.push(close_hunk(s, last_change, &ops, CTX));
                start = None;
            }
        }
    }
    if let Some(s) = start {
        hunks.push(close_hunk(s, last_change, &ops, CTX));
    }

    let mut out = Vec::new();
    for (s, hunk_ops) in hunks {
        // Hunk header with old/new line ranges.
        let (mut ao, mut bo) = (0usize, 0usize);
        for op in ops.iter().take(s) {
            match op {
                Op::Same(_) => {
                    ao += 1;
                    bo += 1;
                }
                Op::Del(_) => ao += 1,
                Op::Add(_) => bo += 1,
            }
        }
        let (mut ac, mut bc) = (0usize, 0usize);
        for op in &hunk_ops {
            match op {
                Op::Same(_) => {
                    ac += 1;
                    bc += 1;
                }
                Op::Del(_) => ac += 1,
                Op::Add(_) => bc += 1,
            }
        }
        out.push(DiffLine::Hunk(format!(
            "@@ -{},{} +{},{} @@",
            ao + 1,
            ac,
            bo + 1,
            bc
        )));
        for op in hunk_ops {
            match op {
                Op::Same(l) => out.push(DiffLine::Context((*l).to_string())),
                Op::Del(l) => out.push(DiffLine::Del((*l).to_string())),
                Op::Add(l) => out.push(DiffLine::Add((*l).to_string())),
            }
        }
    }
    out
}

/// Diff one snapshot against current disk content. `None` means "no diff
/// to show" (binary, unreadable, or unchanged).
pub fn diff_snapshot(snap: &FileSnapshot) -> Option<Vec<DiffLine>> {
    let after = std::fs::read(&snap.path).ok()?;
    if after.iter().take(8192).any(|&b| b == 0) {
        return None;
    }
    let after = String::from_utf8_lossy(&after).into_owned();
    let before = snap.content.as_deref().unwrap_or("");
    if before == after {
        return None;
    }
    let mut lines = vec![DiffLine::FileHeader(snap.path.display().to_string())];
    let mut body = unified_diff(before, &after);
    if body.len() > MAX_DIFF_LINES {
        let skipped = body.len() - MAX_DIFF_LINES;
        body.truncate(MAX_DIFF_LINES);
        body.push(DiffLine::Truncated(skipped));
    }
    lines.extend(body);
    Some(lines)
}

/// Style diff lines with the theme: red `-`, green `+`, dim headers.
pub fn render_diff_lines(
    lines: &[DiffLine],
    th: &crate::session::theme::Theme,
) -> Vec<Line<'static>> {
    lines
        .iter()
        .map(|l| match l {
            DiffLine::FileHeader(p) => Line::from(Span::styled(
                format!("│  diff {p}"),
                Style::default().fg(th.primary).add_modifier(Modifier::BOLD),
            )),
            DiffLine::Hunk(h) => {
                Line::from(Span::styled(format!("│  {h}"), Style::default().fg(th.dim)))
            }
            DiffLine::Context(c) => Line::from(format!("│   {c}")),
            DiffLine::Add(a) => Line::from(Span::styled(
                format!("│  +{a}"),
                Style::default().fg(th.success),
            )),
            DiffLine::Del(d) => Line::from(Span::styled(
                format!("│  -{d}"),
                Style::default().fg(th.failure),
            )),
            DiffLine::Truncated(n) => Line::from(Span::styled(
                format!("│  ... {n} more lines"),
                Style::default().fg(th.dim).add_modifier(Modifier::ITALIC),
            )),
        })
        .collect()
}
