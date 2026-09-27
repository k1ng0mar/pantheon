//! Tests for `pantheon_tools::plugin_tools` — the registration half that
//! split out of `pantheon-exec::supervisor_tests` (capability ≠ tool).
use crate::plugin_tools::register_plugin_tools;
use crate::tools::ToolRegistry;
use pantheon_api::capability::Capability;
use pantheon_exec::plugins::{PluginManifest, ToolCapability};
use pantheon_exec::supervisor::PluginSupervisor;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Registry wiring: plugin tools execute through the shared supervisor
/// and respect direct-execute gating.
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
    let sup = Arc::new(Mutex::new(
        PluginSupervisor::spawn(&runner, &manifest, &dir, Duration::from_secs(5)).unwrap(),
    ));
    let mut reg = ToolRegistry::new();
    register_plugin_tools(&mut reg, &manifest, sup.clone());
    let out = reg.execute("greet", "{}").unwrap();
    assert!(out.contains("hi"), "got: {out}");
    sup.lock().unwrap().stop();
    let _ = std::fs::remove_dir_all(&dir);
}
