//! MCP OAuth 2.1: discovery, dynamic client registration, PKCE, and
//! encrypted token storage.
//!
//! Remote MCP servers advertise protected-resource metadata (RFC 9728)
//! and authorization-server metadata (RFC 8414). Pantheon discovers both,
//! registers a public client with the authorization server when no
//! `client_id` is configured, runs the RFC 8252 loopback authorization
//! code flow with PKCE S256, and stores the resulting token set through
//! the same [`SecretsBroker`] the rest of Pantheon uses (OS keychain, or
//! the encrypted file vault on headless hosts).
//!
//! This module deliberately has no UI. `pantheon mcp login <server>` runs
//! the interactive flow; runtime connects only load and refresh tokens.
//! That split keeps a browser prompt from blocking an agent turn and makes
//! the failure mode explicit: an unauthenticated server reports
//! [`McpError::Unauthorized`] with the login command to run.

use crate::manager::{McpAuth, McpServerSpec};
use crate::McpError;
use pantheon_secrets::oauth::pkce_pair;
use pantheon_secrets::{SecretValue, SecretsBroker};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Default)]
struct ResourceMetadata {
    resource: Option<String>,
    authorization_servers: Vec<String>,
    scopes_supported: Vec<String>,
}

#[derive(Debug, Clone)]
struct AuthServerMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    scopes_supported: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredMeta {
    /// Server name is stored so a sanitized-key collision cannot make one
    /// server read another server's tokens.
    server: String,
    expires_at: Option<u64>,
    token_endpoint: String,
    client_id: String,
    scopes: Vec<String>,
    resource: Option<String>,
    token_type: String,
}

/// A stored OAuth token: access token, refresh token (optional), metadata,
/// and client secret (optional). Returned by [`TokenStore::load`].
struct StoredToken {
    access: String,
    refresh: Option<String>,
    meta: StoredMeta,
    secret: Option<String>,
}

struct TokenStore<'a> {
    broker: &'a SecretsBroker,
    prefix: String,
}

impl<'a> TokenStore<'a> {
    fn new(broker: &'a SecretsBroker, server: &str) -> Self {
        // Secrets names are [A-Za-z0-9_]. The FNV suffix keeps `a-b` and
        // `a_b` from sharing a token slot while staying human-readable.
        let mut hash: u64 = 0xcbf29ce484222325;
        for b in server.as_bytes() {
            hash ^= u64::from(*b);
            hash = hash.wrapping_mul(0x100000001b3);
        }
        Self {
            broker,
            prefix: format!(
                "mcp_oauth_{}_{hash:016x}",
                crate::manager::sanitize_segment(server)
            ),
        }
    }

    fn key(&self, field: &str) -> String {
        format!("{}_{field}", self.prefix)
    }

    fn save(
        &self,
        access_token: &str,
        refresh_token: Option<&str>,
        meta: &StoredMeta,
        client_secret: Option<&str>,
    ) -> Result<(), McpError> {
        self.set_secret(&self.key("access_token"), access_token)?;
        match refresh_token {
            Some(t) if !t.is_empty() => self.set_secret(&self.key("refresh_token"), t)?,
            _ => self.delete_secret(&self.key("refresh_token"))?,
        }
        match client_secret {
            Some(s) if !s.is_empty() => self.set_secret(&self.key("client_secret"), s)?,
            _ => self.delete_secret(&self.key("client_secret"))?,
        }
        let json = serde_json::to_string(meta)
            .map_err(|e| McpError::Protocol(format!("oauth metadata encode: {e}")))?;
        self.set_secret(&self.key("meta"), &json)
    }

    fn set_secret(&self, key: &str, value: &str) -> Result<(), McpError> {
        self.broker
            .set(key, SecretValue::new(value.to_string()))
            .map_err(|e| {
                McpError::Unauthorized(format!(
                    "cannot store MCP OAuth token ({e}); run `pantheon mcp login` on a host with a durable vault"
                ))
            })
    }

    fn delete_secret(&self, key: &str) -> Result<(), McpError> {
        self.broker
            .delete(key)
            .map_err(|e| McpError::Protocol(format!("oauth token delete: {e}")))
    }

