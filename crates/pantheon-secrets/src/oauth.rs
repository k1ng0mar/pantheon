//! Local-first OAuth2 connectors (GitHub, Google, Notion, Spotify).
//!
//! Unlike hosted connector designs, tokens live in the local durable
//! vaults (OS keychain, else the encrypted file vault): there is no
//! server side. The model never sees token bytes.
//! [`SecretsBroker::oauth_request`] resolves a valid token, performs the
//! authenticated request, and returns only the response body. Every
//! handed-out access token is registered with the exact-value redaction
//! registry at handout (fill-time registration), so a later echo in any
//! tool result is scrubbed before it reaches the model.
//!
//! Client IDs come from the environment (`PANTHEON_OAUTH_<APP>_CLIENT_ID`);
//! client secrets are never required. All four providers run the RFC 8252
//! public-client loopback flow with PKCE S256 (RFC 7636). Notion
//! additionally honors `PANTHEON_OAUTH_NOTION_CLIENT_SECRET`: when set,
//! the token exchange uses HTTP Basic (`client_id:client_secret`); when
//! absent, the PKCE public-client flow is used.

use crate::error::{validate_name, SecretsError};
use crate::value::SecretValue;
use crate::SecretsBroker;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How the token endpoint authenticates this client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenAuth {
    /// Public client: PKCE only, no secret (GitHub, Google, Spotify).
    Pkce,
    /// Optional HTTP Basic `client_id:client_secret` (Notion): used when
    /// `PANTHEON_OAUTH_NOTION_CLIENT_SECRET` is set, PKCE otherwise. A
    /// Notion integration configured as a public client works without it;
    /// one configured confidential needs it.
    BasicOptional,
}

/// Static OAuth2 provider definition. Pure data: no secrets here.
#[derive(Debug, Clone, Copy)]
pub struct OAuthProvider {
    /// Stable id: `github`, `google`, `notion`, `spotify`.
    pub id: &'static str,
    pub display: &'static str,
    pub auth_url: &'static str,
    pub token_url: &'static str,
    /// API base the broker-side allowlist enforces (`oauth_request`
    /// refuses any URL outside it).
    pub api_base: &'static str,
    pub default_scopes: &'static [&'static str],
    /// Extra query params on the authorization URL (Google needs
    /// `access_type=offline` for a refresh token to be issued).
    pub extra_auth_params: &'static [(&'static str, &'static str)],
    /// Extra headers on broker-side API requests (Notion's version pin).
    pub extra_headers: &'static [(&'static str, &'static str)],
    pub token_auth: TokenAuth,
    /// Env var carrying the OAuth client ID (public identifier, not a
    /// secret - but read from env so installs never bake one in).
    pub client_id_env: &'static str,
}

pub const GITHUB: OAuthProvider = OAuthProvider {
    id: "github",
    display: "GitHub",
    auth_url: "https://github.com/login/oauth/authorize",
    token_url: "https://github.com/login/oauth/access_token",
    api_base: "https://api.github.com",
    default_scopes: &["read:user", "repo"],
    extra_auth_params: &[],
    extra_headers: &[
        ("Accept", "application/vnd.github+json"),
        ("X-GitHub-Api-Version", "2022-11-28"),
    ],
    token_auth: TokenAuth::Pkce,
    client_id_env: "PANTHEON_OAUTH_GITHUB_CLIENT_ID",
};

pub const GOOGLE: OAuthProvider = OAuthProvider {
    id: "google",
    display: "Google",
    auth_url: "https://accounts.google.com/o/oauth2/v2/auth",
    token_url: "https://oauth2.googleapis.com/token",
    api_base: "https://www.googleapis.com",
    default_scopes: &["openid", "email", "profile"],
    extra_auth_params: &[("access_type", "offline"), ("prompt", "consent")],
    extra_headers: &[],
    token_auth: TokenAuth::Pkce,
    client_id_env: "PANTHEON_OAUTH_GOOGLE_CLIENT_ID",
};

pub const NOTION: OAuthProvider = OAuthProvider {
    id: "notion",
    display: "Notion",
    auth_url: "https://api.notion.com/v1/oauth/authorize",
    token_url: "https://api.notion.com/v1/oauth/token",
    api_base: "https://api.notion.com/v1",
    // Notion grants capabilities at the integration level, not via scopes.
    default_scopes: &[],
    extra_auth_params: &[],
    extra_headers: &[("Notion-Version", "2022-06-28")],
    token_auth: TokenAuth::BasicOptional,
    client_id_env: "PANTHEON_OAUTH_NOTION_CLIENT_ID",
};

