//! Behavioral tests for the MCP server manager, against the hermetic
//! fake server in `eval/tests/fixtures/fake_mcp_server.py`
//! (stdlib-only, no network).
//!
//! Covers the full outward path: stdio handshake, tools/list,
//! namespaced [`ToolRegistry`](pantheon_tools::tools::ToolRegistry)
//! registration, invocation of a registered tool, and the approval gate
//! — including that a version or content change invalidates an
//! approval. Each test gets a fresh temp dir as the manager scope, so
//! approvals never touch the real data dir.

use pantheon_api::approval::{ApprovalRecord, ApprovalStore};
use pantheon_mcp::manager::{McpManager, McpServerSpec, McpTransport, ServerStatus};
use pantheon_tools::tools::ToolRegistry;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

fn fixture() -> String {
    format!(
        "{}/tests/fixtures/fake_mcp_server.py",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn spec(name: &str, log: Option<&Path>) -> McpServerSpec {
    let mut args = vec![fixture(), "normal".to_string()];
    if let Some(l) = log {
        args.push(l.to_string_lossy().into_owned());
    }
    McpServerSpec {
        name: name.to_string(),
        transport: McpTransport::Stdio,
        command: Some("python3".to_string()),
        args,
        env: Default::default(),
        url: None,
        enabled: true,
        timeout: Duration::from_secs(10),
    }
}

fn scope() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn manager(dir: &Path, specs: Vec<McpServerSpec>) -> Arc<McpManager> {
    let m = Arc::new(McpManager::new(dir.to_path_buf()));
    m.configure(specs);
    m
}

fn scope_dir(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("mcp")
}

#[test]
fn unapproved_server_never_launches() {
    let tmp = scope();
    let log = tmp.path().join("methods.log");
    let m = manager(tmp.path(), vec![spec("fake", Some(&log))]);

    let mut reg = ToolRegistry::new();
    let report = m.register_tools(&mut reg);

    assert!(reg.names().is_empty(), "no tools without approval");
    assert!(report.registered.is_empty());
    assert!(report.pending_approval.contains(&"fake".to_string()));
    // The server process was never spawned: the fixture only writes its
    // method log after answering a request.
    assert!(
        !log.exists(),
        "unapproved server must not be launched at all"
    );
    let h = m.health();
    assert_eq!(h.len(), 1);
    assert_eq!(h[0].status, ServerStatus::Unapproved);
}

#[test]
fn approve_then_register_and_invoke() {
    let tmp = scope();
    let m = manager(tmp.path(), vec![spec("fake", None)]);

    let rec = m.approve_server("fake").expect("approve");
    assert_eq!(rec.version, "0");
    assert!(!rec.dir_hash.is_empty());

    let mut reg = ToolRegistry::new();
    let report = m.register_tools(&mut reg);
    assert_eq!(report.registered.len(), 2);
    assert!(report.pending_approval.is_empty());
    let mut names = reg.names();
    names.sort();
    assert_eq!(names, vec!["mcp_fake_echo", "mcp_fake_fail"]);

    // Invoke the registered tool through the registry, like the agent
    // loop would.
    let args = serde_json::json!({"a": 1, "b": "two"});
    let out = reg
        .execute("mcp_fake_echo", &args.to_string())
        .expect("invoke mcp_fake_echo");
    let echoed: serde_json::Value = serde_json::from_str(&out).expect("tool returns json text");
    assert_eq!(echoed, args);

    // The failing tool surfaces the server's error, not a crash.
    assert!(reg.execute("mcp_fake_fail", "{}").is_err());

    // Health reflects the live connection.
    let h = m.health();
    assert_eq!(h[0].status, ServerStatus::Ready);
    assert_eq!(h[0].tools, 2);
}

#[test]
fn version_change_invalidates_approval() {
    let tmp = scope();
    let m = manager(tmp.path(), vec![spec("fake", None)]);
    let rec = m.approve_server("fake").expect("approve");

    // Simulate the server upgrading: same name, new version, same bytes.
    ApprovalStore::open(&scope_dir(&tmp))
        .record(ApprovalRecord {
            plugin: rec.plugin.clone(),
            version: "999".to_string(),
            dir_hash: rec.dir_hash.clone(),
            approved_at_ms: rec.approved_at_ms,
        })
        .expect("tamper");

    let mut reg = ToolRegistry::new();
    let report = m.register_tools(&mut reg);
    assert!(reg.names().is_empty());
    assert!(report.pending_approval.contains(&"fake".to_string()));
}

#[test]
fn content_change_invalidates_approval() {
    let tmp = scope();
    let m = manager(tmp.path(), vec![spec("fake", None)]);
    let rec = m.approve_server("fake").expect("approve");

    // Simulate the binary/script changing under the same version.
    ApprovalStore::open(&scope_dir(&tmp))
        .record(ApprovalRecord {
            plugin: rec.plugin.clone(),
            version: rec.version.clone(),
            dir_hash: "deadbeef".to_string(),
            approved_at_ms: rec.approved_at_ms,
        })
        .expect("tamper");

    let mut reg = ToolRegistry::new();
    let report = m.register_tools(&mut reg);
    assert!(reg.names().is_empty());
    assert!(report.pending_approval.contains(&"fake".to_string()));
}

#[test]
fn tool_names_are_namespaced_and_sanitized() {
    let tmp = scope();
    let m = manager(
        tmp.path(),
        vec![
            spec("alpha", None),
            spec("beta", None),
            spec("My Server!", None),
        ],
    );
    for name in ["alpha", "beta", "My Server!"] {
        m.approve_server(name).expect("approve");
    }

    let mut reg = ToolRegistry::new();
    let report = m.register_tools(&mut reg);
    assert_eq!(report.registered.len(), 6);

    let mut names = reg.names();
    names.sort();
    assert_eq!(
        names,
        vec![
            "mcp_alpha_echo",
            "mcp_alpha_fail",
            "mcp_beta_echo",
            "mcp_beta_fail",
            "mcp_my_server_echo",
            "mcp_my_server_fail",
        ]
    );
    // Colliding servers never clobber each other.
    let out = reg
        .execute("mcp_beta_echo", r#"{"which":"beta"}"#)
        .expect("invoke");
    assert!(out.contains("beta"));
}

/// #5: revoking an approval blocks `call_tool` even while the
/// connection is still alive — the call must not ride a stale
/// connection past a lapsed/revoked approval. (Fails before the fix:
/// the call goes through on the live connection.)
#[test]
fn revoked_approval_blocks_call_tool_on_live_connection() {
    let tmp = scope();
    let m = manager(tmp.path(), vec![spec("fake", None)]);
    m.approve_server("fake").expect("approve");
    let mut reg = ToolRegistry::new();
    let report = m.register_tools(&mut reg);
    assert!(report.pending_approval.is_empty(), "server approved");

    // Sanity: the call works while the approval is live.
    let args = serde_json::json!({"a": 1});
    m.call_tool("fake", "echo", &args)
        .expect("call while approved");

    // Revoke out from under the live connection.
    ApprovalStore::open(&scope_dir(&tmp))
        .revoke("fake")
        .expect("revoke");

    let err = m
        .call_tool("fake", "echo", &args)
        .expect_err("revoked approval must block the call");
    let msg = err.to_string();
    assert!(msg.contains("fake"), "error names the server: {msg}");
    assert!(
        msg.contains("approv"),
        "error explains the approval problem: {msg}"
    );

    // The forced-reconnect path is gated too.
    assert!(
        m.retry_now("fake").is_err(),
        "retry_now must refuse a revoked server"
    );
}

/// #5 happy path: an approved, unchanged server still proceeds — the
/// per-call gate must not break the normal call flow.
#[test]
fn approved_unchanged_server_call_still_proceeds() {
    let tmp = scope();
    let m = manager(tmp.path(), vec![spec("fake", None)]);
    m.approve_server("fake").expect("approve");
    let mut reg = ToolRegistry::new();
    m.register_tools(&mut reg);
    // Direct call_tool (the agent-loop path), twice: the second call
    // rides the already-live connection through the gate.
    let args = serde_json::json!({"a": 1, "b": "two"});
    for _ in 0..2 {
        let out = m.call_tool("fake", "echo", &args).expect("call proceeds");
        // The fixture wraps the echo in an MCP content envelope.
        let text = out["content"][0]["text"].as_str().expect("text content");
        let echoed: serde_json::Value = serde_json::from_str(text).expect("echo payload");
        assert_eq!(echoed, args);
    }
    // A reconnect after the connection dies also re-passes the gate.
    m.retry_now("fake")
        .expect("retry_now proceeds while approved");
    m.call_tool("fake", "echo", &args)
        .expect("call after retry");
}

/// #7: for launcher-shim servers (npx/uvx) the content hash binds the
/// package spec + args — not the shim binary — so bumping the pin
/// lapses the approval. (Fails before the fix: the hash covers the
/// npx binary, identical across pins, so the approval never lapses.)
#[test]
fn launcher_pin_bump_lapses_approval() {
    fn npx_spec(pin: &str) -> McpServerSpec {
        McpServerSpec {
            name: "shimtest".to_string(),
            transport: McpTransport::Stdio,
            command: Some("npx".to_string()),
            args: vec!["-y".to_string(), pin.to_string()],
            env: Default::default(),
            url: None,
            enabled: true,
            timeout: Duration::from_secs(10),
        }
    }
    let tmp = scope();
    let m = manager(tmp.path(), vec![npx_spec("some-pkg@1.0.0")]);
    // No connect needed: pending_approval reports the configure-time hash.
    let hash_before = m
        .pending_approval()
        .into_iter()
        .find(|p| p.name == "shimtest")
        .expect("pending")
        .content_hash
        .expect("hash");
    ApprovalStore::open(&scope_dir(&tmp))
        .record(ApprovalRecord {
            plugin: "shimtest".to_string(),
            version: "1.0.0".to_string(),
            dir_hash: hash_before,
            approved_at_ms: 0,
        })
        .expect("record");
    assert!(m.pending_approval().is_empty(), "approved at pin 1.0.0");
    // Bump the pin: the hash must change, lapsing the approval.
    m.configure(vec![npx_spec("some-pkg@2.0.0")]);
    assert!(
        !m.pending_approval().is_empty(),
        "pin bump must lapse the approval"
    );
}
