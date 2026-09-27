//! Path confinement for file tools.
//!
//! Every model-supplied filesystem path goes through [`confine`] before any
//! read or write. Two layers, in this order:
//!
//! 1. **Deny globs** — secret-bearing locations (`~/.ssh/**`, `/etc/**`,
//!    …) are rejected outright, evaluated *before* any capability allow: a
//!    granted `FilesystemRead` never implies the right to read
//!    `~/.ssh/id_rsa`. The globs are matched against the raw input, the
//!    `~`-expanded input, the absolutized path, and the canonicalized path,
//!    so `../.ssh/id_rsa` and symlinks into denied dirs are caught too.
//! 2. **Workspace containment** — the path is canonicalized (resolving `..`
//!    and symlinks via the longest existing prefix) and must stay under the
//!    canonical workspace root.
//!
//! The workspace root is caller-provided. The tool layer has no workspace
//! concept of its own, so it captures the process working directory at tool
//! *registration* time (overridable via options) and passes it in.
//!
//! Residual TOCTOU: canonicalize-then-use still races a hostile local
//! writer swapping path components between the check and the IO. The write
//! path additionally opens tmp files with `O_NOFOLLOW|O_CREAT|O_EXCL` and
//! publishes with rename-over (which replaces a destination symlink rather
//! than following it), so a symlink swap at the final component cannot
//! redirect a write. Full elimination needs `openat2(RESOLVE_IN_ROOT)`-style
//! dirfd walks; that is future work, not today's gap.

use pantheon_api::error::{Layer, PantheonError};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

fn cerr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Execution,
        false,
        cause,
        "use a path inside the workspace root",
        "",
    )
}

/// Deny patterns. `~` expands to `$HOME` at match time; a trailing `/**`
/// matches the directory itself and everything under it.
fn base_deny_patterns() -> Vec<String> {
    vec![
        "~/.ssh/**".into(),
        "~/.gnupg/**".into(),
        "~/.aws/**".into(),
        "~/.bashrc".into(),
        "~/.bash_profile".into(),
        "~/.profile".into(),
        "~/.gitconfig".into(),
        "/etc/**".into(),
    ]
}

struct DenySet {
    /// Patterns as written (may contain a literal `~`).
    raw: Vec<String>,
    /// Patterns with a leading `~` expanded to `$HOME` (when set).
    expanded: Vec<String>,
}

fn deny_set() -> DenySet {
    let home = std::env::var("HOME")
        .ok()
        .map(|h| h.trim_end_matches('/').to_string())
        .filter(|h| !h.is_empty());
    let mut raw = base_deny_patterns();
    let mut expanded: Vec<String> = raw
        .iter()
        .map(|p| match (&home, p.strip_prefix('~')) {
            (Some(h), Some(rest)) => format!("{h}{rest}"),
            _ => p.clone(),
        })
        .collect();
    // The secrets broker persists under the data dir; nothing the model
    // touches through file tools should reach in there directly.
    if let Ok(d) = std::env::var("PANTHEON_DATA_DIR") {
        let d = d.trim();
        if !d.is_empty() {
            let pat = format!("{}/secrets/**", d.trim_end_matches('/'));
            raw.push(pat.clone());
            expanded.push(pat);
        }
    }
    DenySet { raw, expanded }
}

fn pattern_matches(candidate: &str, pattern: &str) -> bool {
    let cand = candidate.trim_end_matches('/');
    if let Some(prefix) = pattern.strip_suffix("/**") {
        let prefix = prefix.trim_end_matches('/');
        cand == prefix || cand.starts_with(&format!("{prefix}/"))
    } else {
        cand == pattern.trim_end_matches('/')
    }
}

impl DenySet {
    /// First matching pattern, checking both literal and `~`-expanded forms.
    fn matching(&self, candidate: &str) -> Option<&str> {
        self.raw
            .iter()
            .chain(self.expanded.iter())
            .find(|p| pattern_matches(candidate, p))
            .map(|s| s.as_str())
    }
}

/// Expand a leading `~` / `~/` to `$HOME`. `None` when there is no `~`
/// prefix or `HOME` is unset.
fn expand_home_str(s: &str) -> Option<String> {
    if s == "~" || s.starts_with("~/") {
        let home = std::env::var("HOME")
            .ok()
            .map(|h| h.trim_end_matches('/').to_string())
            .filter(|h| !h.is_empty())?;
        return Some(if s == "~" {
            home
        } else {
            format!("{home}{}", &s[1..])
        });
    }
    None
}

