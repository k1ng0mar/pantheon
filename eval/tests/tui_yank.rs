//! Yank: fenced code-block extraction and clipboard backend probing.
//! Never touches a real clipboard.
//! Run with `cargo test -p pantheon-eval`.

use pantheon_tui::yank::{clipboard_command_for, extract_code_blocks, find_clipboard};
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use tempfile::tempdir;

#[test]
fn extract_code_blocks_finds_fences_in_order() {
    let text = "intro\n```rust\nlet x = 1;\n```\nmiddle\n```\nplain\n```\nend";
    let blocks = extract_code_blocks(text);
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].language.as_deref(), Some("rust"));
    assert_eq!(blocks[0].code, "let x = 1;\n");
    assert_eq!(blocks[1].language, None);
    assert_eq!(blocks[1].code, "plain\n");
}

#[test]
fn extract_code_blocks_handles_unclosed_fence() {
    let blocks = extract_code_blocks("```py\nprint(1)\n");
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].code, "print(1)\n");
}

#[test]
fn extract_code_blocks_ignores_inline_backticks() {
    assert!(extract_code_blocks("use `foo` here").is_empty());
    assert!(extract_code_blocks("no fences").is_empty());
}

#[test]
fn find_clipboard_prefers_wl_copy_first() {
    let dir = tempdir().unwrap();
    for bin in ["wl-copy", "xclip", "xsel", "pbcopy"] {
        let p = dir.path().join(bin);
        fs::write(&p, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let (name, _) = find_clipboard(&[dir.path().to_path_buf()]).unwrap();
    assert_eq!(name, "wl-copy");
}

#[test]
fn find_clipboard_falls_through_missing_backends() {
    let dir = tempdir().unwrap();
    // Only xsel present: order skips the missing ones.
    let p = dir.path().join("xsel");
    fs::write(&p, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
    let (name, _) = find_clipboard(&[dir.path().to_path_buf()]).unwrap();
    assert_eq!(name, "xsel");

    let empty = tempdir().unwrap();
    assert_eq!(find_clipboard(&[empty.path().to_path_buf()]), None);
}

#[test]
fn clipboard_command_argv_per_backend() {
    let dir = tempdir().unwrap();
    // xclip/xsel need selection flags; wl-copy/pbcopy take none.
    let args_for = |bin: &str| {
        let p = dir.path().join(bin);
        let cmd = clipboard_command_for(bin, &p);
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(args_for("xclip"), vec!["-selection", "clipboard"]);
    assert_eq!(args_for("xsel"), vec!["--clipboard", "--input"]);
    assert!(args_for("wl-copy").is_empty());
    assert!(args_for("pbcopy").is_empty());
}
