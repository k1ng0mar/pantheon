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
