//! Direct computer-to-computer links: this computer listens for a paired computer, or
//! connects to one. The frames are the relay's, sealed end to end, so the host, the
//! mirrors and the trust pins are unchanged; only the path between them differs.

use super::{config, crypto, host, remote, socket, Device, Inner};
use crate::Result;
use chacha20poly1305::{aead::OsRng, AeadCore, XChaCha20Poly1305};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::Sha256;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::AppHandle;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use x25519_dalek::StaticSecret;

/// The port a direct host listens on when the user has not chosen one.
pub(crate) const DEFAULT_PORT: u16 = 7447;
const CODE: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
/// 12 of 32 symbols is 60 bits: the code is also the secret that authenticates the key
/// exchange, so it must survive an offline guess by whoever records a handshake.
const CODE_LEN: usize = 12;
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

/// A short, unambiguous code: 12 of 32 symbols, so no `0/O` or `1/I` guessing.
fn generate_code() -> String {
    let random = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    (0..CODE_LEN).map(|i| CODE[random[i] as usize % CODE.len()] as char).collect()
}

/// Compares two strings without short-circuiting, so a wrong prefix costs as much as a
/// wrong last symbol.
fn same_text(expected: &str, given: &str) -> bool {
    expected.len() == given.len()
        && expected.bytes().zip(given.bytes()).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
}

/// This computer's half of a pairing: its id and the key that proves it holds the
/// matching private key.
struct Identity<'a> {
    id: &'a str,
    secret: &'a StaticSecret,
}

/// A fresh random nonce, as lowercase hex.
fn nonce() -> String {
    let random = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    random.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// HMAC-SHA256 over the pair key and the whole pairing transcript. Only a party holding
/// both the private key and the code can produce it, so a swapped key or a wrong code
/// fails here rather than at the first frame. An empty code is a trusted computer coming
/// back, where the key itself is the proof.
fn confirmation(key: &crypto::Key, role: &str, code: &str, viewer: &str, host: &str, viewer_nonce: &str, host_nonce: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("a 32-byte key");
    for part in [role, code, viewer, host, viewer_nonce, host_nonce] {
        mac.update(part.as_bytes());
        mac.update(&[0u8]);
    }
    mac.finalize().into_bytes().iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Which kind of computer an address reaches. The pairing is identical; only the wire
/// differs: your network is a plain socket, Tailscale rides WireGuard, and anything else
/// needs TLS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostKind {
    /// Loopback, private or link-local, plus `.local` names: a plain socket is fine.
    Local,
    /// A tailnet address (CGNAT ip or `.ts.net` name). WireGuard carries it.
    Tailscale,
    /// Anything else: acceptable only behind TLS.
    Public,
}

fn host_kind(name: &str) -> HostKind {
    let lower = name.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".local") {
        return HostKind::Local;
    }
    if lower == "ts.net" || lower.ends_with(".ts.net") {
        return HostKind::Tailscale;
    }
    match name.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => {
            let [a, b, _, _] = v4.octets();
            if a == 100 && (64..=127).contains(&b) {
                HostKind::Tailscale
            } else if a == 10 || a == 127 || (a == 172 && (16..32).contains(&b)) || (a == 192 && b == 168) || (a == 169 && b == 254) {
                HostKind::Local
            } else {
                HostKind::Public
            }
        }
        Ok(IpAddr::V6(v6)) => {
            let segments = v6.segments();
            if v6.is_loopback() || (segments[0] & 0xfe00) == 0xfc00 || (segments[0] & 0xffc0) == 0xfe80 {
                HostKind::Local
            } else {
                HostKind::Public
            }
        }
        Err(_) => HostKind::Public,
    }
}

fn parse_port(text: &str) -> Result<u16> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("Enter the computer's address as host:port.".into());
    }
    let port = text.parse::<u16>().map_err(|_| "That port is not usable.".to_string())?;
    if port == 0 {
        return Err("That port is not usable.".into());
    }
    Ok(port)
}

