//! Background turn-complete notifications: trigger logic, summary, and
//! `notify-send` command construction. Never spawns a real notification.
//! Run with `cargo test -p pantheon-eval`.

use pantheon_tui::notify::{find_notify_send, notify_command_with, should_notify, summarize_turn};
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use tempfile::tempdir;

#[test]
fn should_notify_only_for_background_completions() {
    assert!(!should_notify("run-a", "run-a")); // visible tab: no bell
    assert!(should_notify("run-a", "run-b")); // background tab: notify
    assert!(!should_notify("", "run-b")); // defensive: empty never fires
    assert!(!should_notify("", ""));
}

#[test]
fn summarize_turn_uses_first_line_capped() {
    let s = summarize_turn(Some("Fixed the bug\nsecond line here"), 3);
    assert_eq!(s, "turn 3 done — Fixed the bug");
}

#[test]
fn summarize_turn_truncates_long_first_lines() {
    let long = "x".repeat(200);
    let s = summarize_turn(Some(&long), 1);
    assert!(s.chars().count() <= "turn 1 done — ".chars().count() + 120);
}

#[test]
fn summarize_turn_falls_back_without_text() {
    assert_eq!(summarize_turn(None, 2), "turn 2 done");
    assert_eq!(summarize_turn(Some("   "), 2), "turn 2 done");
}

#[test]
fn find_notify_send_detects_executable() {
    let dir = tempdir().unwrap();
    let bin = dir.path().join("notify-send");
    fs::write(&bin, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();

    let found = find_notify_send(&[dir.path().to_path_buf()]);
    assert_eq!(found, Some(bin));
}

#[test]
fn find_notify_send_rejects_non_executable_and_missing() {
    let dir = tempdir().unwrap();
    let bin = dir.path().join("notify-send");
    fs::write(&bin, "x").unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(find_notify_send(&[dir.path().to_path_buf()]), None);

    let empty = tempdir().unwrap();
    assert_eq!(find_notify_send(&[empty.path().to_path_buf()]), None);
}

#[test]
fn notify_command_argv_is_exact() {
    let dir = tempdir().unwrap();
    let bin = dir.path().join("notify-send");
    let cmd = notify_command_with(&bin, "Pantheon — tab", "turn 1 done — hi");
    let args: Vec<_> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        vec![
            "--app-name",
            "Pantheon",
            "--expire-time",
            "8000",
            "Pantheon — tab",
            "turn 1 done — hi",
        ]
    );
    assert_eq!(cmd.get_program().to_string_lossy(), bin.to_string_lossy());
}