pub const SPOTIFY: OAuthProvider = OAuthProvider {
    id: "spotify",
    display: "Spotify",
    auth_url: "https://accounts.spotify.com/authorize",
    token_url: "https://accounts.spotify.com/api/token",
    api_base: "https://api.spotify.com/v1",
    default_scopes: &[
        "user-read-playback-state",
        "user-modify-playback-state",
        "user-read-currently-playing",
    ],
    extra_auth_params: &[],
    extra_headers: &[],
    token_auth: TokenAuth::Pkce,
    client_id_env: "PANTHEON_OAUTH_SPOTIFY_CLIENT_ID",
};

/// All supported providers, in CLI/tool display order.
pub const PROVIDERS: &[OAuthProvider] = &[GITHUB, GOOGLE, NOTION, SPOTIFY];

/// Look up a provider by id (`github`, `google`, `notion`, `spotify`).
pub fn provider(id: &str) -> Option<OAuthProvider> {
    PROVIDERS.iter().find(|p| p.id == id).copied()
}

/// A stored OAuth2 token set. Secret material travels only as
/// [`SecretValue`]; the struct deliberately has no Serialize impl.
#[derive(Debug, Clone)]
pub struct OAuthToken {
    pub access_token: SecretValue,
    pub refresh_token: Option<SecretValue>,
    pub expires_at: Option<SystemTime>,
    pub scopes: Vec<String>,
    pub token_type: String,
}

impl OAuthToken {
    /// True when expired or expiring within the refresh skew window.
    /// `None` expiry means non-expiring (e.g. classic GitHub OAuth App
    /// tokens): never refreshed.
    fn expired_with_skew(&self) -> bool {
        match self.expires_at {
            Some(t) => SystemTime::now() + Duration::from_secs(60) >= t,
            None => false,
        }
    }
}

/// Non-secret token metadata, stored as single-line JSON beside the
/// secret records.
#[derive(serde::Serialize, serde::Deserialize)]
struct TokenMeta {
    expires_at: Option<u64>,
    scopes: Vec<String>,
    token_type: String,
}

/// Connection status: presence, never token material.
#[derive(Debug, Clone)]
pub struct OAuthStatus {
    pub connected: bool,
    pub scopes: Vec<String>,
    pub expires_at: Option<SystemTime>,
}

/// Per-app token records persisted through the broker's durable vaults.
///
/// Names are `oauth_<app>_<field>` (underscores only: the dotenv vault
/// rejects anything outside `[A-Za-z0-9_]`).
pub struct OAuthStore<'a> {
    broker: &'a SecretsBroker,
}

impl<'a> OAuthStore<'a> {
    pub fn new(broker: &'a SecretsBroker) -> Self {
        Self { broker }
    }

    fn names(app: &str) -> (String, String, String) {
        (
            format!("oauth_{app}_access_token"),
            format!("oauth_{app}_refresh_token"),
            format!("oauth_{app}_meta"),
        )
    }

    /// Persist a token set. Overwrites any previous records for the app.
    pub fn save(&self, app: &str, token: &OAuthToken) -> Result<(), SecretsError> {
        provider(app).ok_or_else(|| SecretsError::Invalid(format!("unknown oauth app {app:?}")))?;
        let (access_name, refresh_name, meta_name) = Self::names(app);
        self.broker.set(&access_name, token.access_token.clone())?;
        match &token.refresh_token {
            Some(rt) => self.broker.set(&refresh_name, rt.clone())?,
            None => self.broker.delete(&refresh_name)?,
        }
        let meta = TokenMeta {
            expires_at: token
                .expires_at
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
            scopes: token.scopes.clone(),
            token_type: token.token_type.clone(),
        };
        let json = serde_json::to_string(&meta).map_err(SecretsError::from)?;
        self.broker.set(&meta_name, SecretValue::new(json))?;
        Ok(())
    }

