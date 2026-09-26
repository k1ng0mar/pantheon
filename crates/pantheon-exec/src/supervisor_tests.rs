//! Tests for `pantheon_exec::supervisor::tests` — sibling file so sources stay test-free.
use super::*;

#[test]
fn protocol_round_trips() {
    let req = PluginRequest {
        call_id: "c1".into(),
        tool: "my_tool",
        args: serde_json::json!({"key": "value"}),
    };
    let line = serde_json::to_string(&req).unwrap();
    assert!(line.contains("\"call_id\":\"c1\""));

    let resp: PluginResponse = serde_json::from_str(r#"{"call_id":"c1","result":"ok"}"#).unwrap();
    assert_eq!(resp.call_id, "c1");
    assert_eq!(resp.result.unwrap(), serde_json::json!("ok"));
}

#[test]
fn error_response_parses() {
    let resp: PluginResponse =
        serde_json::from_str(r#"{"call_id":"c2","error":{"code":"TOOL_FAIL","cause":"boom"}}"#)
            .unwrap();
    assert_eq!(resp.call_id, "c2");
    let err = resp.error.unwrap();
    assert_eq!(err.code, "TOOL_FAIL");
    assert_eq!(err.cause, "boom");
}

/// Spawn a fake plugin (a shell loop that answers one canned response)
/// and drive a full call through the supervisor.
#[test]
fn live_call_round_trip() {
    let dir = std::env::temp_dir().join(format!(
        "pantheon-sup-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let runner = dir.join("run.sh");
    // Reads one request line, answers with result "pong:<tool>".
    std::fs::write(
            &runner,
            "#!/bin/sh\nread -r line\ntool=$(printf '%s' \"$line\" | sed 's/.*\"tool\":\"\\([^\"]*\\)\".*/\\1/')\ncid=$(printf '%s' \"$line\" | sed 's/.*\"call_id\":\"\\([^\"]*\\)\".*/\\1/')\nprintf '{\"call_id\":\"%s\",\"result\":\"pong:%s\"}\\n' \"$cid\" \"$tool\"\n",
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
        name: "pong".into(),
        description: String::new(),
        version: "0.1.0".into(),
        sha: None,
        maintainer: String::new(),
        capabilities: vec![ToolCapability {
            name: "ping".into(),
            capability: Capability::ShellExecute,
            description: String::new(),
            parameters: serde_json::json!({}),
        }],
        env_vars: vec![],
        runner: "run.sh".into(),
        enabled: true,
    };
    let mut sup =
        PluginSupervisor::spawn(&runner, &manifest, &dir, Duration::from_secs(5)).unwrap();
    let out = sup.call("ping", serde_json::json!({})).unwrap();
    assert!(out.contains("pong:ping"), "got: {out}");
    sup.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A plugin that never answers must time out and die, not hang the test.
#[test]
fn timeout_kills_wedged_plugin() {
    let dir = std::env::temp_dir().join(format!(
        "pantheon-sup-wedge-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let runner = dir.join("run.sh");
    // Reads one line, then sleeps forever.
    std::fs::write(&runner, "#!/bin/sh\nread -r line\nsleep 60\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(&runner).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(&runner, p).unwrap();
    }
    let manifest = PluginManifest {
        name: "wedge".into(),
        description: String::new(),
        version: "0.1.0".into(),
        sha: None,
        maintainer: String::new(),
        capabilities: vec![],
        env_vars: vec![],
        runner: "run.sh".into(),
        enabled: true,
    };
    let mut sup =
        PluginSupervisor::spawn(&runner, &manifest, &dir, Duration::from_millis(300)).unwrap();
    let t0 = Instant::now();
    let err = sup.call("anything", serde_json::json!({})).unwrap_err();
    assert_eq!(err.code, "PLUGIN_TIMEOUT");
    assert!(t0.elapsed() < Duration::from_secs(10));
    assert!(!sup.is_alive());
    // Second call fails fast.
    let err2 = sup.call("anything", serde_json::json!({})).unwrap_err();
    assert_eq!(err2.code, "PLUGIN_DEAD");
    sup.stop();
    let _ = std::fs::remove_dir_all(&dir);
}

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
