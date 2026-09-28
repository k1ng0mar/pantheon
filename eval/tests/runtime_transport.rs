//! Behavioral / integration tests moved out of the crate's unit suite.
//!
//! Policy: only small deterministic unit tests live beside the code
//! (`cargo test -p <crate>`). Everything behavioral — SQLite stores,
//! threads, sockets, subprocesses, timing, filesystem — lives here and
//! runs via `cargo test -p pantheon-eval`.

//! Tests for `pantheon_api::transport::tests` — sibling file so sources stay test-free.
use pantheon_runtime::rpc::{Dispatcher, Id, Response};
use pantheon_runtime::transport::*;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use tempfile::tempdir;

/// Open a client, send `request`, read exactly one response line, return it.
fn ask(sock: &Path, request: &str) -> Response {
    use std::io::BufRead;
    let mut client = UnixStream::connect(sock).unwrap();
    client.write_all(request.as_bytes()).unwrap();
    client.write_all(b"\n").unwrap();
    // Read from a clone so we control the original's lifetime precisely.
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

#[test]
fn command_round_trips_through_the_unix_socket() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("api.sock");
    let tx = UnixSocketTransport::bind(&sock).unwrap();
    let d = Dispatcher::with_builtins();

    let server = std::thread::spawn(move || tx.serve_once(&d));
    let resp = ask(
        &sock,
        r#"{"jsonrpc":"2.0","id":7,"method":"system.ping","params":null}"#,
    );
    assert_eq!(resp.id, Id::Number(7));
    assert!(resp.is_success());
    assert_eq!(
        resp.result.unwrap().get("pong").unwrap().as_bool(),
        Some(true)
    );
    assert!(server.join().is_ok(), "server thread panicked");
}

#[test]
fn unknown_method_errors_over_the_socket() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("api.sock");
    let tx = UnixSocketTransport::bind(&sock).unwrap();
    let d = Dispatcher::with_builtins();

    let server = std::thread::spawn(move || tx.serve_once(&d));
    let resp = ask(
        &sock,
        r#"{"jsonrpc":"2.0","id":1,"method":"agent.run","params":{}}"#,
    );
    assert_eq!(resp.id, Id::Number(1));
    assert!(!resp.is_success());
    assert_eq!(
        resp.error.unwrap().code,
        pantheon_runtime::rpc::METHOD_NOT_FOUND_CODE
    );
    assert!(server.join().is_ok(), "server thread panicked");
}

#[test]
fn one_connection_carries_many_requests_in_order() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("api.sock");
    let tx = UnixSocketTransport::bind(&sock).unwrap();
    let d = Dispatcher::with_builtins();

    let server = std::thread::spawn(move || tx.serve_once(&d));
    let mut client = UnixStream::connect(&sock).unwrap();
    client
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"system.ping\"}\n")
        .unwrap();
    client
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"system.ping\"}\n")
        .unwrap();
    // Scope the reader so its cloned socket handle is dropped before join:
    // the server only returns when it sees EOF.
    let (r1, r2) = {
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let mut line1 = String::new();
        reader.read_line(&mut line1).unwrap();
        let mut line2 = String::new();
        reader.read_line(&mut line2).unwrap();
        (
            serde_json::from_str::<Response>(&line1).unwrap(),
            serde_json::from_str::<Response>(&line2).unwrap(),
        )
    };
    drop(client);
    assert_eq!(r1.id, Id::Number(1));
    assert_eq!(r2.id, Id::Number(2));
    assert!(r1.is_success() && r2.is_success());
    assert!(server.join().is_ok(), "server thread panicked");
}