    /// Load a token set, or `None` when the app was never connected.
    pub fn load(&self, app: &str) -> Result<Option<OAuthToken>, SecretsError> {
        provider(app).ok_or_else(|| SecretsError::Invalid(format!("unknown oauth app {app:?}")))?;
        let (access_name, refresh_name, meta_name) = Self::names(app);
        let access_token = match self.broker.resolve(&access_name)? {
            Some(v) => v,
            None => return Ok(None),
        };
        let refresh_token = self.broker.resolve(&refresh_name)?;
        let (expires_at, scopes, token_type) = match self.broker.resolve(&meta_name)? {
            Some(meta) => {
                let parsed: TokenMeta =
                    serde_json::from_str(meta.expose()).map_err(SecretsError::from)?;
                (
                    parsed
                        .expires_at
                        .map(|s| UNIX_EPOCH + Duration::from_secs(s)),
                    parsed.scopes,
                    parsed.token_type,
                )
            }
            None => (None, Vec::new(), "Bearer".to_string()),
        };
        Ok(Some(OAuthToken {
            access_token,
            refresh_token,
            expires_at,
            scopes,
            token_type,
        }))
    }

    /// Delete every record for the app from every vault. Unknown app is an
    /// error; a never-connected app is a no-op (deletes are no-ops per the
    /// vault contract).
    pub fn delete(&self, app: &str) -> Result<(), SecretsError> {
        provider(app).ok_or_else(|| SecretsError::Invalid(format!("unknown oauth app {app:?}")))?;
        let (access_name, refresh_name, meta_name) = Self::names(app);
        self.broker.delete(&access_name)?;
        self.broker.delete(&refresh_name)?;
        self.broker.delete(&meta_name)?;
        Ok(())
    }

    /// Connection status for display. Never carries token material.
    pub fn status(&self, app: &str) -> Result<OAuthStatus, SecretsError> {
        provider(app).ok_or_else(|| SecretsError::Invalid(format!("unknown oauth app {app:?}")))?;
        match self.load(app)? {
            Some(t) => Ok(OAuthStatus {
                connected: true,
                scopes: t.scopes,
                expires_at: t.expires_at,
            }),
            None => Ok(OAuthStatus {
                connected: false,
                scopes: Vec::new(),
                expires_at: None,
            }),
        }
    }

    /// Resolve a usable access token, refreshing when expired or expiring
    /// within 60 seconds. Rotated tokens are persisted.
    ///
    /// The handed-out bytes are registered with the broker's redaction
    /// registry (fill-time registration): the exact bytes are recorded
    /// before they can appear in any tool result.
    ///
    /// Fails closed: unknown app, not connected, and expired-without-a-
    /// refresh-token all name the `pantheon oauth login <app>` remediation.
    pub fn ensure_valid_token(&self, app: &str) -> Result<SecretValue, SecretsError> {
        let prov = provider(app)
            .ok_or_else(|| SecretsError::Invalid(format!("unknown oauth app {app:?}")))?;
        let mut token = self.load(app)?.ok_or_else(|| {
            SecretsError::Invalid(format!(
                "{app} is not connected; run `pantheon oauth login {app}`"
            ))
        })?;
        if token.expired_with_skew() {
            let refresh = token.refresh_token.clone().ok_or_else(|| {
                SecretsError::Invalid(format!(
                    "{app} token expired and no refresh token is stored; run `pantheon oauth login {app}`"
                ))
            })?;
            let client_id = client_id_for(prov)?;
            token = refresh_access_token_with(&http_agent(), prov, &client_id, refresh.expose())?;
            self.save(app, &token)?;
        }
        self.broker
            .redaction_registry()
            .register(token.access_token.expose());
        Ok(token.access_token)
    }
}

/// Read the client ID for a provider from its env var. The client ID is a
/// public identifier, not a secret - but it lives in env so installs never
/// bake one in.
fn client_id_for(prov: OAuthProvider) -> Result<String, SecretsError> {
    std::env::var(prov.client_id_env)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            SecretsError::Invalid(format!(
                "missing {}: export your {} OAuth client ID, e.g. `{}=... pantheon oauth login {}`",
                prov.client_id_env, prov.display, prov.client_id_env, prov.id
            ))
        })
}

/// ureq agent for token and API calls. Honors proxy-from-env (the
/// workspace standard) so sandboxed egress keeps working; tests build
/// their own agent without a proxy and pass it to the `_with` variants.
fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new().try_proxy_from_env(true).build()
}

/// Parsed token endpoint response. Field names follow RFC 6749.
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    scopes: Vec<String>,
    token_type: String,
}

impl TokenResponse {
    fn into_token(self) -> OAuthToken {
        OAuthToken {
            access_token: SecretValue::new(self.access_token),
            refresh_token: self.refresh_token.map(SecretValue::new),
            expires_at: self
                .expires_in
                .map(|s| SystemTime::now() + Duration::from_secs(s)),
            scopes: self.scopes,
            token_type: self.token_type,
        }
    }
}

