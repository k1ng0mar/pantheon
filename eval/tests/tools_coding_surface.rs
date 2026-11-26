//! The coding tool surface, end to end through the real registry.
//!
//! These are the tools a coding agent actually lives on: search, outline,
//! read a slice, make a targeted edit. The unit tests beside each closure
//! cover the shapes; this file covers the property that matters at the
//! boundary - every one of them is capability-gated and confined to the
//! workspace, and a read-only policy cannot edit.
//!
//! No provider, no network, no live model: the tools are pure functions
//! over a temp workspace plus a policy, which is exactly the seam that was
//! untested before.

use pantheon_api::capability::Policy;
use pantheon_api::workspace::WorkspaceRoot;
use pantheon_tools::builtins::{register_builtins_with, BuiltinOptions};
use pantheon_tools::tools::ToolRegistry;
use std::path::Path;
use std::sync::Arc;

fn registry(root: &Path) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    register_builtins_with(
        &mut reg,
        BuiltinOptions {
            workspace: Some(Arc::new(WorkspaceRoot::new(root))),
            ..Default::default()
        },
    );
    reg
}

fn seed(root: &Path) {
    std::fs::create_dir_all(root.join("src/util")).expect("mkdir");
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn helper() {}\n\npub fn run() {\n    helper();\n}\n",
    )
    .expect("write");
    std::fs::write(root.join("src/util/mod.rs"), "// helper lives here too\n").expect("write");
    std::fs::write(root.join("README.md"), "# project\nrun the tests\n").expect("write");
}

/// The coder policy allows reads and writes, so a search finds and an edit
/// lands.
#[test]
fn coder_policy_can_search_and_edit() {
    let dir = tempfile::tempdir().expect("tempdir");
    seed(dir.path());
    let reg = registry(dir.path());
    let policy = Policy::coder();

    let hits = reg
        .execute_gated(&policy, "grep", r#"{"pattern": "helper"}"#)
        .expect("grep is allowed under coder");
    assert!(hits.contains("src/lib.rs:1:pub fn helper()"), "got: {hits}");
    assert!(hits.contains("src/lib.rs:4:    helper();"), "got: {hits}");

    let files = reg
        .execute_gated(&policy, "glob", r#"{"pattern": "**/*.rs"}"#)
        .expect("glob is allowed under coder");
    assert_eq!(files.lines().count(), 2, "got: {files}");

    let edited = reg
        .execute_gated(
            &policy,
            "edit_file",
            r#"{"path": "src/lib.rs", "old_string": "pub fn helper() {}", "new_string": "pub fn helper() { /* touched */ }"}"#,
        )
        .expect("edit_file is allowed under coder");
    assert!(edited.contains("1 replacement"), "got: {edited}");
    assert!(std::fs::read_to_string(dir.path().join("src/lib.rs"))
        .expect("read")
        .contains("/* touched */"));
}

/// A read-only policy can search but cannot edit: the gate is the boundary,
/// not a convention the tool closures are expected to honor individually.
#[test]
fn reader_policy_cannot_edit() {
    let dir = tempfile::tempdir().expect("tempdir");
    seed(dir.path());
    let reg = registry(dir.path());
    let policy = Policy::researcher_readonly();

    reg.execute_gated(&policy, "grep", r#"{"pattern": "helper"}"#)
        .expect("reader may search");

    let err = reg
        .execute_gated(
            &policy,
            "edit_file",
            r#"{"path": "src/lib.rs", "old_string": "helper", "new_string": "x", "replace_all": true}"#,
        )
        .expect_err("reader must not edit");
    assert!(
        err.code.contains("CAP") || err.code.contains("DENIED") || err.code.contains("APPROVAL"),
        "unexpected code: {}",
        err.code
    );
    assert!(
        std::fs::read_to_string(dir.path().join("src/lib.rs"))
            .expect("read")
            .contains("pub fn helper()"),
        "the file must be untouched after a denied edit"
    );
}

/// Every search and edit path is confined: a sibling directory outside the
/// workspace is refused before any IO.
#[test]
fn coding_tools_are_confined() {
    let dir = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("tempdir");
    std::fs::write(outside.path().join("secret.txt"), "helper secret\n").expect("write");
    let reg = registry(dir.path());
    let policy = Policy::coder();
    let escape = outside.path().to_string_lossy().into_owned();

    for (tool, args) in [
        (
            "grep",
            format!(r#"{{"pattern": "helper", "path": "{escape}"}}"#),
        ),
        ("glob", format!(r#"{{"pattern": "*", "path": "{escape}"}}"#)),
        ("read_file", format!(r#"{{"path": "{escape}/secret.txt"}}"#)),
        (
            "edit_file",
            format!(
                r#"{{"path": "{escape}/secret.txt", "old_string": "helper", "new_string": "x"}}"#
            ),
        ),
    ] {
        let err = reg
            .execute_gated(&policy, tool, &args)
            .expect_err("confinement must refuse");
        assert!(
            err.code.starts_with("CONFINE"),
            "{tool} escaped confinement: {}",
            err.code
        );
    }
    assert_eq!(
        std::fs::read_to_string(outside.path().join("secret.txt")).expect("read"),
        "helper secret\n"
    );
}

/// A line-range read returns the requested slice with file-relative line
/// numbers, which is what makes a follow-up edit addressable.
#[test]
fn range_read_is_addressable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let body: String = (1..=20).map(|i| format!("line {i}\n")).collect();
    std::fs::write(dir.path().join("n.txt"), body).expect("write");
    let reg = registry(dir.path());

    let out = reg
        .execute_gated(
            &Policy::coder(),
            "read_file",
            r#"{"path": "n.txt", "start_line": 3, "end_line": 5}"#,
        )
        .expect("range read");
    assert_eq!(
        out.lines().take(3).collect::<Vec<_>>(),
        vec!["3:line 3", "4:line 4", "5:line 5"],
        "got: {out}"
    );
}

/// An edit that would be ambiguous fails loudly and leaves the file intact -
/// the property that keeps a model from silently rewriting the wrong site.
#[test]
fn ambiguous_edit_is_refused_without_side_effects() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("a.txt"), "x = 1\ny = 1\n").expect("write");
    let reg = registry(dir.path());

    let err = reg
        .execute_gated(
            &Policy::coder(),
            "edit_file",
            r#"{"path": "a.txt", "old_string": " = 1", "new_string": " = 2"}"#,
        )
        .expect_err("ambiguous edit must fail");
    assert_eq!(err.code, "EDIT_NOT_UNIQUE");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).expect("read"),
        "x = 1\ny = 1\n"
    );

    reg.execute_gated(
        &Policy::coder(),
        "edit_file",
        r#"{"path": "a.txt", "old_string": " = 1", "new_string": " = 2", "replace_all": true}"#,
    )
    .expect("replace_all resolves the ambiguity");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).expect("read"),
        "x = 2\ny = 2\n"
    );
}
