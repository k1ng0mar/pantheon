//! Cached git metadata for the status bar.
//!
//! [`git_label`] shells out to git only when the caller asks — the TUI
//! refreshes it on a coarse interval, never per frame. Anything
//! unexpected (no git binary, not a checkout, unborn HEAD) yields `None`
//! and the status bar simply omits the segment.

use std::path::Path;
use std::process::Command;

/// `⎇ <branch>[*]` for `cwd`, or `None` outside a git checkout.
/// `*` marks a dirty working tree. A detached HEAD falls back to the
/// short SHA so the segment still says where the tree is.
pub fn git_label(cwd: &Path) -> Option<String> {
    let branch = git_out(cwd, &["branch", "--show-current"])?;
    let name = if branch.is_empty() {
        let sha = git_out(cwd, &["rev-parse", "--short", "HEAD"])?;
        if sha.is_empty() {
            return None;
        }
        sha
    } else {
        branch
    };
    let dirty = git_out(cwd, &["status", "--porcelain"])
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    Some(format!("⎇ {}{}", name, if dirty { "*" } else { "" }))
}

/// Run git and return trimmed stdout; `None` on any failure: missing
/// binary, bad cwd, non-zero exit.
fn git_out(cwd: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
