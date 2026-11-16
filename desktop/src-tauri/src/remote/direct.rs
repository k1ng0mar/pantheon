//! Direct computer-to-computer links: this computer listens for a paired computer, or
//! connects to one. The frames are the relay's, sealed end to end, so the host, the
//! mirrors and the trust pins are unchanged; only the path between them differs.

use super::{config, crypto, host, remote, socket, Device, Inner};
use crate::Result;
use chacha20poly1305::{aead::OsRng, AeadCore, XChaCha20Poly1305};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::AppHandle;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

/// The port a direct host listens on when the user has not chosen one.
pub(crate) const DEFAULT_PORT: u16 = 7447;
const CODE: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
const CODE_LEN: usize = 8;
/// A pairing code is good for this long, and for every computer that types it.
const CODE_TTL: Duration = Duration::from_secs(10 * 60);
/// Wrong codes allowed before the listener refuses them for `LOCKOUT`.
const ATTEMPTS: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(60);
const TIMEOUT: Duration = Duration::from_secs(20);
const MAX_ADDRESS: usize = 256;

/// A pairing code shown on the host and typed on the computer joining it.
#[derive(Debug, Clone)]
pub(crate) struct Pairing {
    pub code: String,
    pub expires: Instant,
}

/// The device row a computer sends while pairing.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Introduction {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub platform: String,
    #[serde(default)]
    pub public_key: String,
    #[serde(default)]
    pub host: bool,
}

/// A short, unambiguous code: 8 of 32 symbols, so no `0/O` or `1/I` guessing.
fn generate_code() -> String {
    let random = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    (0..CODE_LEN).map(|i| CODE[random[i] as usize % CODE.len()] as char).collect()
}

/// Compares two codes without short-circuiting, so a wrong prefix costs as much as a
/// wrong last symbol.
fn same_code(expected: &str, typed: &str) -> bool {
    !expected.is_empty()
        && expected.len() == typed.len()
        && expected.bytes().zip(typed.bytes()).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
}

/// Loopback, private and link-local addresses, plus `.local` names: the ones a plain
/// WebSocket is safe on. A public address needs TLS, which is the tailnet work.
fn local_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".local") {
        return true;
    }
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => {
            let [a, b, _, _] = v4.octets();
            a == 10 || a == 127 || (a == 172 && (16..32).contains(&b)) || (a == 192 && b == 168) || (a == 169 && b == 254)
        }
        Ok(IpAddr::V6(v6)) => {
            let segments = v6.segments();
            v6.is_loopback() || (segments[0] & 0xfe00) == 0xfc00 || (segments[0] & 0xffc0) == 0xfe80
        }
        Err(_) => false,
    }
}

/// `host:port` for a computer on this network, as a plain WebSocket URL.
fn ws_url(address: &str) -> Result<String> {
    let address = address.trim();
    if address.is_empty() || address.len() > MAX_ADDRESS || address.contains(char::is_whitespace) {
        return Err("That address is not usable.".into());
    }
    let (name, port) = address.rsplit_once(':').ok_or("Enter the computer's address as host:port.")?;
    if name.is_empty() || port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
        return Err("Enter the computer's address as host:port.".into());
    }
    let port: u16 = port.parse().map_err(|_| "That port is not usable.".to_string())?;
    if port == 0 {
        return Err("That port is not usable.".into());
    }
    let name = name.trim_start_matches('[').trim_end_matches(']');
    if !local_host(name) {
        return Err("That address is not on this network. A public address needs TLS, which is not built yet.".into());
    }
    Ok(format!("ws://{name}:{port}/"))
}

/// Wrong pairing codes, so a stranger on the network cannot guess one all night.
#[derive(Default)]
struct Attempts {
    failed: u32,
    locked: Option<Instant>,
}

impl Attempts {
    fn refused(&self) -> bool {
        self.locked.is_some_and(|until| until > Instant::now())
    }

    fn failure(&mut self) {
        self.failed += 1;
        if self.failed >= ATTEMPTS {
            self.failed = 0;
            self.locked = Some(Instant::now() + LOCKOUT);
        }
    }

    fn success(&mut self) {
        self.failed = 0;
        self.locked = None;
    }
}

