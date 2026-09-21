//! JSON-RPC 2.0 protocol types and method dispatch (§18).
//!
//! The runtime answers commands over a transport; the transport does nothing
//! but move JSON lines — this module owns the protocol. One request id maps
//! to exactly one response id, notifications (id `null`) are executed but
//! never answered, and every failure — bad JSON, unknown method, bad params,
//! handler error — becomes a structured error response, never a crash.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// JSON-RPC 2.0 error codes.
pub const PARSE_ERROR_CODE: i32 = -32700;
pub const INVALID_REQUEST_CODE: i32 = -32600;
pub const METHOD_NOT_FOUND_CODE: i32 = -32601;
pub const INVALID_PARAMS_CODE: i32 = -32602;
pub const INTERNAL_ERROR_CODE: i32 = -32603;

/// Request id. `Null` marks a notification (no response is sent).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    Number(i64),
    Str(String),
    Null,
}

/// A structured JSON-RPC error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn parse_error() -> Self {
        Self::new(
            PARSE_ERROR_CODE,
            "parse error: the message was not valid JSON-RPC 2.0",
        )
    }

    pub fn invalid_request() -> Self {
        Self::new(
            INVALID_REQUEST_CODE,
            "invalid request: expected a JSON-RPC 2.0 request object",
        )
    }

    pub fn method_not_found(method: &str) -> Self {
        Self::new(METHOD_NOT_FOUND_CODE, format!("method not found: {method}"))
    }

    pub fn invalid_params(detail: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS_CODE, detail.into())
    }

    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(INTERNAL_ERROR_CODE, detail.into())
    }
}

/// One inbound command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: Id,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// One outbound answer: a result xor an error, echoing the request id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: Id,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn ok(id: Id, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Id, error: RpcError) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(error),
        }
    }

    pub fn is_success(&self) -> bool {
        self.error.is_none()
    }
}

/// A single command handler. Handlers are pure over their params: all
/// runtime access goes through the runtime that constructs them.
pub trait MethodHandler: Send + Sync {
    fn call(&self, params: Option<Value>) -> Result<Value, RpcError>;
}

/// Method registry + dispatch. Thread-safe: a server may answer many
/// connections concurrently. Registering an existing name replaces it.
pub struct Dispatcher {
    methods: Arc<RwLock<HashMap<String, Arc<dyn MethodHandler>>>>,
}

impl Default for Dispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Dispatcher {
    pub fn new() -> Self {
        Self {
            methods: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// A dispatcher with the transport-agnostic built-ins:
    /// `system.ping` and `system.methods`.
    pub fn with_builtins() -> Self {
        let d = Self::new();
        d.register("system.ping", Ping);
        d.register(
            "system.methods",
            MethodList {
                methods: Arc::clone(&d.methods),
            },
        );
        d
    }

    pub fn register<H: MethodHandler + 'static>(&self, name: impl Into<String>, handler: H) {
        self.methods
            .write()
            .expect("dispatcher lock poisoned")
            .insert(name.into(), Arc::new(handler));
    }

    pub fn unregister(&self, name: &str) -> bool {
        self.methods
            .write()
            .expect("dispatcher lock poisoned")
            .remove(name)
            .is_some()
    }

    /// Registered method names, sorted.
    pub fn methods(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .methods
            .read()
            .expect("dispatcher lock poisoned")
            .keys()
            .cloned()
            .collect();
        names.sort();
        names
    }

    /// Dispatch one request. Notifications (`id: null`) return `None`.
    pub fn dispatch(&self, req: &Request) -> Option<Response> {
        let response = self.invoke(req);
        if req.id == Id::Null {
            None
        } else {
            Some(response)
        }
    }

    fn invoke(&self, req: &Request) -> Response {
        let Some(handler) = self
            .methods
            .read()
            .expect("dispatcher lock poisoned")
            .get(&req.method)
            .cloned()
        else {
            return Response::err(req.id.clone(), RpcError::method_not_found(&req.method));
        };
        match handler.call(req.params.clone()) {
            Ok(value) => Response::ok(req.id.clone(), value),
            Err(e) => Response::err(req.id.clone(), e),
        }
    }

    /// Handle one newline-delimited JSON line and produce its responses.
    ///
    /// Covers the protocol-level failures the spec requires to be answers,
    /// not crashes: a line that is not JSON gets a parse-error response with
    /// id `null`; a non-request or wrong-version object gets an
    /// invalid-request response. Batch arrays are rejected for now — the
    /// runtime answers one command per line.
    pub fn handle_line(&self, line: &str) -> Vec<Response> {
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return vec![Response::err(Id::Null, RpcError::parse_error())],
        };
        if value.is_array() {
            return vec![Response::err(Id::Null, RpcError::invalid_request())];
        }
        let req: Request = match serde_json::from_value(value) {
            Ok(r) => r,
            Err(_) => return vec![Response::err(Id::Null, RpcError::invalid_request())],
        };
        if req.jsonrpc != "2.0" {
            return vec![Response::err(Id::Null, RpcError::invalid_request())];
        }
        match self.dispatch(&req) {
            Some(resp) => vec![resp],
            None => vec![],
        }
    }
}

/// `system.ping` — liveness probe.
struct Ping;
impl MethodHandler for Ping {
    fn call(&self, _params: Option<Value>) -> Result<Value, RpcError> {
        Ok(json!({ "pong": true }))
    }
}

/// `system.methods` — the currently registered command names.
struct MethodList {
    methods: Arc<RwLock<HashMap<String, Arc<dyn MethodHandler>>>>,
}
impl MethodHandler for MethodList {
    fn call(&self, _params: Option<Value>) -> Result<Value, RpcError> {
        let mut names: Vec<String> = self
            .methods
            .read()
            .expect("dispatcher lock poisoned")
            .keys()
            .cloned()
            .collect();
        names.sort();
        Ok(Value::Array(names.into_iter().map(Value::String).collect()))
    }
}

#[cfg(test)]
mod tests {
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
            let params =
                params.ok_or_else(|| RpcError::invalid_params("params object required"))?;
            let text = params
                .get("text")
                .ok_or_else(|| RpcError::invalid_params("field \"text\" is required"))?;
            Ok(json!({ "text": text }))
        }
    }
}
