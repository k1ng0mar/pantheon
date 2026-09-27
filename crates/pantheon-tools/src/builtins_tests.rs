//! Tests for `pantheon_exec::builtins::tests` — sibling file so sources stay test-free.
use super::*;
use crate::tools::ToolRegistry;

fn fresh(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pantheon-builtins-{}-{}-{}",
        name,
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

#[test]
fn shell_push_arg_escalates_to_git_push_capability() {
    let mut reg = ToolRegistry::default();
    crate::builtins::register_builtins(&mut reg);
    for cmd in [
        "git push",
        "git -C /tmp/repo push origin main",
        "sudo git push --force",
    ] {
        let args = serde_json::json!({ "command": cmd }).to_string();
        let caps = reg.required_capabilities("shell", &args);
        assert!(
            caps.contains(&Capability::GitPush),
            "`{cmd}` must escalate to GitPush, got {caps:?}"
        );
        assert_eq!(
            pantheon_api::capability::Policy::coder().check(&Capability::GitPush),
            pantheon_api::capability::Decision::Approval,
            "`{cmd}` must park, not run"
        );
    }
    // A non-push shell call must not drag GitPush in.
    let args = serde_json::json!({ "command": "ls -la" }).to_string();
    let caps = reg.required_capabilities("shell", &args);
    assert!(!caps.contains(&Capability::GitPush), "got {caps:?}");
    assert!(caps.contains(&Capability::ShellExecute));
}