/// POST a form to the token endpoint and parse the JSON response.
/// `token_url` is a parameter (not taken from the provider) so tests can
/// point it at a loopback fake.
fn post_token(
    agent: &ureq::Agent,
    prov_id: &str,
    token_url: &str,
    token_auth: TokenAuth,
    client_id: &str,
    params: &[(&str, &str)],
) -> Result<TokenResponse, SecretsError> {
    validate_name(prov_id)?;
    let mut req = agent.post(token_url).set("Accept", "application/json");
    if token_auth == TokenAuth::BasicOptional {
        // Notion confidential integrations authenticate the token exchange
        // with HTTP Basic. Public integrations leave the env var unset and
        // fall through to the PKCE public-client exchange below.
        if let Ok(secret) = std::env::var("PANTHEON_OAUTH_NOTION_CLIENT_SECRET") {
            if !secret.trim().is_empty() {
                let creds = base64_encode(format!("{client_id}:{secret}").as_bytes());
                // The secret travels only in this header, over TLS, and is
                // never logged. It is NOT registered for redaction: it is a
                // static credential the model must never be able to trigger
                // the use of, let alone see.
                req = req.set("Authorization", &format!("Basic {creds}"));
            }
        }
    }
    let resp = req.send_form(params).map_err(|e| match e {
        ureq::Error::Status(code, r) => {
            let detail = r.into_string().unwrap_or_default();
            let detail: String = detail.chars().take(300).collect();
            SecretsError::Backend(format!("{prov_id} token endpoint error {code}: {detail}"))
        }
        other => SecretsError::Backend(format!("{prov_id} token request failed: {other}")),
    })?;
    let body: serde_json::Value = resp.into_json().map_err(|e| {
        SecretsError::Backend(format!("{prov_id} token endpoint returned non-JSON: {e}"))
    })?;
    if let Some(err) = body.get("error").and_then(|v| v.as_str()) {
        let desc = body
            .get("error_description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        return Err(SecretsError::Backend(format!(
            "{prov_id} token request failed: {err} {desc}"
        )));
    }
    let access_token = body
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            SecretsError::Backend(format!("{prov_id} token response missing access_token"))
        })?;
    // GitHub separates scopes with commas, Google with spaces: split both.
    let scopes = body
        .get("scope")
        .and_then(|v| v.as_str())
        .map(|s| {
            s.split([',', ' '])
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Ok(TokenResponse {
        access_token: access_token.to_string(),
        refresh_token: body
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        expires_in: body.get("expires_in").and_then(|v| v.as_u64()),
        scopes,
        token_type: body
            .get("token_type")
            .and_then(|v| v.as_str())
            .unwrap_or("Bearer")
            .to_string(),
    })
}

/// Exchange an authorization code for tokens (RFC 6749 §4.1.3).
fn exchange_code(
    agent: &ureq::Agent,
    prov: OAuthProvider,
    client_id: &str,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<OAuthToken, SecretsError> {
    let params = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", verifier),
    ];
    Ok(post_token(
        agent,
        prov.id,
        prov.token_url,
        prov.token_auth,
        client_id,
        &params,
    )?
    .into_token())
}

/// Refresh an access token (RFC 6749 §6). `client_id` is a parameter so
/// tests can drive this against a fake endpoint without env mutation.
fn refresh_access_token_with(
    agent: &ureq::Agent,
    prov: OAuthProvider,
    client_id: &str,
    refresh_token: &str,
) -> Result<OAuthToken, SecretsError> {
    let params = [
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    let mut token = post_token(
        agent,
        prov.id,
        prov.token_url,
        prov.token_auth,
        client_id,
        &params,
    )?
    .into_token();
    // Providers that do not rotate the refresh token (Google) omit it:
    // keep the old one rather than losing the ability to refresh.
    if token.refresh_token.is_none() {
        token.refresh_token = Some(SecretValue::new(refresh_token));
    }
    Ok(token)
}

// ---------------------------------------------------------------------------
// PKCE (RFC 7636) and the RFC 8252 loopback flow
// ---------------------------------------------------------------------------

/// Standard base64 with padding. PKCE derives the URL-safe form from it.
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let mut n: u32 = 0;
        for (i, &b) in chunk.iter().enumerate() {
            n |= (b as u32) << (16 - 8 * i);
        }
        let pad = 3 - chunk.len();
        for i in 0..4 - pad {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
        for _ in 0..pad {
            out.push('=');
        }
    }
    out
}

/// Base64url without padding (RFC 7636 §4.2).
fn base64url_nopad(data: &[u8]) -> String {
    base64_encode(data)
        .replace('+', "-")
        .replace('/', "_")
        .trim_end_matches('=')
        .to_string()
}

/// Generate a PKCE verifier/challenge pair (S256). The verifier is 64
/// characters from the RFC 7636 unreserved set.
pub fn pkce_pair() -> (String, String) {
    use rand::Rng;
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
    let mut rng = rand::thread_rng();
    let verifier: String = (0..64)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect();
    let challenge = pkce_challenge(&verifier);
    (verifier, challenge)
}

fn pkce_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    base64url_nopad(&Sha256::digest(verifier.as_bytes()))
}

