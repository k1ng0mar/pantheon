//! `@` file mentions: parser, picker, and attachment resolution.
//! Run with `cargo test -p pantheon-eval`.

use pantheon_tui::mentions::{
    extract_mentions, is_binary, list_files, resolve_mentions, MentionPicker, MAX_ATTACH_BYTES,
    MAX_RESULTS,
};
use std::fs;
use tempfile::tempdir;

#[test]
fn extract_mentions_finds_paths_in_order() {
    let m = extract_mentions("look at @src/main.rs and @Cargo.toml please");
    assert_eq!(m, vec!["src/main.rs", "Cargo.toml"]);
}

#[test]
fn extract_mentions_ignores_lone_at() {
    assert!(extract_mentions("email me @").is_empty());
    assert!(extract_mentions("a @ b").is_empty());
    assert!(extract_mentions("no mentions here").is_empty());
}

#[test]
fn is_binary_detects_nul() {
    assert!(is_binary(b"GIF89a\x00\x01\x02"));
    assert!(!is_binary(b"fn main() {}\n"));
    assert!(!is_binary(b""));
}

#[test]
fn list_files_respects_gitignore_and_skips_noise() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join(".gitignore"), "*.log\nignored_dir/\n").unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("target")).unwrap();
    fs::create_dir_all(root.join("ignored_dir")).unwrap();
    fs::write(root.join("src/main.rs"), "fn main(){}").unwrap();
    fs::write(root.join("debug.log"), "x").unwrap();
    fs::write(root.join("target/out"), "x").unwrap();
    fs::write(root.join("ignored_dir/f"), "x").unwrap();

    let all = list_files(root, "");
    assert!(all.contains(&"src/main.rs".to_string()), "{all:?}");
    assert!(!all.iter().any(|p| p.ends_with(".log")), "{all:?}");
    assert!(!all.iter().any(|p| p.starts_with("target/")), "{all:?}");
    assert!(
        !all.iter().any(|p| p.starts_with("ignored_dir/")),
        "{all:?}"
    );
}

#[test]
fn list_files_fuzzy_filters_and_caps() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    for i in 0..5 {
        fs::write(root.join(format!("alpha_{i}.rs")), "x").unwrap();
    }
    fs::write(root.join("beta.rs"), "x").unwrap();

    let hits = list_files(root, "alp");
    assert_eq!(hits.len(), 5);
    assert!(hits.iter().all(|p| p.contains("alpha")));

    let hits = list_files(root, "zzz_no_match");
    assert!(hits.is_empty());

    assert!(list_files(root, "").len() <= MAX_RESULTS);
}

#[test]
fn resolve_mentions_attaches_text_and_reports() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("notes.txt"), "hello world").unwrap();
    fs::write(root.join("bin.dat"), b"\x00\x01\x02binary").unwrap();

    let (display, model) = resolve_mentions("see @notes.txt and @bin.dat and @missing.txt", root);
    // Transcript copy annotates each mention.
    assert!(display.contains("@notes.txt [attached"), "{display}");
    assert!(
        display.contains("@bin.dat [skipped: binary file]"),
        "{display}"
    );
    assert!(
        display.contains("@missing.txt [skipped: no such file]"),
        "{display}"
    );
    // Model copy carries the file section.
    assert!(model.contains("<attached_files>"), "{model}");
    assert!(model.contains("--- file: notes.txt ---"), "{model}");
    assert!(model.contains("hello world"), "{model}");
    // The binary is named in the prompt but never attached.
    let section = model.split("<attached_files>").nth(1).unwrap();
    assert!(!section.contains("bin.dat"), "{model}");
}

#[test]
fn resolve_mentions_rejects_escapes() {
    let dir = tempdir().unwrap();
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    // The file exists, but outside the working directory.
    fs::write(dir.path().join("secret.txt"), "x").unwrap();
    let (display, _) = resolve_mentions("see @../secret.txt", &sub);
    assert!(
        display.contains("skipped: outside working directory"),
        "{display}"
    );
}

#[test]
fn resolve_mentions_missing_file_says_so() {
    let dir = tempdir().unwrap();
    let (display, _) = resolve_mentions("see @nope.txt", dir.path());
    assert!(display.contains("skipped: no such file"), "{display}");
}

#[test]
fn resolve_mentions_enforces_total_cap() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    // Two files each just over half the budget: the second must truncate.
    let big = "x".repeat(MAX_ATTACH_BYTES / 2 + 100);
    fs::write(root.join("a.txt"), &big).unwrap();
    fs::write(root.join("b.txt"), &big).unwrap();

    let (display, model) = resolve_mentions("@a.txt @b.txt", root);
    assert!(display.contains("@b.txt [attached"), "{display}");
    assert!(display.contains("(truncated)"), "{display}");
    let section = model.split("<attached_files>").nth(1).unwrap();
    assert!(section.len() <= MAX_ATTACH_BYTES + 1024, "budget blown");
}

#[test]
fn resolve_mentions_truncation_never_splits_utf8() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    // Leave 2 bytes of budget so the second file truncates inside "é".
    let filler = "x".repeat(MAX_ATTACH_BYTES - 2);
    fs::write(root.join("fill.txt"), &filler).unwrap();
    // "é" is 2 bytes; a 2-byte take lands mid-char without the fix.
    fs::write(root.join("uni.txt"), "aébc").unwrap();

    let (_, model) = resolve_mentions("@fill.txt @uni.txt", root);
    let section = model.split("<attached_files>").nth(1).unwrap();
    assert!(
        !section.contains('\u{FFFD}'),
        "truncation produced a replacement char"
    );
}

#[test]
fn resolve_mentions_without_mentions_is_identity() {
    let dir = tempdir().unwrap();
    let (display, model) = resolve_mentions("plain text", dir.path());
    assert_eq!(display, "plain text");
    assert_eq!(model, "plain text");
}

#[test]
fn mention_picker_applies_selection_in_place() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    fs::write(root.join("main.rs"), "x").unwrap();

    // Simulate typing "@mai": input holds "@mai", anchor at 0.
    let mut picker = MentionPicker::open(root, 0);
    picker.input = "mai".to_string();
    picker.requery();
    assert_eq!(picker.selected(), Some("main.rs"));

    let out = picker.apply_to("@mai and more");
    assert_eq!(out, "@main.rs  and more", "{out}");
}

#[test]
fn mention_picker_selection_moves() {
    let dir = tempdir().unwrap();
    let root = dir.path().to_path_buf();
    fs::write(root.join("a.rs"), "x").unwrap();
    fs::write(root.join("b.rs"), "x").unwrap();

    let mut picker = MentionPicker::open(root, 0);
    assert_eq!(picker.files.len(), 2);
    picker.move_sel(1);
    assert_eq!(picker.sel, 1);
    picker.move_sel(5); // clamps
    assert_eq!(picker.sel, 1);
    picker.move_sel(-5); // clamps
    assert_eq!(picker.sel, 0);
}