/// The code a computer types to join this one, making a fresh one when the last expired.
pub(crate) fn code(app: &tauri::AppHandle) -> String {
    let state = remote(app);
    let mut inner = state.lock();
    let fresh = inner.pairing.as_ref().is_some_and(|pairing| pairing.expires > Instant::now());
    if !fresh {
        inner.pairing = Some(Pairing { code: generate_code(), expires: Instant::now() + CODE_TTL });
    }
    inner.pairing.as_ref().map(|pairing| pairing.code.clone()).unwrap_or_default()
}

/// The direct half of `remote_status`: the listener, the computers paired here and the
/// ones this computer is connected to.
pub(crate) fn status(inner: &Inner) -> Value {
    let hosts: Vec<Value> = inner
        .settings
        .direct
        .hosts
        .iter()
        .map(|host| json!({"address": host.address, "deviceId": host.device_id, "name": host.name}))
        .collect();
    let connected: Vec<String> = inner.direct_peers.keys().cloned().collect();
    json!({
        "listening": inner.direct_port.is_some(),
        "port": inner.direct_port.unwrap_or(inner.settings.direct.port),
        "connected": connected,
        "hosts": hosts,
    })
}

/// Starts listening for paired computers. Hosting must already be on: the listener is
/// the same power as remote control, and pairing only gates who may ask.
pub(crate) async fn enable(app: AppHandle, port: u16) -> Result<u16> {
    if !remote(&app).lock().settings.host_enabled {
        return Err("Turn on Allow remote control before opening a direct connection.".into());
    }
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .map_err(|error| format!("Could not listen on port {port}: {error}"))?;
    let bound = listener.local_addr().map(|address| address.port()).unwrap_or(port);
    let (generation, settings) = {
        let state = remote(&app);
        let mut inner = state.lock();
        inner.direct_generation += 1;
        inner.direct_port = Some(bound);
        inner.settings.direct.port = bound;
        (inner.direct_generation, inner.settings.clone())
    };
    remote(&app).save(&settings);
    tauri::async_runtime::spawn(accept(app, listener, generation));
    Ok(bound)
}

/// Stops listening and drops every direct connection.
pub(crate) fn disable(app: &tauri::AppHandle) {
    let peers: Vec<String> = {
        let state = remote(app);
        let mut inner = state.lock();
        inner.direct_generation += 1;
        inner.direct_port = None;
        inner.pairing = None;
        let peers = inner.direct_peers.keys().cloned().collect::<Vec<_>>();
        inner.direct_peers.clear();
        inner.settings.direct.port = 0;
        state.save(&inner.settings.clone());
        peers
    };
    let state = remote(app);
    let mut inner = state.lock();
    for peer in peers {
        inner.fail_pending(Some(&peer), "That computer disconnected.");
    }
}

/// Connects this computer to a host on the same network and pairs with its code.
pub(crate) async fn connect(app: AppHandle, address: String, code: String) -> Result<Value> {
    let url = ws_url(&address)?;
    let request = url.into_client_request().map_err(|_| "That address is not usable.")?;
    let (socket, _) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(request))
        .await
        .map_err(|_| "That computer did not answer in time.")?
        .map_err(|_| "Could not reach that computer. Check the address and that it is listening.")?;
    let mine = introduction_value(&app, false)?;
    let (socket, joined) = match pair_out(socket, &code, mine).await {
        Ok(paired) => paired,
        Err(error) => return Err(error),
    };
    let host = register(app.clone(), joined, address);
    tauri::async_runtime::spawn(linked(app.clone(), socket, host.id.clone()));
    tauri::async_runtime::spawn(super::mirror::subscribe_all(app.clone()));
    let state = remote(&app);
    let status = status(&state.lock());
    Ok(status)
}

/// Forgets a paired computer: its key, its address and its place in the device list.
pub(crate) fn forget(app: &tauri::AppHandle, device_id: &str) -> Result<Value> {
    host::valid_id(device_id).map_err(|_| "Unknown computer.".to_string())?;
    let state = remote(app);
    let settings = {
        let mut inner = state.lock();
        inner.settings.trusted.remove(device_id);
        inner.settings.direct.hosts.retain(|host| host.device_id != device_id);
        inner.devices.retain(|device| device.id != device_id);
        inner.direct_peers.remove(device_id);
        inner.keys.remove(device_id);
        inner.catalog.forget(device_id);
        inner.settings.clone()
    };
    state.save(&settings);
    let status = status(&state.lock());
    Ok(status)
}