    fn load(&self) -> Result<Option<StoredToken>, McpError> {
        let Some(access) = self
            .broker
            .resolve(&self.key("access_token"))
            .map_err(|e| McpError::Protocol(format!("oauth token read: {e}")))?
        else {
            return Ok(None);
        };
        let Some(meta) = self
            .broker
            .resolve(&self.key("meta"))
            .map_err(|e| McpError::Protocol(format!("oauth metadata read: {e}")))?
        else {
            return Ok(None);
        };
        let meta: StoredMeta = serde_json::from_str(meta.expose())
            .map_err(|e| McpError::Protocol(format!("oauth metadata corrupt: {e}")))?;
        let refresh = self
            .broker
            .resolve(&self.key("refresh_token"))
            .map_err(|e| McpError::Protocol(format!("oauth refresh read: {e}")))?
            .map(|v| v.expose().to_string());
        let secret = self
            .broker
            .resolve(&self.key("client_secret"))
            .map_err(|e| McpError::Protocol(format!("oauth secret read: {e}")))?
            .map(|v| v.expose().to_string());
        Ok(Some(StoredToken {
            access: access.expose().to_string(),
            refresh,
            meta,
            secret,
        }))
    }
}

/// Resolve a usable bearer token for a remote server, refreshing it when
/// needed. `None`/expired-without-refresh is an actionable auth error.
pub fn ensure_token(spec: &McpServerSpec, broker: &SecretsBroker) -> Result<String, McpError> {
    if spec.auth != McpAuth::OAuth {
        return Err(McpError::Unauthorized(format!(
            "mcp server '{}': OAuth mode is not enabled",
            spec.name
        )));
    }
    let store = TokenStore::new(broker, &spec.name);
    let Some(tok) = store.load()? else {
        return Err(McpError::Unauthorized(format!(
            "mcp server '{}' is not authenticated; run `pantheon mcp login {}`",
            spec.name, spec.name
        )));
    };
    let access = tok.access;
    let refresh = tok.refresh;
    let meta = tok.meta;
    let client_secret = tok.secret;
    if meta.server != spec.name {
        return Err(McpError::Unauthorized(format!(
            "mcp server '{}': stored OAuth token belongs to '{}'; run `pantheon mcp login {}`",
            spec.name, meta.server, spec.name
        )));
    }
    if !expired(meta.expires_at) {
        return Ok(access);
    }
    let Some(refresh) = refresh else {
        return Err(McpError::Unauthorized(format!(
            "mcp server '{}' OAuth token expired and no refresh token is stored; run `pantheon mcp login {}`",
            spec.name, spec.name
        )));
    };
    let (new_access, new_refresh) = refresh_token(&meta, &refresh, client_secret.as_deref())
        .map_err(|e| {
            McpError::Unauthorized(format!(
                "mcp server '{}' OAuth refresh failed: {e}; run `pantheon mcp login {}`",
                spec.name, spec.name
            ))
        })?;
    let mut next_meta = meta;
    next_meta.expires_at = expires_at_from_response(&new_access);
    store.save(
        &new_access.access_token,
        new_refresh.as_deref().or(Some(&refresh)),
        &next_meta,
        client_secret.as_deref(),
    )?;
    Ok(new_access.access_token)
}

