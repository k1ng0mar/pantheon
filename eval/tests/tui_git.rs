//! Git status-bar metadata: branch label and dirty flag.
//! Subprocess + filesystem behavior, so it lives in eval.
//! Run with `cargo test -p pantheon-eval`.

use pantheon_tui::session::git::git_label;
use pantheon_tui::session::statusbar::{render, StatusBarData};
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn git(dir: &Path, args: &[&str]) {
    let st = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(st.success(), "git {args:?} failed in {}", dir.display());
}

/// A committed repo on a known branch.
fn seed_repo() -> tempfile::TempDir {
    let dir = tempdir().unwrap();
    git(dir.path(), &["init", "-b", "testbranch"]);
    git(dir.path(), &["config", "user.email", "t@t"]);
    git(dir.path(), &["config", "user.name", "t"]);
    std::fs::write(dir.path().join("f.txt"), "one").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-m", "init"]);
    dir
}

fn bar_with_git(git: Option<String>) -> String {
    render(
        &StatusBarData {
            status_word: "ready".into(),
            icon: "✓".into(),
            model: "m".into(),
            context_frac: None,
            context_label: None,
            turn_in: None,
            turn_out: None,
            tokens_per_sec: None,
            cache_hit_rate: None,
            turn_no: None,
            session_prefix: "abc".into(),
            cost_usd: None,
            git,
            bg: None,
        },
        200,
    )
}

#[test]
fn non_git_dir_yields_no_label() {
    if !git_available() {
        return;
    }
    let dir = tempdir().unwrap();
    assert_eq!(git_label(dir.path()), None);
}

#[test]
fn clean_repo_shows_branch_without_star() {
    if !git_available() {
        return;
    }
    let dir = seed_repo();
    assert_eq!(git_label(dir.path()).as_deref(), Some("⎇ testbranch"));
}

#[test]
fn dirty_repo_shows_star() {
    if !git_available() {
        return;
    }
    let dir = seed_repo();
    std::fs::write(dir.path().join("f.txt"), "two").unwrap();
    assert_eq!(git_label(dir.path()).as_deref(), Some("⎇ testbranch*"));
    // Untracked files count as dirty too.
    std::fs::write(dir.path().join("f.txt"), "one").unwrap();
    std::fs::write(dir.path().join("new.txt"), "x").unwrap();
    assert_eq!(git_label(dir.path()).as_deref(), Some("⎇ testbranch*"));
}

#[test]
fn status_bar_renders_git_segment_only_when_present() {
    let with = bar_with_git(Some("⎇ main*".into()));
    assert!(with.contains("⎇ main*"), "bar shows label: {with}");
    let without = bar_with_git(None);
    assert!(!without.contains('⎇'), "bar omits segment: {without}");
}