/// The WebSocket URL for a computer address: `host:port` on your network or tailnet, or
/// `wss://host[:port]` for a self-hosted endpoint. A public host is refused on a plain
/// socket and accepted behind TLS.
fn connect_url(address: &str) -> Result<String> {
    let address = address.trim();
    if address.is_empty() || address.len() > MAX_ADDRESS || address.contains(char::is_whitespace) {
        return Err("That address is not usable.".into());
    }
    // An optional scheme; the bare form is plain, and only `wss://` turns on TLS. The prefix
    // is ASCII, so slicing the original at its length keeps the host's own case.
    let lowered = address.to_ascii_lowercase();
    let (scheme, rest): (&str, &str) = if lowered.starts_with("wss://") {
        ("wss", &address[6..])
    } else if lowered.starts_with("ws://") {
        ("ws", &address[5..])
    } else {
        ("ws", address)
    };
    let (given, port) = match rest.rsplit_once(':') {
        Some((given, port)) => (given, Some(port)),
        None => (rest, None),
    };
    let given = given.trim_start_matches('[').trim_end_matches(']').trim();
    if given.is_empty() {
        return Err("Enter the computer's address as host:port.".into());
    }
    let port = port.map(parse_port).transpose()?.unwrap_or(if scheme == "wss" { 443 } else { DEFAULT_PORT });
    if scheme == "ws" && host_kind(given) == HostKind::Public {
        return Err("That address is public, so it needs TLS: enter it as wss://host:port, or use a Tailscale address.".into());
    }
    Ok(format!("{scheme}://{given}:{port}/"))
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

/// Connects this computer to a host on this network, over Tailscale, or through a
/// self-hosted `wss://` endpoint, and pairs with its code.
pub(crate) async fn connect(app: AppHandle, address: String, code: String) -> Result<Value> {
    let url = connect_url(&address)?;
    let request = url.into_client_request().map_err(|_| "That address is not usable.")?;
    let (socket, _) = tokio::time::timeout(TIMEOUT, tokio_tungstenite::connect_async(request))
        .await
        .map_err(|_| "That computer did not answer in time.")?
        .map_err(|_| "Could not reach that computer. Check the address and that it is listening.")?;
    let state = remote(&app);
    let (socket, host) = pair_join(&state, socket, address, &code).await?;
    super::emit_presence(&app);
    tauri::async_runtime::spawn(linked(app.clone(), socket, host.id.clone()));
    tauri::async_runtime::spawn(super::mirror::subscribe_all(app.clone()));
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
    let state = remote(&app);
    let expected = {
        let inner = state.lock();
        inner.pairing.as_ref().filter(|pairing| pairing.expires > Instant::now()).map(|pairing| pairing.code.clone())
    };
    let Some((socket, introduction)) = pair_host(&state, socket, expected, &attempts).await else { return };
    if state.lock().direct_generation != generation {
        return;
    }
    let id = introduction.id.clone();
    super::emit_presence(&app);
    linked(app.clone(), socket, id).await;
}

/// The accepting half of a pairing: read the joining computer, prove the code, pin it
/// and hand back the open socket. No window is needed, so a test can drive two of these.
async fn pair_host<S>(remote: &super::Remote, socket: S, expected: Option<String>, attempts: &Mutex<Attempts>) -> Option<(S, Introduction)>
where
    S: Stream<Item = std::result::Result<Message, WsError>> + Sink<Message, Error = WsError> + Unpin,
{
    let (id, secret) = {
        let inner = remote.lock();
        (inner.settings.device_id.clone(), remote.secret().ok()?)
    };
    let trusted = remote.lock().settings.trusted.clone();
    let identity = Identity { id: &id, secret: &secret };
    let reply = introduction_value(remote, true).ok();
    let (socket, introduction) = pair_in(socket, &identity, expected.as_deref(), &trusted, reply, attempts).await?;
    register_host(remote, introduction.clone());
    Some((socket, introduction))
}

/// The joining half of a pairing: prove the code to the host, pin it and hand back the
/// open socket. No window is needed, so a test can drive two of these.
async fn pair_join<S>(remote: &super::Remote, socket: S, address: String, code: &str) -> Result<(S, Introduction)>
where
    S: Stream<Item = std::result::Result<Message, WsError>> + Sink<Message, Error = WsError> + Unpin,
{
    let (id, secret) = {
        let inner = remote.lock();
        (inner.settings.device_id.clone(), remote.secret()?)
    };
    let identity = Identity { id: &id, secret: &secret };
    let mine = introduction_value(remote, false)?;
    let (socket, joined) = pair_out(socket, &identity, code, mine).await?;
    Ok((socket, register(remote, joined, address)))
}

/// Reads one text frame, or None on a timeout, a close, or anything but text.
async fn read_text<S>(stream: &mut S) -> Option<String>
where
    S: Stream<Item = std::result::Result<Message, WsError>> + Unpin,
{
    let framed = tokio::time::timeout(TIMEOUT, stream.next()).await.ok()?.and_then(|framed| framed.ok());
    match framed {
        Some(Message::Text(text)) => Some(text.to_string()),
        _ => None,
    }
}

/// Reads the joining computer's frame and answers with this computer's own row. The code
/// never crosses the socket: it is folded into an HMAC over the exchanged key, so a wrong
/// code or a swapped key fails the proof instead of merely gating a request. A computer
/// already trusted here, with the same key, proves itself with the key alone.
async fn pair_in<S>(
    socket: S,
    identity: &Identity<'_>,
    expected: Option<&str>,
    trusted: &std::collections::BTreeMap<String, String>,
    reply: Option<Value>,
    attempts: &Mutex<Attempts>,
) -> Option<(S, Introduction)>
where
    S: Stream<Item = std::result::Result<Message, WsError>> + Sink<Message, Error = WsError> + Unpin,
{
    let (mut sink, mut stream) = socket.split();
    let text = read_text(&mut stream).await?;
    let value: Value = serde_json::from_str(&text).ok()?;
    if value["type"].as_str() != Some("pair") {
        return None;
    }
    let introduction: Introduction = serde_json::from_value(value["device"].clone()).ok()?;
    if host::valid_id(&introduction.id).is_err() || crypto::fingerprint(&introduction.public_key).is_empty() {
        return None;
    }
    let viewer_nonce = match value["nonce"].as_str() {
        Some(nonce) if (32..=128).contains(&nonce.len()) => nonce,
        _ => return None,
    };
    // A trusted computer comes back on its key alone; anyone else needs the code on screen.
    let known = trusted.get(&introduction.id).is_some_and(|pinned| pinned == &introduction.public_key);
    let code = if known { "" } else { expected? };
    let refused = {
        let locked = attempts.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        locked.refused()
    };
    if refused {
        let _ = sink.send(Message::text(json!({"type": "pair-fail", "error": "Too many wrong codes. Try again in a minute."}).to_string())).await;
        return None;
    }
    let key = crypto::pair_key(identity.secret, identity.id, &introduction.public_key, &introduction.id).ok()?;
    let host_nonce = nonce();
    let proof = confirmation(&key, "host", code, &introduction.id, identity.id, viewer_nonce, &host_nonce);
    let reply = reply?;
    sink.send(Message::text(json!({"type": "pair-ok", "device": reply, "nonce": host_nonce, "confirm": proof}).to_string())).await.ok()?;
    // The joining computer answers with its own proof, which only its private key and the
    // code can produce. This is where a wrong code or a swapped key is caught, and where a
    // wrong code is counted against the listener.
    let text = read_text(&mut stream).await?;
    let value: Value = serde_json::from_str(&text).ok()?;
    if value["type"].as_str() != Some("pair-confirm") {
        return None;
    }
    let want = confirmation(&key, "viewer", code, &introduction.id, identity.id, viewer_nonce, &host_nonce);
    if !same_text(&want, value["confirm"].as_str().unwrap_or_default()) {
        attempts.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).failure();
        let _ = sink.send(Message::text(json!({"type": "pair-fail", "error": "That code is not right, or that computer is not who it says it is."}).to_string())).await;
        return None;
    }
    attempts.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).success();
    let _ = sink.send(Message::text(json!({"type": "pair-done"}).to_string())).await;
    Some((sink.reunite(stream).ok()?, introduction))
}

