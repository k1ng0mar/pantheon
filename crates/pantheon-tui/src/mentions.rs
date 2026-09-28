//! `@` file mentions: parse `@path` tokens from the prompt, offer a fuzzy
//! file picker, and attach file contents as context on submit.
//!
//! The picker lists files under the working directory, respecting
//! `.gitignore` and always skipping VCS/build noise (`.git`, `target`,
//! `node_modules`, …). On submit, `resolve_mentions` reads each mentioned
//! file: text goes to the model inside a delimited block, binaries and
//! unreadable files are reported and skipped, and total attached bytes are
//! capped so one `@` cannot blow the context window.

use std::path::{Path, PathBuf};

/// Most candidates the picker ever shows.
pub const MAX_RESULTS: usize = 100;
/// Total bytes of file content attached to one submitted prompt.
pub const MAX_ATTACH_BYTES: usize = 200 * 1024;
/// A single file is never read past this (protects the picker too).
pub const MAX_FILE_BYTES: usize = 1024 * 1024;
/// Directories never descended into, even without a .gitignore.
const ALWAYS_SKIP: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".tox",
    "dist",
    "build",
    ".next",
    ".nuxt",
    ".cache",
];

/// Extract `@path` mentions from prompt text, in order of appearance.
///
/// A mention starts at `@` and runs to the next whitespace. A lone `@`
/// (end of input, or followed by whitespace) is not a mention — it is the
/// trigger that opens the picker.
pub fn extract_mentions(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' {
            let mut j = i + 1;
            while j < bytes.len() && !bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if j > i + 1 {
                out.push(text[i + 1..j].to_string());
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// True when `path` looks binary: a NUL byte in the first 8 KiB.
pub fn is_binary(data: &[u8]) -> bool {
    data.iter().take(8192).any(|&b| b == 0)
}

/// Minimal `.gitignore` matcher: `*` wildcards, trailing `/` for dirs,
/// `!` negation is not supported (documented; rare in practice).
#[derive(Debug, Default)]
pub struct IgnoreRules {
    patterns: Vec<String>,
}

impl IgnoreRules {
    pub fn load(dir: &Path) -> Self {
        let mut rules = Self::default();
        if let Ok(text) = std::fs::read_to_string(dir.join(".gitignore")) {
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                rules
                    .patterns
                    .push(line.trim_start_matches('/').to_string());
            }
        }
        rules
    }

    fn matches(&self, rel: &str, is_dir: bool) -> bool {
        for pat in &self.patterns {
            let (pat, dir_only) = match pat.strip_suffix('/') {
                Some(p) => (p, true),
                None => (pat.as_str(), false),
            };
            if dir_only && !is_dir {
                continue;
            }
            if glob_match(pat, rel) || rel.split('/').any(|seg| glob_match(pat, seg)) {
                return true;
            }
        }
        false
    }
}

/// `*`/`?` glob over a single path segment (no `/` crossing).
fn glob_match(pat: &str, name: &str) -> bool {
    let (p, n) = (pat.as_bytes(), name.as_bytes());
    // Recursive matcher; patterns are tiny.
    fn go(p: &[u8], n: &[u8]) -> bool {
        if p.is_empty() {
            return n.is_empty();
        }
        match p[0] {
            b'*' => {
                // Collapse runs of stars.
                let mut p = &p[1..];
                while p.first() == Some(&b'*') {
                    p = &p[1..];
                }
                if p.is_empty() {
                    return true;
                }
                for i in 0..=n.len() {
                    if go(p, &n[i..]) {
                        return true;
                    }
                }
                false
            }
            b'?' => !n.is_empty() && go(&p[1..], &n[1..]),
            c => !n.is_empty() && n[0] == c && go(&p[1..], &n[1..]),
        }
    }
    go(p, n)
}

/// Case-insensitive subsequence (fuzzy) match. Lower score is better.
fn fuzzy_score(query: &str, candidate: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(0);
    }
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let c: Vec<char> = candidate.to_lowercase().chars().collect();
    let (mut qi, mut score, mut consecutive) = (0usize, 0u32, 0u32);
    for (i, &ch) in c.iter().enumerate() {
        if qi < q.len() && ch == q[qi] {
            // Bonus for matches at segment starts or consecutive runs.
            let boundary = i == 0 || c[i - 1] == '/' || c[i - 1] == '_' || c[i - 1] == '-';
            score += i as u32 + if boundary { 0 } else { 5 } - consecutive.min(4);
            consecutive += 1;
            qi += 1;
        } else {
            consecutive = 0;
        }
    }
    if qi == q.len() {
        Some(score)
    } else {
        None
    }
}