/// Make the path absolute: `~` expands to home, relative paths join onto the
/// workspace root. `.`/`..` are collapsed lexically here, before symlink
/// resolution.
///
/// Collapsing `..` before resolving symlinks differs from kernel resolution
/// when a `..` follows a symlink (`<root>/link/../x` with `link -> /etc`
/// collapses to `<root>/x`, the kernel would land in `/`). That is safe
/// here because of the check/use invariant: [`confine`] returns the
/// canonicalized path and callers must use the *returned* path for IO, never
/// the original — so the containment verdict and the actual IO always agree,
/// and a `..`-after-symlink can only remap to another path *inside* the
/// verdict. What lexical collapsing buys us: paths with trailing `..`
/// resolve instead of erroring, and `..` can never smuggle a symlink past
/// the canonicalizer.
fn absolutize(path: &Path, workspace_root: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    let abs = if let Some(expanded) = expand_home_str(&s) {
        PathBuf::from(expanded)
    } else if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace_root.join(path)
    };
    normalize_lexical(&abs)
}

/// Collapse `.` / `..` lexically. Only safe on fully-resolved paths (no
/// symlinks left), which is all this is used for.
fn normalize_lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(Component::RootDir.as_os_str());
    }
    out
}

/// Canonicalize, resolving symlinks. For paths that do not (fully) exist,
/// canonicalize the longest existing prefix and re-append the remainder —
/// intermediate popped components provably do not exist, so no symlink in
/// the remainder can hide an escape; the final result is re-checked against
/// the deny list and the workspace root.
fn canonicalize_best_effort(abs: &Path) -> Result<PathBuf, PantheonError> {
    let mut cur = abs.to_path_buf();
    let mut tail: Vec<OsString> = Vec::new();
    loop {
        match std::fs::canonicalize(&cur) {
            Ok(resolved) => {
                let mut out = resolved;
                for comp in tail.iter().rev() {
                    out.push(comp);
                }
                return Ok(normalize_lexical(&out));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => match cur.file_name() {
                Some(name) => {
                    tail.push(name.to_os_string());
                    match cur.parent() {
                        Some(p) if p != cur => cur = p.to_path_buf(),
                        _ => {
                            return Err(cerr(
                                "CONFINE_BAD_PATH",
                                format!("cannot resolve {}", abs.display()),
                            ))
                        }
                    }
                }
                None => {
                    return Err(cerr(
                        "CONFINE_BAD_PATH",
                        format!("cannot resolve {}", abs.display()),
                    ))
                }
            },
            Err(e) => {
                return Err(cerr(
                    "CONFINE_IO",
                    format!("canonicalize {}: {e}", abs.display()),
                ))
            }
        }
    }
}

/// Confine `path` to `workspace_root`.
///
/// Returns the canonicalized absolute path on success — callers should use
/// the returned path for IO, not the original, so the check and the use
/// agree. Errors:
/// - `CONFINE_DENIED` — matched a deny glob (checked before containment);
/// - `CONFINE_ESCAPE` — canonical path leaves the workspace root;
/// - `CONFINE_BAD_PATH` / `CONFINE_IO` — unresolvable input or root.
pub fn confine(path: &Path, workspace_root: &Path) -> Result<PathBuf, PantheonError> {
    let raw = path.to_string_lossy();
    if raw.trim().is_empty() {
        return Err(cerr("CONFINE_BAD_PATH", "empty path".into()));
    }
    let denies = deny_set();
    let denied = |candidate: &str, pat: &str| {
        cerr(
            "CONFINE_DENIED",
            format!("{candidate} matches deny pattern {pat}"),
        )
    };

    // Stage 1: raw input and its `~` expansion, before touching the fs.
    let mut early = vec![raw.clone().into_owned()];
    if let Some(e) = expand_home_str(&raw) {
        early.push(e);
    }
    for c in &early {
        if let Some(pat) = denies.matching(c) {
            return Err(denied(c, pat));
        }
    }

    // Stage 2: absolutized (relative paths land under the workspace root).
    let abs = absolutize(path, workspace_root);
    let abs_s = abs.to_string_lossy().into_owned();
    if let Some(pat) = denies.matching(&abs_s) {
        return Err(denied(&abs_s, pat));
    }

    // Stage 3: canonicalized — resolves `..` and symlinks, catching
    // `work/../.ssh/x` and `work/link -> ~/.ssh` style evasions.
    let canon = canonicalize_best_effort(&abs)?;
    let canon_s = canon.to_string_lossy().into_owned();
    if let Some(pat) = denies.matching(&canon_s) {
        return Err(denied(&canon_s, pat));
    }

    // Stage 4: containment under the canonical workspace root.
    let root_canon = std::fs::canonicalize(workspace_root).map_err(|e| {
        cerr(
            "CONFINE_BAD_PATH",
            format!("workspace root {}: {e}", workspace_root.display()),
        )
    })?;
    if canon != root_canon && !canon.starts_with(&root_canon) {
        return Err(cerr(
            "CONFINE_ESCAPE",
            format!(
                "{} escapes workspace {}",
                canon.display(),
                root_canon.display()
            ),
        ));
    }
    Ok(canon)
}