/// Interactive login for `pantheon mcp login <server>`.
pub fn login(spec: &McpServerSpec, broker: &SecretsBroker) -> Result<(), McpError> {
    if spec.auth != McpAuth::OAuth {
        return Err(McpError::Unauthorized(format!(
            "mcp server '{}' does not set auth = \"oauth\"",
            spec.name
        )));
    }
    let resource_url = spec
        .url
        .as_deref()
        .filter(|u| !u.trim().is_empty())
        .ok_or_else(|| McpError::Unauthorized("OAuth requires a remote url".into()))?;
    let resource = discover_resource(resource_url)?;
    let auth_server = resource
        .authorization_servers
        .first()
        .cloned()
        .unwrap_or_else(|| origin(resource_url).unwrap_or_else(|| resource_url.to_string()));
    let auth = discover_auth_server(&auth_server)?;

    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|e| McpError::Io(format!("oauth callback bind: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| McpError::Io(format!("oauth callback address: {e}")))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let (client_id, client_secret) = match spec.client_id.clone() {
        Some(id) if !id.trim().is_empty() => (id, None),
        _ => register_client(&auth, &redirect_uri)?,
    };
    let (verifier, challenge) = pkce_pair();
    let state = random_state();
    let scopes = if !spec.scopes.is_empty() {
        spec.scopes.clone()
    } else if !resource.scopes_supported.is_empty() {
        resource.scopes_supported.clone()
    } else {
        auth.scopes_supported.clone()
    };
    let resource_param = resource
        .resource
        .clone()
        .unwrap_or_else(|| resource_url.to_string());

    let mut url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256",
        auth.authorization_endpoint,
        url_encode(&client_id),
        url_encode(&redirect_uri),
        url_encode(&state),
        url_encode(&challenge),
    );
    if !scopes.is_empty() {
        url.push_str(&format!("&scope={}", url_encode(&scopes.join(" "))));
    }
    url.push_str(&format!("&resource={}", url_encode(&resource_param)));

    eprintln!(
        "Opening the browser for MCP server '{}'.\nIf it does not open, visit:\n{url}\n\nWaiting for the callback on {redirect_uri} ...",
        spec.name
    );
    open_browser(&url);
    let code = wait_for_code(&listener, &state)?;
    let token = exchange_code(
        &auth.token_endpoint,
        &client_id,
        client_secret.as_deref(),
        &code,
        &redirect_uri,
        &verifier,
    )?;
    let meta = StoredMeta {
        server: spec.name.clone(),
        expires_at: token.expires_at,
        token_endpoint: auth.token_endpoint.clone(),
        client_id: client_id.clone(),
        scopes,
        resource: Some(resource_param),
        token_type: token.token_type.clone(),
    };
    TokenStore::new(broker, &spec.name).save(
        &token.access_token,
        token.refresh_token.as_deref(),
        &meta,
        client_secret.as_deref(),
    )?;
    eprintln!("MCP server '{}' is authenticated.", spec.name);
    Ok(())
}

/// Remove every token/registration record for a server.
pub fn logout(spec: &McpServerSpec, broker: &SecretsBroker) -> Result<(), McpError> {
    let store = TokenStore::new(broker, &spec.name);
    for field in ["access_token", "refresh_token", "meta", "client_secret"] {
        store.delete_secret(&store.key(field))?;
    }
    Ok(())
}

