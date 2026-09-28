//! Behavioral / integration tests moved out of the crate per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval`.
use pantheon_sandbox::{SandboxLevel, SandboxProfile};
use pantheon_tools::builtins::{register_builtins_with, BuiltinOptions};
use pantheon_tools::tools::ToolRegistry;

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
            workspace_root: Some(work.clone()),
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
            workspace_root: Some(work.clone()),
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
    register_builtins_with(
        &mut reg,
        BuiltinOptions {
            safewrite_state_dir: None,
            workspace_root: Some(work.clone()),
        },
    );
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
fn shell_result_states_whether_the_sandbox_actually_ran() {
    // Fail-closed contract: on a host where user namespaces are unavailable
    // (most EC2/container instances — `bwrap` fails with "setting up uid
    // map: Permission denied"), `run_sandboxed` returns SANDBOX_UNAVAILABLE
    // instead of silently running un-isolated. The shell tool surfaces that
    // error; only an explicit opt-in (`PANTHEON_SANDBOX_FALLBACK=allow`)
    // restores the old degraded run, and then the result says so loudly.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = ENV_LOCK.lock().unwrap();

    let mut reg = ToolRegistry::default();
    pantheon_tools::builtins::register_builtins(&mut reg);
    let args = serde_json::json!({ "command": "echo pantheon-sandbox-probe" }).to_string();

    let probe = pantheon_sandbox::runner::run_sandboxed(
        &SandboxProfile::from(SandboxLevel::High),
        "sh",
        &["-c", "true"],
        &std::env::temp_dir().to_string_lossy(),
    );
    let sandbox_works = probe.map(|r| r.sandboxed).unwrap_or(false);

    // Make sure the opt-in is off for the fail-closed assertion.
    let saved = std::env::var("PANTHEON_SANDBOX_FALLBACK").ok();
    std::env::remove_var("PANTHEON_SANDBOX_FALLBACK");

    if sandbox_works {
        let out = reg.execute("shell", &args).expect("shell must run");
        assert!(
            out.contains("pantheon-sandbox-probe"),
            "the command must actually have run, got {out:?}"
        );
        assert!(
            !out.contains("WITHOUT namespace isolation"),
            "no degraded-isolation notice expected when the sandbox ran, got {out:?}"
        );
    } else {
        let err = reg
            .execute("shell", &args)
            .expect_err("shell must fail closed when the sandbox is unavailable");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("SANDBOX_UNAVAILABLE"),
            "expected SANDBOX_UNAVAILABLE, got {msg}"
        );

        // Explicit opt-in restores the degraded run, loudly.
        std::env::set_var("PANTHEON_SANDBOX_FALLBACK", "allow");
        let out = reg
            .execute("shell", &args)
            .expect("shell must run degraded once the direct fallback is explicitly opted in");
        assert!(
            out.contains("pantheon-sandbox-probe"),
            "the command must actually have run, got {out:?}"
        );
        assert!(
            out.contains("WITHOUT namespace isolation"),
            "the degraded-isolation notice must appear on the opted-in fallback, got {out:?}"
        );
    }

    match saved {
        Some(v) => std::env::set_var("PANTHEON_SANDBOX_FALLBACK", v),
        None => std::env::remove_var("PANTHEON_SANDBOX_FALLBACK"),
    }
}

fn reg_in(work: &std::path::Path) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    register_builtins_with(
        &mut reg,
        BuiltinOptions {
            safewrite_state_dir: None,
            workspace_root: Some(work.to_path_buf()),
        },
    );
    reg
}

#[test]
fn read_file_rejects_escape() {
    let work = fresh("confine-read");
    let reg = reg_in(&work);
    std::fs::write(work.join("ok.txt"), "fine\n").unwrap();
    // In-workspace read (absolute and relative) works.
    let out = reg
        .execute(
            "read_file",
            &format!(r#"{{"path":"{}"}}"#, work.join("ok.txt").display()),
        )
        .unwrap();
    assert!(out.contains("fine"), "got: {out}");
    let out = reg.execute("read_file", r#"{"path":"ok.txt"}"#).unwrap();
    assert!(out.contains("fine"), "got: {out}");
    // `..` escape is rejected.
    let err = reg
        .execute(
            "read_file",
            &format!(r#"{{"path":"{}"}}"#, work.join("../outside.txt").display()),
        )
        .unwrap_err();
    assert_eq!(err.code, "CONFINE_ESCAPE", "{err}");
    // list_dir is confined too.
    let err = reg
        .execute(
            "list_dir",
            &format!(r#"{{"path":"{}"}}"#, work.join("..").display()),
        )
        .unwrap_err();
    assert_eq!(err.code, "CONFINE_ESCAPE", "{err}");
    let out = reg
        .execute("list_dir", &format!(r#"{{"path":"{}"}}"#, work.display()))
        .unwrap();
    assert!(out.contains("ok.txt"), "got: {out}");
}

#[test]
fn write_file_rejects_deny_globs_and_escapes() {
    let work = fresh("confine-write");
    let state = fresh("confine-write-state");
    let mut reg = ToolRegistry::new();
    register_builtins_with(
        &mut reg,
        BuiltinOptions {
            safewrite_state_dir: Some(state),
            workspace_root: Some(work.clone()),
        },
    );
    // /etc/** is denied by glob even though the capability is granted.
    let err = reg
        .execute(
            "write_file",
            r#"{"path":"/etc/pantheon-confine-probe","content":"x"}"#,
        )
        .unwrap_err();
    assert_eq!(err.code, "CONFINE_DENIED", "{err}");
    // `..` escape is rejected.
    let err = reg
        .execute(
            "write_file",
            &format!(
                r#"{{"path":"{}","content":"x"}}"#,
                work.join("../evil.txt").display()
            ),
        )
        .unwrap_err();
    assert_eq!(err.code, "CONFINE_ESCAPE", "{err}");
    assert!(!work.join("../evil.txt").exists());
    // In-workspace write still works.
    let args = serde_json::json!({"path": work.join("w.txt"), "content": "v\n"}).to_string();
    let out = reg.execute("write_file", &args).unwrap();
    assert!(out.contains("checkpoint="), "got: {out}");
    assert_eq!(std::fs::read_to_string(work.join("w.txt")).unwrap(), "v\n");
}

#[test]
#[cfg(unix)]
fn read_file_rejects_symlink_escape() {
    let base = fresh("confine-link");
    let work = base.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let secret = base.join("secret.txt");
    std::fs::write(&secret, "topsecret\n").unwrap();
    std::os::unix::fs::symlink(&secret, work.join("link")).unwrap();
    let reg = reg_in(&work);
    let err = reg
        .execute(
            "read_file",
            &format!(r#"{{"path":"{}"}}"#, work.join("link").display()),
        )
        .unwrap_err();
    assert_eq!(err.code, "CONFINE_ESCAPE", "{err}");
}
