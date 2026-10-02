//! Behavioral / integration tests moved out of the crate per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval`.
//! Tests for the safe-write tool registrations - sibling file so sources stay test-free.
use pantheon_tools::safewrite_tools::{register_safewrite_with, SafewriteOptions};
use pantheon_tools::tools::ToolRegistry;

fn fresh(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pantheon-swtools-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn reg_in(work: &std::path::Path) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    register_safewrite_with(
        &mut reg,
        SafewriteOptions {
            state_dir: fresh("state"),
            workspace_root: Some(work.to_path_buf()),
        },
    );
    reg
}

#[test]
fn preview_file_rejects_escape() {
    let work = fresh("preview");
    let reg = reg_in(&work);
    std::fs::write(work.join("a.txt"), "hello\n").unwrap();
    let out = reg
        .execute(
            "preview_file",
            &format!(
                r#"{{"path":"{}","content":"bye\n"}}"#,
                work.join("a.txt").display()
            ),
        )
        .unwrap();
    assert!(
        out.contains("hello") || out.contains("before_hash"),
        "got: {out}"
    );
    let err = reg
        .execute(
            "preview_file",
            &format!(
                r#"{{"path":"{}","content":"x"}}"#,
                work.join("../evil.txt").display()
            ),
        )
        .unwrap_err();
    assert_eq!(err.code, "CONFINE_ESCAPE", "{err}");
}

#[test]
fn stage_and_apply_reject_deny_glob() {
    let work = fresh("stage");
    let reg = reg_in(&work);
    let err = reg
        .execute(
            "stage_files",
            r#"{"edits":[{"path":"/etc/pantheon-probe","content":"x"}]}"#,
        )
        .unwrap_err();
    assert_eq!(err.code, "CONFINE_DENIED", "{err}");
    let err = reg
        .execute(
            "apply_files",
            r#"{"edits":[{"path":"/etc/pantheon-probe","content":"x"}]}"#,
        )
        .unwrap_err();
    assert_eq!(err.code, "CONFINE_DENIED", "{err}");
    // checkpoint_files is confined too.
    let err = reg
        .execute(
            "checkpoint_files",
            &format!(r#"{{"paths":["{}"]}}"#, work.join("../evil.txt").display()),
        )
        .unwrap_err();
    assert_eq!(err.code, "CONFINE_ESCAPE", "{err}");
}

#[test]
fn stage_and_apply_in_workspace_still_work() {
    let work = fresh("stage-ok");
    let reg = reg_in(&work);
    let out = reg
        .execute(
            "stage_files",
            &format!(
                r#"{{"edits":[{{"path":"{}","content":"v1\n"}}]}}"#,
                work.join("f.txt").display()
            ),
        )
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let stage_id = v.get("id").and_then(|x| x.as_str()).unwrap().to_string();
    let out = reg
        .execute("apply_staged", &format!(r#"{{"stage_id":"{stage_id}"}}"#))
        .unwrap();
    assert!(out.contains("checkpoint_id"), "got: {out}");
    assert_eq!(std::fs::read_to_string(work.join("f.txt")).unwrap(), "v1\n");
}