/// `(connected, scopes, expires_at)` without exposing token bytes.
pub fn status(
    spec: &McpServerSpec,
    broker: &SecretsBroker,
) -> Result<(bool, Vec<String>, Option<u64>), McpError> {
    if spec.auth != McpAuth::OAuth {
        return Ok((false, Vec::new(), None));
    }
    let store = TokenStore::new(broker, &spec.name);
    match store.load()? {
        Some(tok) if tok.meta.server == spec.name => {
            Ok((true, tok.meta.scopes, tok.meta.expires_at))
        }
        _ => Ok((false, Vec::new(), None)),
    }
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

fn discover_resource(resource_url: &str) -> Result<ResourceMetadata, McpError> {
    let base = origin(resource_url)
        .ok_or_else(|| McpError::Unauthorized(format!("invalid MCP url {resource_url:?}")))?;
    let path = path_of(resource_url);
    let mut candidates = vec![format!("{base}/.well-known/oauth-protected-resource")];
    if !path.is_empty() && path != "/" {
        candidates.push(format!(
            "{base}/.well-known/oauth-protected-resource{}",
            path.trim_end_matches('/')
        ));
    }
    for url in candidates {
        if let Some(v) = get_json_opt(&url)? {
            return Ok(resource_from(&v));
        }
    }
    // RFC 9728 also allows the resource to point at its metadata in a
    // WWW-Authenticate challenge. Probe the MCP endpoint once; a 401 is
    // expected and carries the pointer when the server implements it.
    // Statuses surface as responses (ureq 3), so one Ok arm covers the
    // 401 too; only a transport failure loses the challenge.
    let agent = http_agent();
    let challenge = match agent.get(resource_url).call() {
        Ok(resp) => resp
            .headers()
            .get("WWW-Authenticate")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        Err(_) => None,
    };
    if let Some(challenge) = challenge {
        if let Some(url) = header_param(&challenge, "resource_metadata") {
            if let Some(v) = get_json_opt(&url)? {
                return Ok(resource_from(&v));
            }
        }
    }
    Ok(ResourceMetadata::default())
}

fn discover_auth_server(issuer: &str) -> Result<AuthServerMetadata, McpError> {
    let base = origin(issuer).ok_or_else(|| {
        McpError::Unauthorized(format!("invalid authorization server {issuer:?}"))
    })?;
    let path = path_of(issuer);
    let mut candidates = vec![format!("{base}/.well-known/oauth-authorization-server")];
    if !path.is_empty() && path != "/" {
        candidates.push(format!(
            "{base}/.well-known/oauth-authorization-server{}",
            path.trim_end_matches('/')
        ));
    }
    candidates.push(format!("{base}/.well-known/openid-configuration"));
    for url in candidates {
        if let Some(v) = get_json_opt(&url)? {
            let authorization_endpoint = str_field(&v, "authorization_endpoint");
            let token_endpoint = str_field(&v, "token_endpoint");
            if let (Some(a), Some(t)) = (authorization_endpoint, token_endpoint) {
                let issuer = str_field(&v, "issuer").unwrap_or_else(|| issuer.to_string());
                return Ok(AuthServerMetadata {
                    issuer,
                    authorization_endpoint: a,
                    token_endpoint: t,
                    registration_endpoint: str_field(&v, "registration_endpoint"),
                    scopes_supported: string_array(&v, "scopes_supported"),
                });
            }
        }
    }
    Err(McpError::Unauthorized(format!(
        "no OAuth authorization-server metadata at {issuer}; check the server URL or set auth = \"none\" with explicit headers"
    )))
}

fn resource_from(v: &Value) -> ResourceMetadata {
    ResourceMetadata {
        resource: str_field(v, "resource"),
        authorization_servers: string_array(v, "authorization_servers"),
        scopes_supported: string_array(v, "scopes_supported"),
    }
}

// ---------------------------------------------------------------------------
// Dynamic registration + token exchange
// ---------------------------------------------------------------------------

fn register_client(
    auth: &AuthServerMetadata,
    redirect_uri: &str,
) -> Result<(String, Option<String>), McpError> {
    let endpoint = auth.registration_endpoint.as_deref().ok_or_else(|| {
        McpError::Unauthorized(format!(
            "authorization server {} does not support dynamic client registration; set client_id in [mcp.servers.<name>]",
            auth.issuer
        ))
    })?;
    let body = serde_json::json!({
        "client_name": "Pantheon",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
    });
    let resp = http_agent()
        .post(endpoint)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .send_json(body)
        .map_err(|e| McpError::Unauthorized(format!("dynamic client registration failed: {e}")))?;
    if resp.status().as_u16() >= 400 {
        // Same message the ureq 2 status error displayed.
        let code = resp.status().as_u16();
        return Err(McpError::Unauthorized(format!(
            "dynamic client registration failed: {endpoint}: status code {code}"
        )));
    }
    let v: Value = resp
        .into_body()
        .read_json()
        .map_err(|e| McpError::Protocol(format!("registration response: {e}")))?;
    let client_id = str_field(&v, "client_id")
        .ok_or_else(|| McpError::Protocol("registration response has no client_id".into()))?;
    Ok((client_id, str_field(&v, "client_secret")))
}

#[derive(Debug)]
struct TokenSet {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: Option<u64>,
    token_type: String,
}

fn exchange_code(
    token_endpoint: &str,
    client_id: &str,
    client_secret: Option<&str>,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<TokenSet, McpError> {
    let mut params = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", verifier),
    ];
    if let Some(secret) = client_secret.filter(|s| !s.is_empty()) {
        params.push(("client_secret", secret));
    }
    token_request(token_endpoint, &params)
}

fn refresh_token(
    meta: &StoredMeta,
    refresh: &str,
    client_secret: Option<&str>,
) -> Result<(TokenSet, Option<String>), McpError> {
    let mut params = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh),
        ("client_id", meta.client_id.as_str()),
    ];
    if let Some(secret) = client_secret.filter(|s| !s.is_empty()) {
        params.push(("client_secret", secret));
    }
    let token = token_request(&meta.token_endpoint, &params)?;
    let rotated = token.refresh_token.clone();
    Ok((token, rotated))
}

