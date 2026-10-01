//! Behavioral tests for the MCP stdio client, against the hermetic fake
//! server in `eval/tests/fixtures/fake_mcp_server.py`
//! (stdlib-only, no network). Moved here from
//! `pantheon-mcp/src/client_tests.rs`: subprocess + stdio + timeouts, so it
//! runs under `cargo test -p pantheon-eval`, not beside the code.
use pantheon_mcp::{McpClient, McpError, McpServerConfig};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn fixture() -> String {
    format!(
        "{}/tests/fixtures/fake_mcp_server.py",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn config(mode: &str, log: Option<&std::path::Path>) -> McpServerConfig {
    let mut args = vec![fixture(), mode.to_string()];
    if let Some(l) = log {
        args.push(l.to_string_lossy().into_owned());
    }
    McpServerConfig::new("fake", "python3")
        .with_args(args)
        .with_timeout(Duration::from_secs(10))
}

fn allow_all(_: &str, _: &Value) -> Result<(), McpError> {
    Ok(())
}

fn read_log(path: &std::path::Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[test]
fn handshake_lists_and_calls() {
    let mut client = McpClient::connect(&config("normal", None), None).expect("connect");
    assert_eq!(client.negotiated_version, "2025-03-26");
    assert!(client.alive());

    let tools = client.list_tools().expect("tools/list");
    assert_eq!(tools.len(), 2);
    let echo = &tools[0];
    assert_eq!(echo.server, "fake");
    assert_eq!(echo.name, "echo");
    assert_eq!(echo.description, "Echoes its arguments back");
    assert_eq!(echo.input_schema, serde_json::json!({"type": "object"}));
    // Convertible into a pantheon-tools registration schema.
    let schema = echo.to_tool_schema();
    assert_eq!(schema.name, "echo");
    assert_eq!(schema.parameters, serde_json::json!({"type": "object"}));

    let args = serde_json::json!({"a": 1, "b": "two"});
    let result = client
        .call_tool("echo", &args, &allow_all)
        .expect("tools/call");
    let text = result["content"][0]["text"].as_str().expect("text block");
    assert_eq!(serde_json::from_str::<Value>(text).unwrap(), args);
}

#[test]
fn tool_error_is_structured() {
    let mut client = McpClient::connect(&config("normal", None), None).expect("connect");
    let err = client
        .call_tool("fail", &Value::Null, &allow_all)
        .expect_err("fail tool must error");
    assert_eq!(err, McpError::ServerToolError("boom".into()));
}

#[test]
fn gate_denial_aborts_before_wire() {
    let log = std::env::temp_dir().join(format!("mcp-gate-{}", std::process::id()));
    let _ = std::fs::remove_file(&log);
    let mut client = McpClient::connect(&config("normal", Some(&log)), None).expect("connect");
    let deny = |_: &str, _: &Value| Err(McpError::GateDenied("policy says no".into()));
    let err = client
        .call_tool("echo", &serde_json::json!({}), &deny)
        .expect_err("gate must deny");
    assert!(matches!(err, McpError::GateDenied(_)), "got {err:?}");
    // The handshake reached the server, but the denied call never did.
    let seen = read_log(&log);
    assert!(seen.contains("initialize"), "log: {seen}");
    assert!(!seen.contains("tools/call"), "log: {seen}");
    let _ = std::fs::remove_file(&log);
}

#[test]
fn gate_sees_name_and_args() {
    let mut client = McpClient::connect(&config("normal", None), None).expect("connect");
    let seen: Arc<Mutex<Vec<(String, Value)>>> = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let gate = move |name: &str, args: &Value| {
        seen2.lock().unwrap().push((name.to_string(), args.clone()));
        Ok(())
    };
    let args = serde_json::json!({"x": true});
    client.call_tool("echo", &args, &gate).expect("call");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "echo");
    assert_eq!(seen[0].1, args);
}

#[test]
fn timeout_kills_hanging_server() {
    let cfg = config("hang", None).with_timeout(Duration::from_secs(1));
    let mut client = McpClient::connect(&cfg, None).expect("connect");
    let err = client
        .call_tool("echo", &Value::Null, &allow_all)
        .expect_err("hang must time out");
    assert!(matches!(err, McpError::Timeout { .. }), "got {err:?}");
    // The child was killed, not left hanging.
    assert!(!client.alive());
    // And the client is dead: no further requests.
    assert_eq!(
        client.list_tools().expect_err("dead client"),
        McpError::Closed
    );
}

#[test]
fn oversized_message_rejected() {
    let mut client = McpClient::connect(&config("oversize", None), None).expect("connect");
    let err = client.list_tools().expect_err("oversize must fail");
    assert!(matches!(err, McpError::Oversized { .. }), "got {err:?}");
}

#[test]
fn malformed_jsonrpc_is_structured_error() {
    let mut client = McpClient::connect(&config("garbage", None), None).expect("connect");
    let err = client.list_tools().expect_err("garbage must fail");
    assert!(matches!(err, McpError::Framing(_)), "got {err:?}");
}

#[test]
fn unknown_protocol_version_rejected() {
    let err = match McpClient::connect(&config("badversion", None), None) {
        Err(e) => e,
        Ok(_) => panic!("bad version must fail"),
    };
    assert!(matches!(err, McpError::Protocol(_)), "got {err:?}");
}

#[test]
fn spawn_failure_is_structured() {
    let cfg = McpServerConfig::new("nope", "pantheon-definitely-not-a-real-binary");
    let err = match McpClient::connect(&cfg, None) {
        Err(e) => e,
        Ok(_) => panic!("missing binary must fail"),
    };
    assert!(matches!(err, McpError::Spawn(_)), "got {err:?}");
}
