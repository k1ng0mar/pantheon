//! JSON-RPC 2.0 protocol types and method dispatch (§18).
//!
//! The runtime answers commands over a transport; the transport does nothing
//! but move JSON lines - this module owns the protocol. One request id maps
//! to exactly one response id, notifications (id `null`) are executed but
//! never answered, and every failure - bad JSON, unknown method, bad params,
//! handler error - becomes a structured error response, never a crash.

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
    /// invalid-request response.
    ///
    /// Batch arrays follow JSON-RPC 2.0 (§6): each element is dispatched in
    /// order and its responses concatenated. An empty array is an invalid
    /// request; a batch in which every element is a notification produces no
    /// response at all, so the caller must not assume one reply per line.
    pub fn handle_line(&self, line: &str) -> Vec<Response> {
        let value: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => return vec![Response::err(Id::Null, RpcError::parse_error())],
        };
        let Some(items) = value.as_array() else {
            return self.handle_value(value);
        };
        // §6: an empty batch is an invalid request, not a no-op.
        if items.is_empty() {
            return vec![Response::err(Id::Null, RpcError::invalid_request())];
        }
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            out.extend(self.handle_value(item.clone()));
        }
        out
    }

    /// Dispatch one already-parsed JSON value into zero or one response.
    fn handle_value(&self, value: Value) -> Vec<Response> {
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

/// `system.ping` - liveness probe.
struct Ping;
impl MethodHandler for Ping {
    fn call(&self, _params: Option<Value>) -> Result<Value, RpcError> {
        Ok(json!({ "pong": true }))
    }
}

/// `system.methods` - the currently registered command names.
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
mod batch_tests {
    use super::*;

    fn d() -> Dispatcher {
        Dispatcher::with_builtins()
    }

    /// Batch arrays used to be rejected wholesale. They are part of JSON-RPC
    /// 2.0 (§6) and clients send them, so they must dispatch element-wise.
    #[test]
    fn batch_dispatches_every_element_in_order() {
        let line = r#"[
            {"jsonrpc":"2.0","id":1,"method":"system.ping"},
            {"jsonrpc":"2.0","id":2,"method":"system.ping"}
        ]"#;
        let out = d().handle_line(line);
        assert_eq!(out.len(), 2, "one response per request");
        assert_eq!(out[0].id, Id::Number(1));
        assert_eq!(out[1].id, Id::Number(2));
        assert!(out.iter().all(|r| r.is_success()));
    }

    /// §6: an empty array is an invalid request, not a silent success.
    #[test]
    fn empty_batch_is_invalid_request() {
        let out = d().handle_line("[]");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].error.as_ref().unwrap().code, INVALID_REQUEST_CODE);
    }

    /// A batch of notifications must produce no responses at all. A server
    /// that wrote a `null` reply here would corrupt the client's stream.
    #[test]
    fn notification_only_batch_answers_nothing() {
        let line = r#"[
            {"jsonrpc":"2.0","id":null,"method":"system.ping"},
            {"jsonrpc":"2.0","id":null,"method":"system.ping"}
        ]"#;
        assert!(d().handle_line(line).is_empty());
    }

    /// Mixed batches answer only the calls that carry an id.
    #[test]
    fn mixed_batch_answers_only_requests() {
        let line = r#"[
            {"jsonrpc":"2.0","id":null,"method":"system.ping"},
            {"jsonrpc":"2.0","id":7,"method":"system.ping"}
        ]"#;
        let out = d().handle_line(line);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, Id::Number(7));
    }

    /// One bad element must not poison its siblings: §6 has servers answer
    /// per-element, and an unknown method is a normal error response.
    #[test]
    fn unknown_method_in_batch_fails_only_its_element() {
        let line = r#"[
            {"jsonrpc":"2.0","id":1,"method":"system.ping"},
            {"jsonrpc":"2.0","id":2,"method":"no.such.method"}
        ]"#;
        let out = d().handle_line(line);
        assert_eq!(out.len(), 2);
        assert!(out[0].is_success());
        assert_eq!(
            out[1].error.as_ref().unwrap().code,
            METHOD_NOT_FOUND_CODE,
            "a bad element must answer method-not-found, not fail the batch"
        );
    }

    /// A single (non-array) request keeps behaving exactly as before.
    #[test]
    fn single_object_still_answers_one_response() {
        let out = d().handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"system.ping"}"#);
        assert_eq!(out.len(), 1);
        assert!(out[0].is_success());
    }

    /// Malformed JSON is still a parse error with a null id.
    #[test]
    fn malformed_json_is_still_a_parse_error() {
        let out = d().handle_line("{not json");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].error.as_ref().unwrap().code, PARSE_ERROR_CODE);
        assert_eq!(out[0].id, Id::Null);
    }
}