/// Sends this computer's pairing frame and reads the host's row, refusing to trust it
/// until its proof checks out. The code is never sent: it only feeds the proof.
async fn pair_out<S>(socket: S, identity: &Identity<'_>, code: &str, introduction: Value) -> Result<(S, Introduction)>
where
    S: Stream<Item = std::result::Result<Message, WsError>> + Sink<Message, Error = WsError> + Unpin,
{
    let (mut sink, mut stream) = socket.split();
    let viewer_nonce = nonce();
    let frame = json!({"type": "pair", "nonce": viewer_nonce, "device": introduction});
    sink.send(Message::text(frame.to_string())).await.map_err(|_| "Could not reach that computer.")?;
    let text = read_text(&mut stream).await.ok_or("That computer closed the connection.")?;
    let value: Value = serde_json::from_str(&text).map_err(|_| "That computer sent an unreadable reply.".to_string())?;
    let introduction: Introduction = match value["type"].as_str() {
        Some("pair-ok") => serde_json::from_value(value["device"].clone()).map_err(|_| "That computer sent an unreadable reply.".to_string())?,
        Some("pair-fail") => return Err(value["error"].as_str().unwrap_or("That code is not right.").chars().take(200).collect()),
        _ => return Err("That computer did not answer the pairing request.".into()),
    };
    if host::valid_id(&introduction.id).is_err() || crypto::fingerprint(&introduction.public_key).is_empty() {
        return Err("That computer sent an unusable identity.".into());
    }
    let host_nonce = value["nonce"].as_str().unwrap_or_default();
    let key = crypto::pair_key(identity.secret, identity.id, &introduction.public_key, &introduction.id)?;
    let host_proof = confirmation(&key, "host", code, identity.id, &introduction.id, &viewer_nonce, host_nonce);
    let accepted = same_text(&host_proof, value["confirm"].as_str().unwrap_or_default());
    // Sent either way: the host has to see a wrong code to count it against the listener.
    let proof = confirmation(&key, "viewer", code, identity.id, &introduction.id, &viewer_nonce, host_nonce);
    sink.send(Message::text(json!({"type": "pair-confirm", "confirm": proof}).to_string())).await.map_err(|_| "Could not reach that computer.")?;
    let verdict = read_text(&mut stream).await.ok_or("That computer closed the connection.")?;
    if !accepted {
        return Err("That code is not right, or that computer is not who it says it is.".into());
    }
    let value: Value = serde_json::from_str(&verdict).map_err(|_| "That computer sent an unreadable reply.".to_string())?;
    if value["type"].as_str() != Some("pair-done") {
        return Err("That computer did not finish pairing.".into());
    }
    Ok((sink.reunite(stream).map_err(|_| "That computer closed the connection.")?, introduction))
}

/// This computer's row, as it introduces itself. `host` is whether remote control is on.
fn introduction_value(remote: &super::Remote, host: bool) -> Result<Value> {
    let (id, public_key) = {
        let inner = remote.lock();
        (inner.settings.device_id.clone(), crypto::public_key(&remote.secret()?))
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
fn register_host(remote: &super::Remote, introduction: Introduction) {
    let id = introduction.id.clone();
    let settings = {
        let mut inner = remote.lock();
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
    remote.save(&settings);
}

/// Pins the host this computer paired with, remembers its address and lists it.
fn register(remote: &super::Remote, introduction: Introduction, address: String) -> Introduction {
    let id = introduction.id.clone();
    let settings = {
        let mut inner = remote.lock();
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
    remote.save(&settings);
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
    use crate::remote::{secrets, Remote};

    /// The code the host shows and the joining computer types.
    const PAIR_CODE: &str = "ABCD23456789";

    fn identity<'a>(id: &'a str, secret: &'a StaticSecret) -> Identity<'a> {
        Identity { id, secret }
    }

    fn test_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("pantheon-direct-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

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
    fn text_compares_without_short_circuiting() {
        assert!(same_text(PAIR_CODE, PAIR_CODE));
        assert!(!same_text(PAIR_CODE, "ABCD2345678"));
        assert!(!same_text(PAIR_CODE, "ABCD23456788"));
        assert!(same_text("", ""));
    }

    /// The proof covers the key, the code and both nonces, so a swapped key or a guessed
    /// code stops the handshake instead of merely gating a request.
    #[test]
    fn the_proof_binds_the_key_the_code_and_the_nonces() {
        let viewer = crypto::generate();
        let host = crypto::generate();
        let pair = crypto::pair_key(&viewer, "viewer", &crypto::public_key(&host), "host").unwrap();
        let base = confirmation(&pair, "host", PAIR_CODE, "viewer", "host", "n1", "n2");
        assert_eq!(base, confirmation(&pair, "host", PAIR_CODE, "viewer", "host", "n1", "n2"));
        assert_ne!(base, confirmation(&pair, "host", "ABCD2345678", "viewer", "host", "n1", "n2"), "a wrong code");
        assert_ne!(base, confirmation(&pair, "host", PAIR_CODE, "viewer", "host", "n1", "n3"), "a replayed nonce");
        assert_ne!(base, confirmation(&pair, "viewer", PAIR_CODE, "viewer", "host", "n1", "n2"), "the other role");
        let other = crypto::pair_key(&viewer, "viewer", &crypto::public_key(&crypto::generate()), "host").unwrap();
        assert_ne!(base, confirmation(&other, "host", PAIR_CODE, "viewer", "host", "n1", "n2"), "another key");
    }

    #[test]
    fn addresses_get_the_right_scheme() {
        // Plain: your network, with or without a port.
        assert_eq!(connect_url("192.168.1.5:7447").unwrap(), "ws://192.168.1.5:7447/");
        assert_eq!(connect_url("10.0.0.2").unwrap(), "ws://10.0.0.2:7447/");
        assert_eq!(connect_url("127.0.0.1:1").unwrap(), "ws://127.0.0.1:1/");
        assert_eq!(connect_url("studio.local:7447").unwrap(), "ws://studio.local:7447/");
        assert_eq!(connect_url("[::1]:7447").unwrap(), "ws://::1:7447/");
        assert_eq!(connect_url("ws://192.168.1.5:7447").unwrap(), "ws://192.168.1.5:7447/");
        // Tailscale rides WireGuard, so it stays plain.
        assert_eq!(connect_url("100.126.202.84").unwrap(), "ws://100.126.202.84:7447/");
        assert_eq!(connect_url("100.64.1.2:8443").unwrap(), "ws://100.64.1.2:8443/");
        assert_eq!(connect_url("studio.ts.net:7447").unwrap(), "ws://studio.ts.net:7447/");
        // A public host is accepted behind TLS.
        assert_eq!(connect_url("wss://connect.example.com").unwrap(), "wss://connect.example.com:443/");
        assert_eq!(connect_url("wss://connect.example.com:8443").unwrap(), "wss://connect.example.com:8443/");
        assert_eq!(connect_url("wss://8.8.8.8").unwrap(), "wss://8.8.8.8:443/");
        assert_eq!(connect_url("WSS://Host:8443").unwrap(), "wss://Host:8443/");
        // But a public host on a plain socket is refused, as are malformed addresses.
        for address in ["8.8.8.8:7447", "example.com:7447", "ws://8.8.8.8:7447", "192.168.1.5:0", "192.168.1.5:x", ":7447", ""] {
            assert!(connect_url(address).is_err(), "{address}");
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

    fn introduction(id: &str, host: bool, public_key: &str) -> Value {
        json!({"id": id, "name": id, "platform": "linux", "publicKey": public_key, "host": host})
    }

    #[tokio::test]
    async fn two_computers_pair_over_a_socket() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let attempts = Arc::new(Mutex::new(Attempts::default()));
        let host_secret = crypto::generate();
        let viewer_secret = crypto::generate();
        let host_public = crypto::public_key(&host_secret);

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let host = identity("host-1", &host_secret);
            let trusted = std::collections::BTreeMap::new();
            pair_in(socket, &host, Some(PAIR_CODE), &trusted, Some(introduction("host-1", true, &host_public)), &attempts).await
        });

        let request = format!("ws://{address}/").into_client_request().unwrap();
        let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let viewer = identity("viewer-1", &viewer_secret);
        let (mut client, host_row) = pair_out(socket, &viewer, PAIR_CODE, introduction("viewer-1", false, &crypto::public_key(&viewer_secret))).await.unwrap();
        assert_eq!((host_row.id.as_str(), host_row.host), ("host-1", true));

        let (mut host_side, viewer_row) = server.await.unwrap().unwrap();
        assert_eq!((viewer_row.id.as_str(), viewer_row.host), ("viewer-1", false));

        // Paired is not the same as silent: both directions still carry frames.
        client.send(Message::text("ping")).await.unwrap();
        assert_eq!(host_side.next().await.unwrap().unwrap(), Message::text("ping"));
        host_side.send(Message::text("pong")).await.unwrap();
        assert_eq!(client.next().await.unwrap().unwrap(), Message::text("pong"));
    }

    #[tokio::test]
    async fn a_wrong_code_is_refused() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let attempts = Arc::new(Mutex::new(Attempts::default()));
        let shared = attempts.clone();
        let host_secret = crypto::generate();
        let viewer_secret = crypto::generate();
        let host_public = crypto::public_key(&host_secret);

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let host = identity("host-1", &host_secret);
            let trusted = std::collections::BTreeMap::new();
            pair_in(socket, &host, Some(PAIR_CODE), &trusted, Some(introduction("host-1", true, &host_public)), &shared).await
        });

        let request = format!("ws://{address}/").into_client_request().unwrap();
        let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let viewer = identity("viewer-1", &viewer_secret);
        let error = pair_out(socket, &viewer, "WRONG1234567", introduction("viewer-1", false, &crypto::public_key(&viewer_secret))).await.unwrap_err();
        assert!(error.contains("not right"), "{error}");
        assert!(server.await.unwrap().is_none());
        assert_eq!(attempts.lock().unwrap().failed, 1, "one wrong code is one failure");
    }

    #[tokio::test]
    async fn a_trusted_computer_comes_back_without_a_code() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let viewer_secret = crypto::generate();
        let viewer_public = crypto::public_key(&viewer_secret);
        let mut trusted = std::collections::BTreeMap::new();
        trusted.insert("viewer-1".to_string(), viewer_public.clone());
        let host_secret = crypto::generate();
        let host_public = crypto::public_key(&host_secret);

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let host = identity("host-1", &host_secret);
            // No code on screen at all: only the pinned key gets in.
            pair_in(socket, &host, None, &trusted, Some(introduction("host-1", true, &host_public)), &Mutex::new(Attempts::default())).await
        });

        let request = format!("ws://{address}/").into_client_request().unwrap();
        let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let viewer = identity("viewer-1", &viewer_secret);
        let (_, host_row) = pair_out(socket, &viewer, "", introduction("viewer-1", false, &viewer_public)).await.unwrap();
        assert_eq!(host_row.id, "host-1");
        assert!(server.await.unwrap().is_some());
    }

    /// Two independent computers, each with its own state, key and device id, pair over a
    /// real socket and pin one another. This is the shape a LAN pair takes, without a
    /// window in the middle.
    #[tokio::test]
    async fn two_computers_pair_and_pin_one_another() {
        let dir_a = test_dir("host");
        let dir_b = test_dir("viewer");
        let a = Remote::new(&dir_a);
        let b = Remote::new(&dir_b);
        let secret_a = crypto::generate();
        let secret_b = crypto::generate();
        secrets::set_key(&dir_a, &crypto::encode_secret(&secret_a)).unwrap();
        secrets::set_key(&dir_b, &crypto::encode_secret(&secret_b)).unwrap();
        let id_a = a.lock().settings.device_id.clone();
        let id_b = b.lock().settings.device_id.clone();
        assert_ne!(id_a, id_b, "two computers must not share a device id");

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let attempts = Arc::new(Mutex::new(Attempts::default()));
        let host = async {
            let (stream, _) = listener.accept().await.unwrap();
            let socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            pair_host(&a, socket, Some(PAIR_CODE.to_string()), &attempts).await
        };
        let join = async {
            let request = format!("ws://{address}/").into_client_request().unwrap();
            let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
            pair_join(&b, socket, format!("{address}"), PAIR_CODE).await
        };
        let (host_result, join_result) = futures_util::join!(host, join);

        let (_host_socket, viewer_row) = host_result.unwrap();
        let (_join_socket, host_row) = join_result.unwrap();
        assert_eq!((host_row.id.as_str(), viewer_row.id.as_str()), (id_a.as_str(), id_b.as_str()));
        // Each side pinned the other's key, so the trust outlives this socket.
        assert!(a.lock().settings.is_trusted(&id_b, &crypto::public_key(&secret_b)), "the host pinned the viewer");
        assert!(b.lock().settings.is_trusted(&id_a, &crypto::public_key(&secret_a)), "the viewer pinned the host");
        let saved_a = super::config::load(&dir_a);
        let saved_b = super::config::load(&dir_b);
        assert!(saved_a.is_trusted(&id_b, &crypto::public_key(&secret_b)), "the host's pin is on disk");
        assert!(saved_b.is_trusted(&id_a, &crypto::public_key(&secret_a)), "the viewer's pin is on disk");
        let _ = std::fs::remove_dir_all(dir_a);
        let _ = std::fs::remove_dir_all(dir_b);
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

    /// One side of a pairing, run as its own process. `two_networks_pair` drives two of
    /// these in separate network namespaces, which is the shape of a real two-computer
    /// link.
    #[tokio::test]
    async fn pair_side() {
        let side = std::env::var("PANTHEON_PAIR_SIDE").unwrap_or_default();
        if side.is_empty() {
            return;
        }
        let dir = std::env::var("PANTHEON_PAIR_DIR").expect("PANTHEON_PAIR_DIR");
        let code = std::env::var("PANTHEON_PAIR_CODE").expect("PANTHEON_PAIR_CODE");
        let address = std::env::var("PANTHEON_PAIR_ADDRESS").expect("PANTHEON_PAIR_ADDRESS");
        let port: u16 = std::env::var("PANTHEON_PAIR_PORT").expect("PANTHEON_PAIR_PORT").parse().unwrap();
        let path = std::path::Path::new(&dir);
        let remote = Remote::new(path);
        let secret = crypto::generate();
        secrets::set_key(path, &crypto::encode_secret(&secret)).unwrap();
        let attempts = Arc::new(Mutex::new(Attempts::default()));
        let paired = if side == "host" {
            let listener = TcpListener::bind(("0.0.0.0", port)).await.unwrap();
            let (stream, _) = listener.accept().await.unwrap();
            let socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            pair_host(&remote, socket, Some(code), &attempts).await.is_some()
        } else {
            // Through the same address parser the app uses, so a tailnet or TLS address is
            // classified here rather than assumed.
            let request = connect_url(&format!("{address}:{port}")).unwrap().into_client_request().unwrap();
            let (socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
            pair_join(&remote, socket, format!("{address}:{port}"), &code).await.is_ok()
        };
        let id = remote.lock().settings.device_id.clone();
        let pinned = remote.lock().settings.trusted.keys().cloned().collect::<Vec<String>>();
        let result = json!({
            "side": side,
            "paired": paired,
            "id": id,
            "pinned": pinned,
            "failed": attempts.lock().unwrap().failed,
        });
        std::fs::write(format!("{dir}.result"), result.to_string()).unwrap();
    }

    /// Two computers, two network namespaces joined by a veth pair, pairing over that
    /// link with a code. Host and viewer are separate processes, so this is the same path
    /// a real pair takes. Needs root and the namespaces already up; `cargo test -- --ignored`.
    async fn two_networks(host_address: &str, port: u16) {
        let exe = std::env::current_exe().unwrap();
        let side = |side: &str, namespace: &str, dir: &std::path::Path, code: &str| {
            let mut command = std::process::Command::new("sudo");
            // `sudo` drops the environment, so the sides are passed as `env` arguments
            // rather than as this process's own variables.
            command.args(["ip", "netns", "exec", namespace, "env"]);
            command.arg(format!("PANTHEON_PAIR_SIDE={side}"));
            command.arg(format!("PANTHEON_PAIR_DIR={}", dir.display()));
            command.arg(format!("PANTHEON_PAIR_CODE={code}"));
            command.arg(format!("PANTHEON_PAIR_ADDRESS={host_address}"));
            command.arg(format!("PANTHEON_PAIR_PORT={port}"));
            command.arg(&exe);
            command.args(["--exact", "remote::direct::tests::pair_side", "--nocapture"]);
            command
        };
        let read = |dir: &std::path::Path| -> Value {
            let text = std::fs::read_to_string(format!("{}.result", dir.display())).unwrap();
            serde_json::from_str(&text).unwrap()
        };
        for (label, code) in [("a right code", PAIR_CODE), ("a wrong code", "WRONG1234567")] {
            let host_dir = test_dir(&format!("net-host-{label}"));
            let viewer_dir = test_dir(&format!("net-viewer-{label}"));
            let host = side("host", "pantheon-a", &host_dir, PAIR_CODE).spawn().expect("spawn the host");
            std::thread::sleep(std::time::Duration::from_millis(400));
            let viewer = side("join", "pantheon-b", &viewer_dir, code).output().expect("run the viewer");
            assert!(viewer.status.success(), "{label}: viewer: {}{}", String::from_utf8_lossy(&viewer.stdout), String::from_utf8_lossy(&viewer.stderr));
            let host = host.wait_with_output().expect("wait for the host");
            assert!(host.status.success(), "{label}: host: {}{}", String::from_utf8_lossy(&host.stdout), String::from_utf8_lossy(&host.stderr));
            let host = read(&host_dir);
            let viewer = read(&viewer_dir);
            if code == PAIR_CODE {
                assert_eq!(host["paired"], json!(true), "{label}: the host paired");
                assert_eq!(viewer["paired"], json!(true), "{label}: the viewer paired");
                assert!(host["pinned"].as_array().unwrap().contains(&viewer["id"]), "{label}: the host pinned the viewer");
                assert!(viewer["pinned"].as_array().unwrap().contains(&host["id"]), "{label}: the viewer pinned the host");
            } else {
                assert_eq!(host["paired"], json!(false), "{label}: a wrong code is refused");
                assert_eq!(viewer["paired"], json!(false), "{label}: the viewer is not paired");
                assert_eq!(host["failed"], json!(1), "{label}: the host counted one wrong code");
            }
            let _ = std::fs::remove_dir_all(&host_dir);
            let _ = std::fs::remove_dir_all(&viewer_dir);
        }
    }

    #[tokio::test]
    #[ignore = "needs two network namespaces joined by a veth pair"]
    async fn two_networks_pair() {
        two_networks("10.99.0.1", 7447).await;
    }

    /// The same two computers on a CGNAT (`100.64.0.0/10`) link, which is the address range
    /// Tailscale hands out. The address still has to classify as tailnet, not as a public
    /// host that needs TLS. A different port keeps the two tests from fighting for the one
    /// the namespaces share.
    #[tokio::test]
    #[ignore = "needs two network namespaces joined by a veth pair"]
    async fn two_networks_pair_over_tailnet() {
        two_networks("100.64.0.1", 7448).await;
    }
}