/// Accepts computers until the listener is replaced. Each connection pairs first, then
/// carries the same frames the relay would.
async fn accept(app: AppHandle, listener: TcpListener, generation: u64) {
    let attempts = Arc::new(Mutex::new(Attempts::default()));
    while remote(&app).lock().direct_generation == generation {
        let (stream, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(_) => continue,
        };
        let app = app.clone();
        let attempts = attempts.clone();
        tauri::async_runtime::spawn(serve(app, stream, generation, attempts));
    }
}

async fn serve(app: AppHandle, stream: TcpStream, generation: u64, attempts: Arc<Mutex<Attempts>>) {
    let Ok(socket) = tokio_tungstenite::accept_async(stream).await else { return };
    let (expected, trusted, reply) = {
        let state = remote(&app);
        let inner = state.lock();
        let expected = inner
            .pairing
            .as_ref()
            .filter(|pairing| pairing.expires > Instant::now())
            .map(|pairing| pairing.code.clone());
        (expected, inner.settings.trusted.clone(), introduction_value(&app, true).ok())
    };
    let Some((socket, introduction)) = pair_in(socket, expected.as_deref(), &trusted, reply, &attempts).await else { return };
    if remote(&app).lock().direct_generation != generation {
        return;
    }
    let id = introduction.id.clone();
    register_host(&app, introduction);
    linked(app.clone(), socket, id).await;
}

/// Reads the pairing frame and answers with this computer's own row. A computer already
/// trusted here, with the same key, may come back without a code.
async fn pair_in<S>(
    socket: S,
    expected: Option<&str>,
    trusted: &std::collections::BTreeMap<String, String>,
    reply: Option<Value>,
    attempts: &Mutex<Attempts>,
) -> Option<(S, Introduction)>
where
    S: Stream<Item = std::result::Result<Message, WsError>> + Sink<Message, Error = WsError> + Unpin,
{
    let (mut sink, mut stream) = socket.split();
    let framed = tokio::time::timeout(TIMEOUT, stream.next()).await.ok()?.and_then(|framed| framed.ok());
    let text = match framed {
        Some(Message::Text(text)) => text,
        _ => return None,
    };
    let value: Value = serde_json::from_str(&text).ok()?;
    if value["type"].as_str() != Some("pair") {
        return None;
    }
    let introduction: Introduction = serde_json::from_value(value["device"].clone()).ok()?;
    if host::valid_id(&introduction.id).is_err() || crypto::fingerprint(&introduction.public_key).is_empty() {
        return None;
    }
    let refused = {
        let locked = attempts.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        locked.refused()
    };
    if refused {
        let _ = sink.send(Message::text(json!({"type": "pair-fail", "error": "Too many wrong codes. Try again in a minute."}).to_string())).await;
        return None;
    }
    let known = trusted.get(&introduction.id).is_some_and(|pinned| pinned == &introduction.public_key);
    let accepted = known || expected.is_some_and(|code| same_code(code, value["code"].as_str().unwrap_or_default()));
    if !accepted {
        attempts.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).failure();
        let _ = sink.send(Message::text(json!({"type": "pair-fail", "error": "That code is not right."}).to_string())).await;
        return None;
    }
    attempts.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).success();
    let reply = reply?;
    sink.send(Message::text(json!({"type": "pair-ok", "device": reply}).to_string())).await.ok()?;
    Some((sink.reunite(stream).ok()?, introduction))
}

/// Sends this computer's pairing frame and reads the host's row.
async fn pair_out<S>(socket: S, code: &str, introduction: Value) -> Result<(S, Introduction)>
where
    S: Stream<Item = std::result::Result<Message, WsError>> + Sink<Message, Error = WsError> + Unpin,
{
    let (mut sink, mut stream) = socket.split();
    let frame = json!({"type": "pair", "code": code, "device": introduction});
    sink.send(Message::text(frame.to_string())).await.map_err(|_| "Could not reach that computer.")?;
    let framed = tokio::time::timeout(TIMEOUT, stream.next()).await.map_err(|_| "That computer did not answer in time.")?;
    let framed = framed.ok_or("That computer closed the connection.")?;
    let text = match framed {
        Ok(Message::Text(text)) => text,
        _ => return Err("That computer closed the connection.".into()),
    };
    let value: Value = serde_json::from_str(&text).map_err(|_| "That computer sent an unreadable reply.".to_string())?;
    match value["type"].as_str() {
        Some("pair-ok") => {
            let introduction: Introduction = serde_json::from_value(value["device"].clone()).map_err(|_| "That computer sent an unreadable reply.".to_string())?;
            if host::valid_id(&introduction.id).is_err() || crypto::fingerprint(&introduction.public_key).is_empty() {
                return Err("That computer sent an unusable identity.".into());
            }
            Ok((sink.reunite(stream).map_err(|_| "That computer closed the connection.")?, introduction))
        }
        Some("pair-fail") => Err(value["error"].as_str().unwrap_or("That code is not right.").chars().take(200).collect()),
        _ => Err("That computer did not answer the pairing request.".into()),
    }
}