/// Unpredictable `state` for the authorization request (CSRF protection,
/// RFC 6749 §10.12).
fn random_state() -> String {
    use rand::Rng;
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..32)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect()
}

/// Percent-encode a query component (RFC 3986 unreserved set).
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Percent-decode a query component. Malformed sequences pass through
/// rather than failing the whole callback.
fn url_decode(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (
                (bytes[i + 1] as char).to_digit(16),
                (bytes[i + 2] as char).to_digit(16),
            ) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((url_decode(k), url_decode(v)))
        })
        .collect()
}

/// Best-effort browser open. Failures are silent: the caller always prints
/// the URL as the fallback.
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let prog = "open";
    #[cfg(target_os = "windows")]
    let prog = "cmd";
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let prog = "xdg-open";
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new(prog)
        .args(["/c", "start", "", url])
        .spawn();
    #[cfg(not(target_os = "windows"))]
    let result = std::process::Command::new(prog).arg(url).spawn();
    let _ = result;
}

fn respond(stream: &TcpStream, status: u16, message: &str) -> Result<(), SecretsError> {
    let reason = if status == 200 { "OK" } else { "Bad Request" };
    let body = format!(
        "<html><body><h2>{message}</h2><p>You can close this tab and return to the terminal.</p></body></html>"
    );
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = stream;
    stream
        .write_all(response.as_bytes())
        .map_err(SecretsError::from)?;
    stream.flush().map_err(SecretsError::from)?;
    Ok(())
}

/// Accept the single OAuth callback on the loopback listener and return
/// the authorization code. The `state` parameter is checked against the
/// value sent (CSRF protection); a mismatch fails closed.
fn wait_for_code(listener: &TcpListener, expected_state: &str) -> Result<String, SecretsError> {
    let (stream, _) = listener.accept().map_err(SecretsError::from)?;
    stream
        .set_read_timeout(Some(Duration::from_secs(300)))
        .map_err(SecretsError::from)?;
    // One small GET: cap the head so a stray connection cannot make us
    // buffer without bound.
    let mut head = Vec::new();
    {
        let mut reader = BufReader::new(&stream);
        loop {
            let mut line = Vec::new();
            let n = reader
                .read_until(b'\n', &mut line)
                .map_err(SecretsError::from)?;
            if n == 0 {
                break; // EOF or timeout
            }
            head.extend_from_slice(&line);
            if head.len() > 16_384 {
                return Err(SecretsError::Invalid(
                    "callback request head too large".into(),
                ));
            }
            if line == b"\r\n" {
                break;
            }
        }
    }
    let head_str = String::from_utf8_lossy(&head);
    let request_line = head_str.lines().next().unwrap_or("");
    let target = request_line.split_whitespace().nth(1).unwrap_or("");
    let query = target.split('?').nth(1).unwrap_or("");
    let params = parse_query(query);
    if let Some(err) = params.get("error") {
        let _ = respond(&stream, 400, "Authorization failed.");
        return Err(SecretsError::Invalid(format!(
            "provider returned error: {err}"
        )));
    }
    match (params.get("code"), params.get("state")) {
        (Some(code), Some(state)) if state == expected_state && !code.is_empty() => {
            let _ = respond(&stream, 200, "Signed in.");
            Ok(code.clone())
        }
        _ => {
            let _ = respond(&stream, 400, "Authorization failed.");
            Err(SecretsError::Invalid(
                "callback missing code or state mismatch (possible CSRF)".into(),
            ))
        }
    }
}

