//! Inline unified diffs: edit-target parsing, snapshot/diff roundtrips,
//! and the render model. Never touches the real TUI.
//! Run with `cargo test -p pantheon-eval`.

use pantheon_tui::diffview::{
    diff_snapshot, edit_targets, render_diff_lines, snapshot_file, unified_diff, DiffLine,
    MAX_DIFF_LINES,
};
use pantheon_tui::session::theme::Theme;
use std::fs;
use tempfile::tempdir;

#[test]
fn unified_diff_marks_changed_lines() {
    let lines = unified_diff("a\nb\nc\n", "a\nB\nc\n");
    assert!(lines.iter().any(|l| matches!(l, DiffLine::Hunk(_))));
    assert!(lines.contains(&DiffLine::Del("b".into())));
    assert!(lines.contains(&DiffLine::Add("B".into())));
    assert!(lines.contains(&DiffLine::Context("a".into())));
}

#[test]
fn unified_diff_new_file_is_all_adds() {
    let lines = unified_diff("", "x\ny\n");
    assert!(lines
        .iter()
        .all(|l| matches!(l, DiffLine::Add(_) | DiffLine::Hunk(_))));
    assert_eq!(
        lines
            .iter()
            .filter(|l| matches!(l, DiffLine::Add(_)))
            .count(),
        2
    );
}

#[test]
fn unified_diff_deleted_file_is_all_dels() {
    let lines = unified_diff("x\ny\n", "");
    assert!(lines
        .iter()
        .all(|l| matches!(l, DiffLine::Del(_) | DiffLine::Hunk(_))));
}

#[test]
fn unified_diff_identical_is_empty() {
    assert!(unified_diff("a\nb\n", "a\nb\n").is_empty());
}

#[test]
fn edit_targets_parses_apply_files_args() {
    let args =
        r#"{"edits": [{"path": "a.rs", "content": "x"}, {"path": "b/c.rs", "content": "y"}]}"#;
    let targets = edit_targets("apply_files", args);
    assert_eq!(targets.len(), 2);
    assert_eq!(targets[0].to_string_lossy(), "a.rs");
}

#[test]
fn edit_targets_rejects_other_tools_and_bad_json() {
    assert!(edit_targets("read_file", r#"{"path": "a"}"#).is_empty());
    assert!(edit_targets("apply_files", "not json").is_empty());
    assert!(edit_targets("apply_files", r#"{"edits": "nope"}"#).is_empty());
}

#[test]
fn snapshot_diff_roundtrip_shows_edits() {
    let dir = tempdir().unwrap();
    let f = dir.path().join("f.rs");
    fs::write(&f, "a\nb\nc\n").unwrap();

    let snap = snapshot_file(&f).unwrap();
    fs::write(&f, "a\nB\nc\nd\n").unwrap();

    let diff = diff_snapshot(&snap).unwrap();
    assert!(matches!(diff[0], DiffLine::FileHeader(_)));
    assert!(diff.contains(&DiffLine::Del("b".into())));
    assert!(diff.contains(&DiffLine::Add("B".into())));
    assert!(diff.contains(&DiffLine::Add("d".into())));
}

#[test]
fn snapshot_diff_none_when_unchanged() {
    let dir = tempdir().unwrap();
    let f = dir.path().join("f.rs");
    fs::write(&f, "same\n").unwrap();
    let snap = snapshot_file(&f).unwrap();
    assert!(diff_snapshot(&snap).is_none());
}

#[test]
fn snapshot_of_new_file_diffs_as_creation() {
    let dir = tempdir().unwrap();
    let f = dir.path().join("new.rs");
    // File did not exist at snapshot time.
    let snap = snapshot_file(&f).unwrap();
    assert!(snap.content.is_none());
    fs::write(&f, "created\n").unwrap();
    let diff = diff_snapshot(&snap).unwrap();
    assert!(diff.iter().all(|l| matches!(
        l,
        DiffLine::FileHeader(_) | DiffLine::Add(_) | DiffLine::Hunk(_)
    )));
}

#[test]
fn snapshot_skips_binary() {
    let dir = tempdir().unwrap();
    let f = dir.path().join("b.bin");
    fs::write(&f, b"\x00\x01\x02").unwrap();
    assert!(snapshot_file(&f).is_none());
}

#[test]
fn diff_snapshot_truncates_long_diffs() {
    let dir = tempdir().unwrap();
    let f = dir.path().join("big.rs");
    let before: String = (0..200).map(|i| format!("line {i}\n")).collect();
    fs::write(&f, &before).unwrap();
    let snap = snapshot_file(&f).unwrap();
    let after: String = (0..200).map(|i| format!("LINE {i}\n")).collect();
    fs::write(&f, &after).unwrap();

    let diff = diff_snapshot(&snap).unwrap();
    // Body capped; last line is the honest truncation marker.
    let body: Vec<_> = diff.iter().skip(1).collect();
    assert!(body.len() <= MAX_DIFF_LINES + 1);
    assert!(matches!(body.last().unwrap(), DiffLine::Truncated(n) if *n > 0));
}

#[test]
fn render_diff_lines_keeps_signs_and_order() {
    let th = Theme::pantheon();
    let diff = unified_diff("a\nb\n", "a\nB\n");
    let rendered = render_diff_lines(&diff, &th);
    let text: Vec<String> = rendered
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    assert!(text.iter().any(|l| l.contains("-b")));
    assert!(text.iter().any(|l| l.contains("+B")));
    assert!(text.iter().any(|l| l.contains("@@")));
    // FileHeader renders first when present.
    let with_header = {
        let mut d = vec![DiffLine::FileHeader("f.rs".into())];
        d.extend(diff);
        render_diff_lines(&d, &th)
    };
    let first: String = with_header[0]
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(first.contains("f.rs"), "{first}");
}