/// This computer's row, as it introduces itself. `host` is whether remote control is on.
fn introduction_value(app: &tauri::AppHandle, host: bool) -> Result<Value> {
    let state = remote(app);
    let (id, public_key) = {
        let inner = state.lock();
        (inner.settings.device_id.clone(), crypto::public_key(&state.secret()?))
    };
    Ok(json!({
        "id": id,
        "name": config::device_name(),
        "platform": std::env::consts::OS,
        "publicKey": public_key,
        "host": host,
    }))
}

/// Pins a computer that just paired with this one and lists it as connected.
fn register_host(app: &tauri::AppHandle, introduction: Introduction) {
    let id = introduction.id.clone();
    let state = remote(app);
    let settings = {
        let mut inner = state.lock();
        inner.settings.trusted.insert(id.clone(), introduction.public_key.clone());
        inner.devices.retain(|device| device.id != id);
        inner.devices.push(Device {
            id: id.clone(),
            name: introduction.name.clone(),
            platform: introduction.platform.clone(),
            public_key: introduction.public_key.clone(),
            host: introduction.host,
            online: true,
            last_seen: Value::Null,
        });
        inner.settings.clone()
    };
    state.save(&settings);
    super::emit_presence(app);
}

/// Pins the host this computer paired with, remembers its address and lists it.
fn register(app: AppHandle, introduction: Introduction, address: String) -> Introduction {
    let id = introduction.id.clone();
    let state = remote(&app);
    let settings = {
        let mut inner = state.lock();
        inner.settings.trusted.insert(id.clone(), introduction.public_key.clone());
        inner.devices.retain(|device| device.id != id);
        inner.devices.push(Device {
            id: id.clone(),
            name: introduction.name.clone(),
            platform: introduction.platform.clone(),
            public_key: introduction.public_key.clone(),
            host: introduction.host,
            online: true,
            last_seen: Value::Null,
        });
        inner.settings.direct.hosts.retain(|host| host.device_id != id);
        inner.settings.direct.hosts.push(super::config::PairedHost { address, device_id: id.clone(), name: introduction.name.clone() });
        inner.settings.clone()
    };
    state.save(&settings);
    super::emit_presence(&app);
    introduction
}

