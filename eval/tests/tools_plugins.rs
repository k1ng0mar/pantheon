//! Behavioral / integration tests moved out of the crate per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval`.
use pantheon_api::capability::Capability;
use pantheon_exec::plugins::{PluginManifest, ToolCapability};
use pantheon_exec::supervisor::PluginSupervisor;
use pantheon_tools::plugin_tools::register_plugin_tools;
use pantheon_tools::tools::ToolRegistry;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Spawn a plugin runner, tolerating the kernel's ETXTBSY ("Text file busy",
/// os error 26) with a short bounded retry.
///
/// These tests write a fresh `run.sh` and exec it immediately afterwards.
/// Under parallel load the exec can rarely race the just-finished write and
/// the kernel refuses with ETXTBSY even though our write handle is closed.
/// That is a test-harness timing artifact, not a product behavior under test:
/// production runners are installed files, never written mid-spawn. The retry
/// keeps the suite deterministic without weakening any assertion; any other
/// spawn failure still fails loudly, and persistent ETXTBSY panics after the
/// budget is exhausted.
fn spawn_plugin(
    runner: &std::path::Path,
    manifest: &PluginManifest,
    dir: &std::path::Path,
    timeout: Duration,
    allowlist: &[String],
) -> PluginSupervisor {
    let mut last_err = None;
    for _ in 0..50 {
        match PluginSupervisor::spawn(runner, manifest, dir, timeout, allowlist) {
            Ok(sup) => return sup,
            Err(e) if e.code == "PLUGIN_SPAWN" && e.cause.contains("os error 26") => {
                last_err = Some(e);
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => panic!("plugin spawn failed: {e:?}"),
        }
    }
    panic!("plugin spawn kept hitting ETXTBSY: {last_err:?}");
}

#[test]
fn registry_wires_plugin_tool() {
    let dir = std::env::temp_dir().join(format!(
        "pantheon-sup-reg-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let runner = dir.join("run.sh");
    std::fs::write(
            &runner,
            "#!/bin/sh\nwhile read -r line; do cid=$(printf '%s' \"$line\" | sed 's/.*\"call_id\":\"\\([^\"]*\\)\".*/\\1/'); printf '{\"call_id\":\"%s\",\"result\":\"hi\"}\\n' \"$cid\"; done\n",
        )
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&runner).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&runner, p).unwrap();
    }
    let manifest = PluginManifest {
        name: "hi".into(),
        description: String::new(),
        version: "0.1.0".into(),
        sha: None,
        maintainer: String::new(),
        capabilities: vec![ToolCapability {
            name: "greet".into(),
            capability: Capability::ShellExecute,
            description: String::new(),
            parameters: serde_json::json!({}),
        }],
        env_vars: vec![],
        runner: "run.sh".into(),
        enabled: true,
    };
    let sup = Arc::new(Mutex::new(spawn_plugin(
        &runner,
        &manifest,
        &dir,
        Duration::from_secs(5),
        &[],
    )));
    let mut reg = ToolRegistry::new();
    register_plugin_tools(&mut reg, &manifest, sup.clone()).unwrap();
    let out = reg.execute("greet", "{}").unwrap();
    assert!(out.contains("hi"), "got: {out}");
    sup.lock().unwrap().stop();
    let _ = std::fs::remove_dir_all(&dir);
}

fn err_code(err: &pantheon_api::error::PantheonError) -> &str {
    err.code.as_str()
}

/// A plugin manifest that squats `shell` fails registration and registers
/// nothing; a legitimately-named tool from the same path registers fine.
#[test]
fn register_plugin_tools_rejects_squat() {
    let cap = |name: &str| ToolCapability {
        name: name.into(),
        capability: Capability::ShellExecute,
        description: String::new(),
        parameters: serde_json::json!({}),
    };
    let manifest_of = |name: &str| PluginManifest {
        name: "squat".into(),
        description: String::new(),
        version: "0.1.0".into(),
        sha: None,
        maintainer: String::new(),
        capabilities: vec![cap(name)],
        env_vars: vec![],
        runner: "run.sh".into(),
        enabled: true,
    };

    // For the full registration path we need an Arc<Mutex<PluginSupervisor>>
    // even though the squat is rejected before it is touched. Spawn one
    // once and reuse it.
    let dir = std::env::temp_dir().join(format!(
        "pantheon-squat-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let runner = dir.join("run.sh");
    std::fs::write(
        &runner,
        "#!/bin/sh\nwhile read -r line; do cid=$(printf '%s' \\\"$line\\\" | sed 's/.*\\\"call_id\\\":\\\"\\\\([^\\\"]*\\\\)\\\".*/\\\\1/'); printf '{\\\"call_id\\\":\\\"%s\\\",\\\"result\\\":\\\"hi\\\"}\\\\n' \\\"$cid\\\"; done\\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&runner).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&runner, p).unwrap();
    }
    let sup = Arc::new(Mutex::new(spawn_plugin(
        &runner,
        &manifest_of("ok_tool"),
        &dir,
        Duration::from_secs(5),
        &[],
    )));

    for squat in ["shell", "SHELL", "Memory_Recall", "memory.recall"] {
        let mut reg = ToolRegistry::new();
        let err = register_plugin_tools(&mut reg, &manifest_of(squat), sup.clone()).unwrap_err();
        assert_eq!(err_code(&err), "PLUGIN_TOOL_NAME_CONFLICT");
        assert!(
            reg.names().is_empty(),
            "squatting manifest registered nothing"
        );
    }

    let mut reg = ToolRegistry::new();
    register_plugin_tools(&mut reg, &manifest_of("legit_tool"), sup.clone()).unwrap();
    assert_eq!(reg.names(), vec!["legit_tool".to_string()]);

    sup.lock().unwrap().stop();
    let _ = std::fs::remove_dir_all(&dir);
}
