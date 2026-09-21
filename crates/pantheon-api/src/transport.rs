//! Transport-agnostic serving, plus the Unix-socket transport (§18).
//!
//! A transport moves newline-delimited JSON in and out; the protocol (and
//! every correctness property of it) lives in `crate::rpc`, exactly the same
//! for every surface. Unix socket is the first transport; WebSocket comes
//! later behind the same [`ApiTransport`] trait.

use crate::rpc::Dispatcher;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// A surface that carries JSON-RPC traffic for one [`Dispatcher`].
pub trait ApiTransport: Send + Sync {
    /// Blocking loop: accept connections and serve JSON-RPC until the
    /// listener is gone.
    fn serve(&self, dispatcher: &Dispatcher) -> io::Result<()>;
}

/// Unix-socket transport. Newline-delimited JSON-RPC 2.0 per connection.
///
/// The listener binds eagerly in [`bind`](Self::bind), so bind errors (bad
/// path, socket file already existing) surface before anything listens and a
/// client can safely connect as soon as bind returns. The socket file is
/// left behind; remove it when shutting down.
pub struct UnixSocketTransport {
    path: PathBuf,
    listener: UnixListener,
}

impl UnixSocketTransport {
    /// Create the socket file and start listening.
    pub fn bind(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        let listener = UnixListener::bind(&path)?;
        Ok(Self { path, listener })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Serve one connection to EOF, then return. Deterministic for tests,
    /// drivers, and one-shot CLI commands.
    pub fn serve_once(&self, dispatcher: &Dispatcher) -> io::Result<()> {
        let (stream, _addr) = self.listener.accept()?;
        self.handle(stream, dispatcher)
    }

    fn handle(&self, mut stream: UnixStream, dispatcher: &Dispatcher) -> io::Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut line = String::new();
        loop {
            line.clear();
            let read = reader.read_line(&mut line)?;
            if read == 0 {
                return Ok(()); // client closed
            }
            if line.trim().is_empty() {
                continue;
            }
            for response in dispatcher.handle_line(&line) {
                let encoded = serde_json::to_string(&response)
                    .map_err(|e| io::Error::other(format!("encode response: {e}")))?;
                stream.write_all(encoded.as_bytes())?;
                stream.write_all(b"\n")?;
            }
        }
    }
}

impl ApiTransport for UnixSocketTransport {
    fn serve(&self, dispatcher: &Dispatcher) -> io::Result<()> {
        for stream in self.listener.incoming() {
            match stream {
                Ok(stream) => {
                    // A rude client ends only its own connection.
                    let _ = self.handle(stream, dispatcher);
                }
                Err(err) => {
                    // Accept glitches (e.g. EINTR) are skipped; the server lives on.
                    eprintln!("pantheon-api: accept failed: {err}");
                    continue;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{Dispatcher, Id, Response};
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
        assert_eq!(resp.error.unwrap().code, crate::rpc::METHOD_NOT_FOUND_CODE);
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
}