fn token_request(endpoint: &str, params: &[(&str, &str)]) -> Result<TokenSet, McpError> {
    let resp = http_agent()
        .post(endpoint)
        .header("Accept", "application/json")
        .send_form(params.iter().copied())
        .map_err(|e| McpError::Unauthorized(format!("token request failed: {e}")))?;
    if resp.status().as_u16() >= 400 {
        let code = resp.status().as_u16();
        let detail = resp.into_body().read_to_string().unwrap_or_default();
        return Err(McpError::Unauthorized(format!(
            "token endpoint returned {code}: {}",
            detail.chars().take(300).collect::<String>()
        )));
    }
    let v: Value = resp
        .into_body()
        .read_json()
        .map_err(|e| McpError::Protocol(format!("token response is not JSON: {e}")))?;
    if let Some(err) = v.get("error").and_then(|e| e.as_str()) {
        let desc = v
            .get("error_description")
            .and_then(|d| d.as_str())
            .unwrap_or("");
        return Err(McpError::Unauthorized(format!(
            "token endpoint error: {err} {desc}"
        )));
    }
    let access_token = str_field(&v, "access_token")
        .ok_or_else(|| McpError::Protocol("token response has no access_token".into()))?;
    Ok(TokenSet {
        access_token,
        refresh_token: str_field(&v, "refresh_token"),
        expires_at: v
            .get("expires_in")
            .and_then(|e| e.as_u64())
            .map(|s| now_secs().saturating_add(s)),
        token_type: str_field(&v, "token_type").unwrap_or_else(|| "Bearer".into()),
    })
}

fn expires_at_from_response(token: &TokenSet) -> Option<u64> {
    token.expires_at
}

fn expired(at: Option<u64>) -> bool {
    match at {
        Some(at) => now_secs().saturating_add(60) >= at,
        None => false,
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Loopback callback + small URL/HTTP helpers
// ---------------------------------------------------------------------------

fn wait_for_code(listener: &TcpListener, expected_state: &str) -> Result<String, McpError> {
    let deadline = Instant::now() + LOGIN_TIMEOUT;
    listener
        .set_nonblocking(true)
        .map_err(|e| McpError::Io(format!("oauth listener: {e}")))?;
    loop {
        if Instant::now() > deadline {
            return Err(McpError::Timeout {
                method: "oauth callback".into(),
            });
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if let Some(code) = handle_callback(stream, expected_state)? {
                    return Ok(code);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(McpError::Io(format!("oauth callback: {e}"))),
        }
    }
}

fn handle_callback(stream: TcpStream, expected_state: &str) -> Result<Option<String>, McpError> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| McpError::Io(format!("oauth callback: {e}")))?;
    let mut head = Vec::new();
    {
        let mut reader = BufReader::new(&stream);
        loop {
            let mut line = Vec::new();
            let n = reader
                .read_until(b'\n', &mut line)
                .map_err(|e| McpError::Io(format!("oauth callback read: {e}")))?;
            if n == 0 {
                break;
            }
            head.extend_from_slice(&line);
            if head.len() > 16_384 {
                return Err(McpError::Protocol("oauth callback head too large".into()));
            }
            if line == b"\r\n" {
                break;
            }
        }
    }
    let head = String::from_utf8_lossy(&head);
    let target = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("");
    if !target.starts_with("/callback") {
        let _ = respond(&stream, 404, "Not found.");
        return Ok(None);
    }
    let params = parse_query(target.split('?').nth(1).unwrap_or(""));
    if let Some(err) = params.get("error") {
        let _ = respond(&stream, 400, "Authorization failed.");
        return Err(McpError::Unauthorized(format!(
            "authorization server returned error: {err}"
        )));
    }
    match (params.get("code"), params.get("state")) {
        (Some(code), Some(state)) if state == expected_state && !code.is_empty() => {
            let _ = respond(&stream, 200, "Signed in. You can close this tab.");
            Ok(Some(code.clone()))
        }
        _ => {
            let _ = respond(&stream, 400, "Authorization failed.");
            Err(McpError::Unauthorized(
                "oauth callback missing code or state mismatch (possible CSRF)".into(),
            ))
        }
    }
}

fn respond(stream: &TcpStream, status: u16, message: &str) -> Result<(), McpError> {
    let reason = if status == 200 { "OK" } else { "Error" };
    let body = format!(
        "<html><body><h2>{message}</h2><p>You can close this tab and return to Pantheon.</p></body></html>"
    );
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = stream;
    stream
        .write_all(response.as_bytes())
        .map_err(|e| McpError::Io(format!("oauth callback write: {e}")))?;
    stream
        .flush()
        .map_err(|e| McpError::Io(format!("oauth callback flush: {e}")))
}

fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(HTTP_TIMEOUT))
        .max_redirects(5)
        .http_status_as_error(false)
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .new_agent()
}

