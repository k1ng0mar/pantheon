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
fn write_file_routes_through_safewrite_when_state_dir_given() {
    let work = fresh("work");
    let state = fresh("state");
    let p = work.join("f.txt");
    std::fs::write(&p, "v1\n").unwrap();
    let mut reg = ToolRegistry::new();
    register_builtins_with(
        &mut reg,
        BuiltinOptions {
            safewrite_state_dir: Some(state.clone()),
        },
    );
    let out = reg
        .execute(
            "write_file",
            &format!(r#"{{"path":"{}","content":"v2\n"}}"#, p.display()),
        )
        .unwrap();
    assert!(out.contains("checkpoint="), "got: {out}");
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "v2\n");
    // The checkpoint dir must now have a manifest for the file.
    let ckpts: Vec<_> = std::fs::read_dir(state.join("checkpoints"))
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert!(!ckpts.is_empty());
}

#[test]
fn write_file_stale_check_rejects_mismatch() {
    let work = fresh("work-stale");
    let state = fresh("state-stale");
    let p = work.join("f.txt");
    std::fs::write(&p, "v1\n").unwrap();
    let mut reg = ToolRegistry::new();
    register_builtins_with(
        &mut reg,
        BuiltinOptions {
            safewrite_state_dir: Some(state.clone()),
        },
    );
    // Pretend the file is still at "v0"; the safe path must reject.
    let err = reg
        .execute(
            "write_file",
            &format!(
                r#"{{"path":"{}","content":"v2\n","expected_hash":"deadbeef"}}"#,
                p.display()
            ),
        )
        .unwrap_err();
    // Safewrite errors are wrapped in TOOL_FS at the tool boundary.
    // The code on the wire is what callers test against.
    assert_eq!(err.code, "TOOL_FS");
    assert!(
        err.cause.contains("safewrite") || err.cause.contains("stale"),
        "cause should mention stale: {}",
        err.cause
    );
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "v1\n");
}

#[test]
fn write_file_unsafe_fallback_when_no_state_dir() {
    let work = fresh("work-unsafe");
    let p = work.join("f.txt");
    let mut reg = ToolRegistry::new();
    register_builtins(&mut reg);
    let out = reg
        .execute(
            "write_file",
            &format!(r#"{{"path":"{}","content":"hi\n"}}"#, p.display()),
        )
        .unwrap();
    assert!(!out.contains("checkpoint="), "got: {out}");
    assert_eq!(std::fs::read_to_string(&p).unwrap(), "hi\n");
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

#[test]
fn shell_result_states_whether_the_sandbox_actually_ran() {
    // `run_shell` used to discard `SandboxOutcome::sandboxed`, so on a host
    // where user namespaces are unavailable — most EC2 and container
    // instances, where `bwrap` fails with "setting up uid map: Permission
    // denied" — a command the tool documents as HIGH isolation ran
    // un-isolated with nothing in the tool result, the ledger, or `pantheon logs` to
    // record the downgrade. The invariant is that the result is never silent
    // about degraded isolation.
    let mut reg = ToolRegistry::default();
    crate::builtins::register_builtins(&mut reg);
    let args = serde_json::json!({ "command": "echo pantheon-sandbox-probe" }).to_string();

    let probe = pantheon_sandbox::runner::run_sandboxed(
        &SandboxProfile::from(SandboxLevel::High),
        "sh",
        &["-c", "true"],
        &std::env::temp_dir().to_string_lossy(),
    );
    let sandbox_works = probe.map(|r| r.sandboxed).unwrap_or(false);

    let out = reg.execute("shell", &args).expect(
        "shell must still run when isolation is unavailable; the capability \
                 gate already ran, so degraded isolation is a notice, not a failure",
    );
    assert!(
        out.contains("pantheon-sandbox-probe"),
        "the command must actually have run, got {out:?}"
    );
    assert_eq!(
        out.contains("ran WITHOUT namespace isolation"),
        !sandbox_works,
        "the degraded-isolation notice must appear exactly when the sandbox did not \
         initialize (sandbox_works={sandbox_works}), got {out:?}"
    );
}