/// Carries frames between two paired computers until either side closes. Outgoing frames
/// go out through this connection; incoming ones go through the same routing the relay
/// socket uses, so a request is answered by `host::handle` and a reply settles its call.
async fn linked<S>(app: AppHandle, socket: S, peer: String)
where
    S: Stream<Item = std::result::Result<Message, WsError>> + Sink<Message, Error = WsError> + Unpin,
{
    let (mut write, mut read) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    {
        let state = remote(&app);
        let mut inner = state.lock();
        inner.direct_peers.insert(peer.clone(), tx);
    }
    let mut peers: HashMap<String, mpsc::Sender<socket::Request>> = HashMap::new();
    loop {
        tokio::select! {
            outgoing = rx.recv() => match outgoing {
                Some(text) => if write.send(Message::text(text)).await.is_err() { break },
                None => break,
            },
            incoming = read.next() => match incoming {
                Some(Ok(Message::Text(text))) => socket::frame(&app, text.as_str(), &mut peers),
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    {
        let state = remote(&app);
        let mut inner = state.lock();
        inner.direct_peers.remove(&peer);
        inner.devices.retain(|device| device.id != peer);
        inner.fail_pending(Some(&peer), "That computer disconnected.");
    }
    super::emit_presence(&app);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_long_and_unambiguous() {
        for _ in 0..64 {
            let code = generate_code();
            assert_eq!(code.len(), CODE_LEN);
            assert!(code.bytes().all(|b| CODE.contains(&b)), "{code}");
        }
        assert_ne!(generate_code(), generate_code());
    }

    #[test]
    fn a_code_matches_only_itself() {
        assert!(same_code("ABCD2345", "ABCD2345"));
        assert!(!same_code("ABCD2345", "ABCD2346"));
        assert!(!same_code("ABCD2345", "ABCD234"));
        assert!(!same_code("ABCD2345", ""));
        assert!(!same_code("", ""));
    }

    #[test]
    fn only_a_local_address_gets_a_plain_socket() {
        assert_eq!(ws_url("192.168.1.5:7447").unwrap(), "ws://192.168.1.5:7447/");
        assert_eq!(ws_url("10.0.0.2:1").unwrap(), "ws://10.0.0.2:1/");
        assert_eq!(ws_url("127.0.0.1:7447").unwrap(), "ws://127.0.0.1:7447/");
        assert_eq!(ws_url("studio.local:7447").unwrap(), "ws://studio.local:7447/");
        assert_eq!(ws_url("[::1]:7447").unwrap(), "ws://::1:7447/");
        for address in ["8.8.8.8:7447", "example.com:7447", "192.168.1.5", "192.168.1.5:0", "192.168.1.5:x", ":7447", ""] {
            assert!(ws_url(address).is_err(), "{address}");
        }
    }

    #[test]
    fn wrong_codes_are_refused_for_a_while() {
        let mut attempts = Attempts::default();
        assert!(!attempts.refused());
        for _ in 0..ATTEMPTS {
            attempts.failure();
        }
        assert!(attempts.refused());
        attempts.success();
        assert!(!attempts.refused());
    }

    fn introduction(id: &str, host: bool) -> Value {
        json!({"id": id, "name": id, "platform": "linux", "publicKey": crypto::public_key(&crypto::generate()), "host": host})
    }

    #[tokio::test]
    async fn two_computers_pair_over_a_socket() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let attempts = Arc::new(Mutex::new(Attempts::default()));

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            pair_in(socket, Some("ABCD2345"), &std::collections::BTreeMap::new(), Some(introduction("host-1", true)), &attempts).await
        });

        let request = format!("ws://{address}/").into_client_request().unwrap();
        let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let (mut viewer, host) = pair_out(socket, "ABCD2345", introduction("viewer-1", false)).await.unwrap();
        assert_eq!((host.id.as_str(), host.host), ("host-1", true));

        let (mut host_side, viewer_row) = server.await.unwrap().unwrap();
        assert_eq!((viewer_row.id.as_str(), viewer_row.host), ("viewer-1", false));

        // Paired is not the same as silent: both directions still carry frames.
        viewer.send(Message::text("ping")).await.unwrap();
        assert_eq!(host_side.next().await.unwrap().unwrap(), Message::text("ping"));
        host_side.send(Message::text("pong")).await.unwrap();
        assert_eq!(viewer.next().await.unwrap().unwrap(), Message::text("pong"));
    }

    #[tokio::test]
    async fn a_wrong_code_is_refused() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let attempts = Arc::new(Mutex::new(Attempts::default()));

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            pair_in(socket, Some("ABCD2345"), &std::collections::BTreeMap::new(), Some(introduction("host-1", true)), &attempts).await
        });

        let request = format!("ws://{address}/").into_client_request().unwrap();
        let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let error = pair_out(socket, "WRONG123", introduction("viewer-1", false)).await.unwrap_err();
        assert!(error.contains("not right"), "{error}");
        assert!(server.await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_trusted_computer_comes_back_without_a_code() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let viewer = introduction("viewer-1", false);
        let key = viewer["publicKey"].as_str().unwrap().to_string();
        let mut trusted = std::collections::BTreeMap::new();
        trusted.insert("viewer-1".to_string(), key);

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            // The host is not showing a code at all: `None` would refuse everyone else.
            pair_in(socket, None, &trusted, Some(introduction("host-1", true)), &Mutex::new(Attempts::default())).await
        });

        let request = format!("ws://{address}/").into_client_request().unwrap();
        let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let (_, host) = pair_out(socket, "", viewer).await.unwrap();
        assert_eq!(host.id, "host-1");
        assert!(server.await.unwrap().is_some());
    }

    #[test]
    fn introductions_need_a_key_and_an_id() {
        let good = json!({"id": "dev-1", "name": "Mac", "platform": "macos", "publicKey": crypto::public_key(&crypto::generate()), "host": true});
        let intro: Introduction = serde_json::from_value(good).unwrap();
        assert_eq!((intro.id.as_str(), intro.host), ("dev-1", true));
        assert!(serde_json::from_value::<Introduction>(json!({"id": "dev-1"})).is_ok());
        assert!(host::valid_id(&intro.id).is_ok());
        assert!(crypto::fingerprint(&intro.public_key).len() == 16);
    }
}