/// List files under `dir` matching `query`, best matches first, capped at
/// [`MAX_RESULTS`]. Respects `.gitignore` and [`ALWAYS_SKIP`].
pub fn list_files(dir: &Path, query: &str) -> Vec<String> {
    let rules = IgnoreRules::load(dir);
    let mut found: Vec<(u32, String)> = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    // Bound the walk so giant trees cannot hang the picker.
    let mut visited = 0usize;
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > 20_000 {
                break;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            let ft = entry.file_type().ok();
            let is_dir = ft.map(|t| t.is_dir()).unwrap_or(false);
            if is_dir && ALWAYS_SKIP.contains(&name.as_str()) {
                continue;
            }
            let Ok(rel) = path.strip_prefix(dir) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            if rules.matches(&rel, is_dir) {
                continue;
            }
            if is_dir {
                stack.push(path);
                continue;
            }
            if let Some(score) = fuzzy_score(query, &rel) {
                found.push((score, rel));
            }
            if found.len() >= MAX_RESULTS * 4 {
                // Enough raw candidates; ranking trims to MAX_RESULTS.
                break;
            }
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.len().cmp(&b.1.len())));
    found.truncate(MAX_RESULTS);
    found.into_iter().map(|(_, p)| p).collect()
}

/// One attached file: its mention path and what happened.
#[derive(Debug)]
pub enum Attachment {
    /// Text content, possibly truncated to the byte budget.
    Text {
        path: String,
        content: String,
        truncated: bool,
    },
    /// Skipped with a human reason shown in the transcript.
    Skipped { path: String, reason: String },
}

/// Resolve `@` mentions in `text` against `cwd`.
///
/// Returns `(display, model)`: `display` is what the transcript shows
/// (mentions annotated with size/skip reason), `model` is what the agent
/// receives (original text plus a delimited attachment section).
pub fn resolve_mentions(text: &str, cwd: &Path) -> (String, String) {
    let mentions = extract_mentions(text);
    if mentions.is_empty() {
        return (text.to_string(), text.to_string());
    }
    let mut attachments: Vec<Attachment> = Vec::new();
    let mut budget = MAX_ATTACH_BYTES;
    let mut seen = std::collections::HashSet::new();
    // Confine every mention to the working directory: no `..` escapes,
    // no absolute paths outside the tree.
    let canon_cwd = cwd.canonicalize().ok();
    for m in mentions {
        if !seen.insert(m.clone()) {
            continue;
        }
        let disk = cwd.join(&m);
        let Some(root) = canon_cwd.as_deref() else {
            attachments.push(Attachment::Skipped {
                path: m,
                reason: "cannot resolve working directory".into(),
            });
            continue;
        };
        let target = match disk.canonicalize() {
            Ok(p) => p,
            Err(_) => {
                // Name the common case honestly: a mention that matches
                // nothing is "no such file", not a sandbox violation.
                let reason = if disk.symlink_metadata().is_err() {
                    "no such file"
                } else {
                    "cannot resolve path"
                };
                attachments.push(Attachment::Skipped {
                    path: m,
                    reason: reason.into(),
                });
                continue;
            }
        };
        if !target.starts_with(root) {
            attachments.push(Attachment::Skipped {
                path: m,
                reason: "outside working directory".into(),
            });
            continue;
        }
        if target.is_dir() {
            attachments.push(Attachment::Skipped {
                path: m,
                reason: "is a directory".into(),
            });
            continue;
        }
        let data = match std::fs::read(&target) {
            Ok(d) => d,
            Err(e) => {
                attachments.push(Attachment::Skipped {
                    path: m,
                    reason: format!("unreadable: {e}"),
                });
                continue;
            }
        };
        if data.len() > MAX_FILE_BYTES {
            attachments.push(Attachment::Skipped {
                path: m,
                reason: format!("over {} file limit", fmt_bytes(MAX_FILE_BYTES)),
            });
            continue;
        }
        if is_binary(&data) {
            attachments.push(Attachment::Skipped {
                path: m,
                reason: "binary file".into(),
            });
            continue;
        }
        let content = String::from_utf8_lossy(&data).into_owned();
        if budget == 0 {
            attachments.push(Attachment::Skipped {
                path: m,
                reason: format!("over {} attach budget", fmt_bytes(MAX_ATTACH_BYTES)),
            });
            continue;
        }
        let mut take = budget.min(content.len());
        // Never split a UTF-8 char: back up to the char boundary.
        while take > 0 && !content.is_char_boundary(take) {
            take -= 1;
        }
        let truncated = take < content.len();
        budget -= take;
        attachments.push(Attachment::Text {
            path: m,
            content: content[..take].to_string(),
            truncated,
        });
    }

    // Display: annotate each mention in place.
    let mut display = text.to_string();
    for a in &attachments {
        let (path, note) = match a {
            Attachment::Text {
                path,
                content,
                truncated,
            } => (
                path.clone(),
                format!("attached, {}", fmt_bytes(content.len()))
                    + if *truncated { " (truncated)" } else { "" },
            ),
            Attachment::Skipped { path, reason } => (path.clone(), format!("skipped: {reason}")),
        };
        display = display.replacen(&format!("@{path}"), &format!("@{path} [{note}]"), 1);
    }

    // Model: original text plus the attachment section.
    let mut model = text.to_string();
    let texts: Vec<&Attachment> = attachments
        .iter()
        .filter(|a| matches!(a, Attachment::Text { .. }))
        .collect();
    if !texts.is_empty() {
        model.push_str("\n\n<attached_files>\n");
        for a in texts {
            if let Attachment::Text {
                path,
                content,
                truncated,
            } = a
            {
                model.push_str(&format!("--- file: {path} ---\n{content}"));
                if !content.ends_with('\n') {
                    model.push('\n');
                }
                if *truncated {
                    model.push_str("(truncated to attach budget)\n");
                }
            }
        }
        model.push_str("</attached_files>");
    }
    (display, model)
}

