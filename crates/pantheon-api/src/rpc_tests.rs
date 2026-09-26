//! Tests for `pantheon_api::rpc::tests` — sibling file so sources stay test-free.
use super::*;

fn one_line(dispatcher: &Dispatcher, line: &str) -> Vec<Response> {
    dispatcher.handle_line(line)
}

#[test]
fn ping_round_trips_id_and_result() {
    let d = Dispatcher::with_builtins();
    let out = one_line(
        &d,
        r#"{"jsonrpc":"2.0","id":7,"method":"system.ping","params":null}"#,
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].id, Id::Number(7));
    assert!(out[0].is_success());
    assert_eq!(
        out[0].result.as_ref().unwrap().get("pong").unwrap(),
        &json!(true)
    );
    assert!(out[0].error.is_none());
}

#[test]
fn string_ids_and_objects_echo_exactly() {
    let d = Dispatcher::with_builtins();
    let out = one_line(
        &d,
        r#"{"jsonrpc":"2.0","id":"run_1","method":"system.ping"}"#,
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].id, Id::Str("run_1".into()));
    assert!(out[0].is_success());
}

#[test]
fn unknown_method_gets_structured_32601() {
    let d = Dispatcher::with_builtins();
    let out = one_line(&d, r#"{"jsonrpc":"2.0","id":1,"method":"agent.run"}"#);
    assert_eq!(out.len(), 1);
    let e = out[0].error.as_ref().unwrap();
    assert_eq!(e.code, METHOD_NOT_FOUND_CODE);
    assert!(e.message.contains("agent.run"));
    assert_eq!(out[0].id, Id::Number(1));
}

#[test]
fn notifications_are_executed_but_never_answered() {
    let d = Dispatcher::with_builtins();
    let line = r#"{"jsonrpc":"2.0","id":null,"method":"system.ping"}"#;
    assert!(one_line(&d, line).is_empty());
}

#[test]
fn unparseable_line_is_a_parse_error_with_null_id() {
    let d = Dispatcher::with_builtins();
    let out = one_line(&d, "not json at all");
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].id, Id::Null);
    assert_eq!(out[0].error.as_ref().unwrap().code, PARSE_ERROR_CODE);
}

#[test]
fn wrong_version_or_non_object_is_invalid_request() {
    let d = Dispatcher::with_builtins();
    let v = one_line(&d, r#"{"jsonrpc":"1.0","id":1,"method":"system.ping"}"#);
    assert_eq!(v[0].error.as_ref().unwrap().code, INVALID_REQUEST_CODE);
    let a = one_line(&d, r#"["a","b"]"#);
    assert_eq!(a[0].error.as_ref().unwrap().code, INVALID_REQUEST_CODE);
    let s = one_line(&d, r#""just a string""#);
    assert_eq!(s[0].error.as_ref().unwrap().code, INVALID_REQUEST_CODE);
}

#[test]
fn handler_errors_become_invalid_params_when_params_are_wrong() {
    let d = Dispatcher::new();
    d.register("echo", Echo);
    let ok = one_line(
        &d,
        r#"{"jsonrpc":"2.0","id":3,"method":"echo","params":{"text":"hi"}}"#,
    );
    assert_eq!(ok[0].result.as_ref().unwrap(), &json!({"text": "hi"}));
    let bad = one_line(
        &d,
        r#"{"jsonrpc":"2.0","id":3,"method":"echo","params":{}}"#,
    );
    assert_eq!(bad[0].error.as_ref().unwrap().code, INVALID_PARAMS_CODE);
}

#[test]
fn methods_lists_sorted_builtins() {
    let d = Dispatcher::with_builtins();
    let out = one_line(&d, r#"{"jsonrpc":"2.0","id":1,"method":"system.methods"}"#);
    let arr = out[0].result.as_ref().unwrap().as_array().unwrap();
    let names: Vec<&str> = arr.iter().map(|v| v.as_str().unwrap()).collect();
    assert!(names.contains(&"system.ping"));
    assert!(names.contains(&"system.methods"));
    // Sorted so a client sees a stable, diffable list.
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
}

struct Echo;
impl MethodHandler for Echo {
    fn call(&self, params: Option<Value>) -> Result<Value, RpcError> {
        let params = params.ok_or_else(|| RpcError::invalid_params("params object required"))?;
        let text = params
            .get("text")
            .ok_or_else(|| RpcError::invalid_params("field \"text\" is required"))?;
        Ok(json!({ "text": text }))
    }
}