/// Run the RFC 8252 loopback authorization flow: open the provider's
/// consent page, receive the authorization code on an ephemeral
/// `127.0.0.1` listener, and exchange it for tokens. Blocks until the
/// user completes (or abandons) the browser step. `client_id` is a public
/// identifier, never a secret.
pub fn login_flow(prov: OAuthProvider, client_id: &str) -> Result<OAuthToken, SecretsError> {
    validate_name(prov.id)?;
    let listener = TcpListener::bind("127.0.0.1:0").map_err(SecretsError::from)?;
    let port = listener.local_addr().map_err(SecretsError::from)?.port();
    // Loopback redirect: the port is ephemeral per RFC 8252 §7.3, so the
    // exact URI is printed for the app's redirect allowlist.
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let (verifier, challenge) = pkce_pair();
    let state = random_state();
    let mut url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        prov.auth_url,
        url_encode(client_id),
        url_encode(&redirect_uri),
        url_encode(&prov.default_scopes.join(" ")),
        url_encode(&state),
        url_encode(&challenge),
    );
    for (k, v) in prov.extra_auth_params {
        url.push_str(&format!("&{}={}", url_encode(k), url_encode(v)));
    }
    println!(
        "Opening the browser for {} sign-in.\nIf it does not open, visit this URL:\n{url}\n\nWaiting for the callback on {redirect_uri} ...",
        prov.display
    );
    open_browser(&url);
    let code = wait_for_code(&listener, &state)?;
    exchange_code(
        &http_agent(),
        prov,
        client_id,
        &code,
        &redirect_uri,
        &verifier,
    )
}

// ---------------------------------------------------------------------------
// Broker-side request execution: the "model never sees the token" boundary
// ---------------------------------------------------------------------------

