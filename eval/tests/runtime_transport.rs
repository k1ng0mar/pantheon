//! Behavioral tests for the runtime's transport-agnostic JSON-RPC dispatcher.
//!
//! Policy: only small deterministic unit tests live beside the code
//! (`cargo test -p <crate>`). Everything behavioral - SQLite stores,
//! threads, sockets, subprocesses, timing, filesystem - lives here and
//! runs via `cargo test -p pantheon-eval`.
//!
//! The old unix-socket transport (`UnixSocketTransport`) was removed by the
//! gateway serve-surface refactor: the runtime stays client-agnostic and the
//! gateway owns all sockets. These tests exercise the same dispatcher
//! behavior in-process, through the public `dispatch` entry point.
//!
//! Sibling to the crate's unit suite so sources stay test-free.
use pantheon_runtime::rpc::{Dispatcher, Id, Request, Response};

/// Build a request and dispatch it, returning the response.
fn ask(d: &Dispatcher, id: i64, method: &str) -> Response {
    let req = Request {
        jsonrpc: "2.0".to_string(),
        id: Id::Number(id),
        method: method.to_string(),
        params: None,
    };
    d.dispatch(&req)
        .expect("dispatch returned None for a non-notification")
}

#[test]
fn ping_round_trips_through_the_dispatcher() {
    let d = Dispatcher::with_builtins();
    let resp = ask(&d, 7, "system.ping");
    assert_eq!(resp.id, Id::Number(7));
    assert!(resp.is_success());
    assert_eq!(
        resp.result.unwrap().get("pong").unwrap().as_bool(),
        Some(true)
    );
}

#[test]
fn unknown_method_errors() {
    let d = Dispatcher::with_builtins();
    let resp = ask(&d, 1, "agent.run");
    assert_eq!(resp.id, Id::Number(1));
    assert!(!resp.is_success());
    assert_eq!(
        resp.error.unwrap().code,
        pantheon_runtime::rpc::METHOD_NOT_FOUND_CODE
    );
}

#[test]
fn many_requests_dispatch_in_order() {
    let d = Dispatcher::with_builtins();
    let r1 = ask(&d, 1, "system.ping");
    let r2 = ask(&d, 2, "system.ping");
    assert_eq!(r1.id, Id::Number(1));
    assert_eq!(r2.id, Id::Number(2));
    assert!(r1.is_success() && r2.is_success());
}

#[test]
fn notifications_return_no_response() {
    let d = Dispatcher::with_builtins();
    let req = Request {
        jsonrpc: "2.0".to_string(),
        id: Id::Null,
        method: "system.ping".to_string(),
        params: None,
    };
    assert!(d.dispatch(&req).is_none(), "notifications must not answer");
}