fn get_json_opt(url: &str) -> Result<Option<Value>, McpError> {
    // Statuses never surface as errors on this agent (ureq 3): a 4xx is
    // a miss on the discovery probe, not a failure.
    let resp = http_agent()
        .get(url)
        .header("Accept", "application/json")
        .call()
        .map_err(|e| McpError::Io(format!("{url}: {e}")))?;
    if resp.status().as_u16() >= 400 {
        return Ok(None);
    }
    let v: Value = resp
        .into_body()
        .read_json()
        .map_err(|e| McpError::Protocol(format!("{url}: bad JSON: {e}")))?;
    Ok(Some(v))
}

fn origin(url: &str) -> Option<String> {
    let mut parts = url.splitn(4, '/');
    let scheme = parts.next()?.trim_end_matches(':');
    let _empty = parts.next()?;
    let host = parts.next()?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    if host.is_empty() {
        return None;
    }
    Some(format!("{scheme}://{host}"))
}

fn path_of(url: &str) -> &str {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);
    match rest.find('/') {
        Some(i) => &rest[i..],
        None => "",
    }
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn string_array(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn header_param(header: &str, name: &str) -> Option<String> {
    // `WWW-Authenticate` is a comma-separated auth-param list such as
    // `Bearer resource_metadata="https://..."`. Find the parameter name
    // case-insensitively, then take a quoted string (which may itself
    // contain commas) or a bare token.
    let needle = format!("{}=", name.to_ascii_lowercase());
    let lower = header.to_ascii_lowercase();
    let start = lower.find(&needle)? + needle.len();
    let rest = header.get(start..)?.trim_start();
    if let Some(rest) = rest.strip_prefix('"') {
        let end = rest.find('"')?;
        return Some(rest[..end].to_string());
    }
    let end = rest
        .find(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

fn random_state() -> String {
    use rand::RngExt;
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..32)
        .map(|_| CHARSET[rng.random_range(0..CHARSET.len())] as char)
        .collect()
}

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

fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((url_decode(k), url_decode(v)))
        })
        .collect()
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_and_path_keep_the_port_but_strip_the_path() {
        assert_eq!(
            origin("https://mcp.example.com:8443/a/b?x=1").as_deref(),
            Some("https://mcp.example.com:8443")
        );
        assert_eq!(path_of("https://mcp.example.com:8443/a/b?x=1"), "/a/b?x=1");
    }

    #[test]
    fn auth_header_param_is_quoted_and_case_insensitive() {
        let header = r#"Bearer resource_metadata="https://x/.well-known/oauth-protected-resource""#;
        assert_eq!(
            header_param(header, "resource_metadata").as_deref(),
            Some("https://x/.well-known/oauth-protected-resource")
        );
    }

    #[test]
    fn query_round_trip_handles_spaces_and_slashes() {
        let q = format!("scope={}&state={}", url_encode("a b/c"), url_encode("xyz"));
        let parsed = parse_query(&q);
        assert_eq!(parsed.get("scope").map(String::as_str), Some("a b/c"));
        assert_eq!(parsed.get("state").map(String::as_str), Some("xyz"));
    }

    #[test]
    fn token_store_keys_do_not_collide_on_punctuation() {
        let broker = SecretsBroker::new();
        let a = format!(
            "mcp_oauth_{}_{}",
            crate::manager::sanitize_segment("a-b"),
            0
        );
        let b = format!(
            "mcp_oauth_{}_{}",
            crate::manager::sanitize_segment("a_b"),
            0
        );
        assert_eq!(a, b, "the sanitized prefix alone is not unique");
        let store_a = TokenStore::new(&broker, "a-b");
        let store_b = TokenStore::new(&broker, "a_b");
        assert_ne!(store_a.prefix, store_b.prefix);
    }
}