fn fmt_bytes(n: usize) -> String {
    if n >= 1024 {
        format!("{:.1}KB", n as f64 / 1024.0)
    } else {
        format!("{n}B")
    }
}

/// Interactive `@` picker state, owned by the TUI loop. `anchor` is the
/// byte offset in the prompt input where the triggering `@` sits, so
/// selecting a file replaces `@<filter>` with `@<path> ` in place.
#[derive(Debug)]
pub struct MentionPicker {
    pub cwd: PathBuf,
    pub anchor: usize,
    pub input: String,
    pub files: Vec<String>,
    pub sel: usize,
}

impl MentionPicker {
    pub fn open(cwd: PathBuf, anchor: usize) -> Self {
        let mut p = Self {
            cwd,
            anchor,
            input: String::new(),
            files: Vec::new(),
            sel: 0,
        };
        p.requery();
        p
    }

    /// Re-run the file search for the current filter text.
    pub fn requery(&mut self) {
        self.files = list_files(&self.cwd, &self.input);
        self.sel = 0;
    }

    pub fn move_sel(&mut self, n: isize) {
        if self.files.is_empty() {
            self.sel = 0;
            return;
        }
        let len = self.files.len() as isize;
        self.sel = (self.sel as isize + n).clamp(0, len - 1) as usize;
    }

    pub fn selected(&self) -> Option<&str> {
        self.files.get(self.sel).map(String::as_str)
    }

    /// Splice the selected path into `input`, replacing `@<filter>`.
    /// Returns the new input string.
    pub fn apply_to(&self, input: &str) -> String {
        let path = match self.selected() {
            Some(p) => p,
            None => return input.to_string(),
        };
        // The mention runs from `anchor` to the end of the current
        // `@`-token (anchor + 1 + filter length), or to input end.
        let token_end = (self.anchor + 1 + self.input.len()).min(input.len());
        let mut out = String::with_capacity(input.len() + path.len());
        out.push_str(&input[..self.anchor.min(input.len())]);
        out.push('@');
        out.push_str(path);
        out.push(' ');
        out.push_str(&input[token_end.min(input.len())..]);
        out
    }
}
