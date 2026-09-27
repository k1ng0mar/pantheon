//! Tests for `pantheon_exec::supervisor::tests` — sibling file so sources stay test-free.
use super::*;
use crate::plugins::{EnvVarDecl, ToolCapability};

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
        PluginSupervisor::spawn(&runner, &manifest, &dir, Duration::from_secs(5), &[]).unwrap();
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
        PluginSupervisor::spawn(&runner, &manifest, &dir, Duration::from_millis(300), &[]).unwrap();
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

/// A manifest-declared env var reaches the plugin ONLY on an allowlist hit.
/// The host holds two vars, the manifest declares both, the operator
/// allowlists one: the plugin must see exactly the allowlisted one. A
/// project-controlled manifest naming `*_SECRET_*` must not smuggle host
/// secrets past the boundary.
#[test]
fn manifest_env_needs_allowlist_hit() {
    std::env::set_var("PANTHEON_SUP_TEST_ALLOWED", "visible");
    std::env::set_var("PANTHEON_SUP_TEST_DENIED", "must-not-leak");

    let dir = std::env::temp_dir().join(format!(
        "pantheon-sup-env-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let runner = dir.join("run.sh");
    // Echoes what the child environment actually contains, inside the
    // protocol reply.
    std::fs::write(
        &runner,
        concat!(
            "#!/bin/sh\n",
            "read -r line\n",
            "cid=$(printf '%s' \"$line\" | sed 's/.*\"call_id\":\"\\([^\"]*\\)\".*/\\1/')\n",
            "printf '{\"call_id\":\"%s\",\"result\":\"allowed=%s denied=%s\"}\\n' \"$cid\" ",
            "\"${PANTHEON_SUP_TEST_ALLOWED:-<unset>}\" \"${PANTHEON_SUP_TEST_DENIED:-<unset>}\"\n",
        ),
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
        name: "envprobe".into(),
        description: String::new(),
        version: "0.1.0".into(),
        sha: None,
        maintainer: String::new(),
        capabilities: vec![],
        env_vars: vec![
            EnvVarDecl {
                name: "PANTHEON_SUP_TEST_ALLOWED".into(),
                required: false,
                description: String::new(),
            },
            EnvVarDecl {
                name: "PANTHEON_SUP_TEST_DENIED".into(),
                required: false,
                description: String::new(),
            },
        ],
        runner: "run.sh".into(),
        enabled: true,
    };
    let allowlist = vec!["PANTHEON_SUP_TEST_ALLOWED".to_string()];
    let mut sup =
        PluginSupervisor::spawn(&runner, &manifest, &dir, Duration::from_secs(5), &allowlist)
            .unwrap();
    let out = sup.call("ping", serde_json::json!({})).unwrap();
    assert!(
        out.contains("allowed=visible"),
        "allowlisted var must reach the plugin, got: {out}"
    );
    assert!(
        out.contains("denied=<unset>"),
        "non-allowlisted declared var must NOT reach the plugin, got: {out}"
    );
    sup.stop();

    // Empty allowlist: nothing declared crosses, even when set on the host.
    let mut sup =
        PluginSupervisor::spawn(&runner, &manifest, &dir, Duration::from_secs(5), &[]).unwrap();
    let out = sup.call("ping", serde_json::json!({})).unwrap();
    assert!(
        out.contains("allowed=<unset>"),
        "empty allowlist must pass nothing, got: {out}"
    );
    sup.stop();

    std::env::remove_var("PANTHEON_SUP_TEST_ALLOWED");
    std::env::remove_var("PANTHEON_SUP_TEST_DENIED");
    let _ = std::fs::remove_dir_all(&dir);
}