impl SecretsBroker {
    /// Perform an authenticated API request broker-side: resolve a valid
    /// token (refreshing when needed), send it as `Authorization: Bearer`,
    /// and return the response body. The token never leaves this function
    /// and is never logged.
    ///
    /// Fails closed: unknown app, unconnected app, expired token without a
    /// refresh token, disallowed method, and any URL outside the provider's
    /// API base are all errors (the first three name the
    /// `pantheon oauth login <app>` remediation).
    pub fn oauth_request(
        &self,
        app: &str,
        method: &str,
        url: &str,
        body: Option<&str>,
    ) -> Result<String, SecretsError> {
        let prov = provider(app)
            .ok_or_else(|| SecretsError::Invalid(format!("unknown oauth app {app:?}")))?;
        let method = method.to_ascii_uppercase();
        match method.as_str() {
            "GET" | "POST" | "PUT" | "PATCH" | "DELETE" => {}
            _ => {
                return Err(SecretsError::Invalid(format!(
                    "unsupported oauth method {method:?}"
                )))
            }
        }
        // Exact base or base + "/": `api.github.com.evil.com` does not pass.
        if !(url == prov.api_base || url.starts_with(&format!("{}/", prov.api_base))) {
            return Err(SecretsError::Invalid(format!("url outside {app} api base")));
        }
        let token = OAuthStore::new(self).ensure_valid_token(app)?;
        let agent = http_agent();
        let mut req = agent
            .request(&method, url)
            .set("Authorization", &format!("Bearer {}", token.expose()))
            .set("Accept", "application/json");
        for (k, v) in prov.extra_headers {
            req = req.set(k, v);
        }
        let result = match body {
            Some(b) => req.set("Content-Type", "application/json").send_string(b),
            None => req.call(),
        };
        let resp = result.map_err(|e| match e {
            ureq::Error::Status(code, r) => {
                let detail = r.into_string().unwrap_or_default();
                let detail: String = detail.chars().take(300).collect();
                SecretsError::Backend(format!("{app} api error {code}: {detail}"))
            }
            other => SecretsError::Backend(format!("{app} request failed: {other}")),
        })?;
        // The response body travels back through the tool-result path,
        // which the session scrubs against the redaction registry: even a
        // misbehaving API that echoed the token could not reach the model.
        resp.into_string()
            .map_err(|e| SecretsError::Backend(format!("{app} read response: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::MemoryVault;
    use std::io::Read;

    fn test_broker() -> SecretsBroker {
        SecretsBroker::new().with_vault(Box::new(MemoryVault::new()))
    }

    fn test_token(access: &str) -> OAuthToken {
        OAuthToken {
            access_token: SecretValue::new(access),
            refresh_token: Some(SecretValue::new("refresh-123")),
            expires_at: None,
            scopes: vec!["read:user".into()],
            token_type: "Bearer".into(),
        }
    }

    #[test]
    fn pkce_matches_rfc7636_vector() {
        // RFC 7636 Appendix B test vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        // And the generator's own pairs are self-consistent.
        let (v, c) = pkce_pair();
        assert_eq!(v.len(), 64);
        assert_eq!(c, pkce_challenge(&v));
    }

    #[test]
    fn base64url_nopad_shape() {
        // 32 zero bytes -> 43 chars, no padding, URL-safe alphabet.
        assert_eq!(
            base64url_nopad(&[0u8; 32]),
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
        );
    }

    #[test]
    fn store_round_trip_memory_vault() {
        let broker = test_broker();
        let store = OAuthStore::new(&broker);
        let token = test_token("access-abc");
        store.save("github", &token).unwrap();
        let loaded = store.load("github").unwrap().expect("saved token");
        assert_eq!(loaded.access_token.expose(), "access-abc");
        assert_eq!(loaded.refresh_token.unwrap().expose(), "refresh-123");
        assert_eq!(loaded.scopes, vec!["read:user"]);
        let status = store.status("github").unwrap();
        assert!(status.connected);
        // Never-connected app: clean negative, no error.
        assert!(!store.status("google").unwrap().connected);
        assert!(store.load("google").unwrap().is_none());
        // Unknown app id is rejected, not silently namespaced.
        assert!(store.load("eviltwin").is_err());
    }

    #[test]
    fn delete_removes_all_records() {
        let broker = test_broker();
        let store = OAuthStore::new(&broker);
        store.save("spotify", &test_token("access-x")).unwrap();
        assert!(store.status("spotify").unwrap().connected);
        store.delete("spotify").unwrap();
        assert!(!store.status("spotify").unwrap().connected);
        // Deleting twice is a no-op, per the vault contract.
        store.delete("spotify").unwrap();
    }

    #[test]
    fn expired_without_refresh_fails_closed() {
        let broker = test_broker();
        let store = OAuthStore::new(&broker);
        let mut token = test_token("stale-access");
        token.expires_at = Some(SystemTime::now() - Duration::from_secs(10));
        token.refresh_token = None;
        store.save("github", &token).unwrap();
        let err = store.ensure_valid_token("github").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("pantheon oauth login github"),
            "unexpected: {msg}"
        );
    }

    #[test]
    fn not_connected_fails_closed() {
        let broker = test_broker();
        let store = OAuthStore::new(&broker);
        let err = store.ensure_valid_token("notion").unwrap_err();
        assert!(err.to_string().contains("pantheon oauth login notion"));
    }

    #[test]
    fn redaction_registered_on_handout() {
        let broker = test_broker();
        let store = OAuthStore::new(&broker);
        store
            .save("google", &test_token("handout-secret-xyz"))
            .unwrap();
        let token = store.ensure_valid_token("google").unwrap();
        assert_eq!(token.expose(), "handout-secret-xyz");
        // The handed-out bytes are registered before any tool result exists.
        let scrubbed = broker
            .redaction_registry()
            .scrub("echo handout-secret-xyz done");
        assert_eq!(scrubbed, "echo [redacted-secret] done");
    }

    /// Serve one token-endpoint POST from a loopback listener and capture
    /// the request body, so the refresh grant is verified without network.
    fn fake_token_endpoint(
        response_json: &'static str,
    ) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut reader = BufReader::new(&stream);
            let mut head = Vec::new();
            let content_length: usize = loop {
                let mut line = Vec::new();
                reader.read_until(b'\n', &mut line).unwrap();
                let text = String::from_utf8_lossy(&line).to_lowercase();
                if text.starts_with("content-length:") {
                    head.extend_from_slice(&line);
                    let len: usize = text
                        .strip_prefix("content-length:")
                        .unwrap_or("")
                        .trim()
                        .parse()
                        .unwrap_or(0);
                    // Keep reading to the blank line, then read the body.
                    loop {
                        let mut l2 = Vec::new();
                        reader.read_until(b'\n', &mut l2).unwrap();
                        head.extend_from_slice(&l2);
                        if l2 == b"\r\n" {
                            break;
                        }
                    }
                    break len;
                }
                head.extend_from_slice(&line);
                if line == b"\r\n" {
                    break 0;
                }
            };
            let mut body = vec![0u8; content_length];
            let mut read = 0;
            while read < content_length {
                let n = reader.read(&mut body[read..]).unwrap();
                if n == 0 {
                    break;
                }
                read += n;
            }
            let body_text = String::from_utf8_lossy(&body[..read]).into_owned();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_json}",
                response_json.len()
            );
            let _ = (&stream).write_all(response.as_bytes());
            body_text
        });
        (format!("http://127.0.0.1:{port}/token"), handle)
    }

    #[test]
    fn refresh_grant_against_fake_endpoint() {
        let (token_url, handle) = fake_token_endpoint(
            r#"{"access_token":"fresh-access-1","refresh_token":"fresh-refresh-1","expires_in":3600,"scope":"read:user repo","token_type":"Bearer"}"#,
        );
        let leaked: &'static str = Box::leak(token_url.into_boxed_str());
        let prov = OAuthProvider {
            token_url: leaked,
            ..GITHUB
        };
        // No proxy: the fake endpoint is loopback, and ureq has no
        // no_proxy support (its env proxy would refuse the address).
        let agent = ureq::AgentBuilder::new().try_proxy_from_env(false).build();
        let token =
            refresh_access_token_with(&agent, prov, "test-client-id", "old-refresh").unwrap();
        assert_eq!(token.access_token.expose(), "fresh-access-1");
        assert_eq!(token.refresh_token.unwrap().expose(), "fresh-refresh-1");
        assert_eq!(token.scopes, vec!["read:user", "repo"]);
        assert!(token.expires_at.is_some());
        let body = handle.join().unwrap();
        assert!(body.contains("grant_type=refresh_token"), "{body}");
        assert!(body.contains("refresh_token=old-refresh"), "{body}");
        assert!(body.contains("client_id=test-client-id"), "{body}");
    }

    #[test]
    fn refresh_without_rotation_keeps_old_token() {
        // Google does not rotate: no refresh_token in the response.
        let (token_url, _handle) = fake_token_endpoint(
            r#"{"access_token":"fresh-access-2","expires_in":3600,"token_type":"Bearer"}"#,
        );
        let leaked: &'static str = Box::leak(token_url.into_boxed_str());
        let prov = OAuthProvider {
            token_url: leaked,
            ..GOOGLE
        };
        let agent = ureq::AgentBuilder::new().try_proxy_from_env(false).build();
        let token = refresh_access_token_with(&agent, prov, "cid", "kept-refresh").unwrap();
        assert_eq!(token.refresh_token.unwrap().expose(), "kept-refresh");
    }

    #[test]
    fn token_endpoint_error_surfaces() {
        let (token_url, _handle) = fake_token_endpoint(
            r#"{"error":"invalid_grant","error_description":"bad refresh token"}"#,
        );
        let leaked: &'static str = Box::leak(token_url.into_boxed_str());
        let prov = OAuthProvider {
            token_url: leaked,
            ..SPOTIFY
        };
        let agent = ureq::AgentBuilder::new().try_proxy_from_env(false).build();
        let err = refresh_access_token_with(&agent, prov, "cid", "bad").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid_grant"), "{msg}");
        assert!(msg.contains("bad refresh token"), "{msg}");
    }

    #[test]
    fn url_allowlist_rejection_needs_no_network() {
        let broker = test_broker();
        // Subdomain trick: rejected before any token resolution.
        let err = broker
            .oauth_request("github", "GET", "https://api.github.com.evil.com/x", None)
            .unwrap_err();
        assert!(err.to_string().contains("outside github api base"));
        // Wrong provider base entirely.
        let err = broker
            .oauth_request("notion", "GET", "https://api.github.com/user", None)
            .unwrap_err();
        assert!(err.to_string().contains("outside notion api base"));
        // Disallowed method.
        let err = broker
            .oauth_request("spotify", "TRACE", "https://api.spotify.com/v1/me", None)
            .unwrap_err();
        assert!(err.to_string().contains("unsupported oauth method"));
        // Unknown app.
        let err = broker
            .oauth_request("eviltwin", "GET", "https://example.com/", None)
            .unwrap_err();
        assert!(err.to_string().contains("unknown oauth app"));
    }

    #[test]
    fn provider_table_has_four_apps() {
        assert_eq!(PROVIDERS.len(), 4);
        for id in ["github", "google", "notion", "spotify"] {
            assert!(provider(id).is_some(), "{id}");
        }
        assert!(provider("eviltwin").is_none());
        // Client IDs come from env, never baked in.
        for p in PROVIDERS {
            assert!(p.client_id_env.starts_with("PANTHEON_OAUTH_"));
            assert!(!p.api_base.is_empty());
        }
    }
}
