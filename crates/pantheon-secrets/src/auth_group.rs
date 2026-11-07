//! auth_group: oauth, logins, secure_forms, gateway_tokens, migration, dotenv, env in one file.

pub mod oauth {
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
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
        ///
        /// All-or-nothing: the previous set is snapshotted first, and any
        /// failure mid-save restores it (best-effort) before the error
        /// propagates. The old order wrote access, then refresh, then meta
        /// with no rollback, so a failure left a mixed token set - a new
        /// access token paired with an old refresh token and old metadata.
        pub fn save(&self, app: &str, token: &OAuthToken) -> Result<(), SecretsError> {
            provider(app)
                .ok_or_else(|| SecretsError::Invalid(format!("unknown oauth app {app:?}")))?;
            let (access_name, refresh_name, meta_name) = Self::names(app);
            let prev_access = self.broker.resolve(&access_name)?;
            let prev_refresh = self.broker.resolve(&refresh_name)?;
            let prev_meta = self.broker.resolve(&meta_name)?;
            let write = (|| -> Result<(), SecretsError> {
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
            })();
            if let Err(e) = write {
                // Restore the previous set; rollback failures are ignored
                // (the original error is the one reported) but attempted for
                // every record, not just the first.
                for (name, prev) in [
                    (&access_name, &prev_access),
                    (&refresh_name, &prev_refresh),
                    (&meta_name, &prev_meta),
                ] {
                    match prev {
                        Some(v) => {
                            let _ = self.broker.set(name, v.clone());
                        }
                        None => {
                            let _ = self.broker.delete(name);
                        }
                    }
                }
                return Err(e);
            }
            Ok(())
        }

        /// Load a token set, or `None` when the app was never connected.
        pub fn load(&self, app: &str) -> Result<Option<OAuthToken>, SecretsError> {
            provider(app)
                .ok_or_else(|| SecretsError::Invalid(format!("unknown oauth app {app:?}")))?;
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
            provider(app)
                .ok_or_else(|| SecretsError::Invalid(format!("unknown oauth app {app:?}")))?;
            let (access_name, refresh_name, meta_name) = Self::names(app);
            self.broker.delete(&access_name)?;
            self.broker.delete(&refresh_name)?;
            self.broker.delete(&meta_name)?;
            Ok(())
        }

        /// Connection status for display. Never carries token material.
        pub fn status(&self, app: &str) -> Result<OAuthStatus, SecretsError> {
            provider(app)
                .ok_or_else(|| SecretsError::Invalid(format!("unknown oauth app {app:?}")))?;
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
                token =
                    refresh_access_token_with(&http_agent(), prov, &client_id, refresh.expose())?;
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
    /// workspace standard, on by default in ureq 3) so sandboxed egress keeps
    /// working; tests build their own agent without a proxy and pass it to the
    /// `_with` variants. HTTP statuses are returned as responses, not errors,
    /// because every call site reads the error body.
    fn http_agent() -> ureq::Agent {
        http_agent_with(std::time::Duration::from_secs(30))
    }

    /// [`http_agent`] with an explicit overall budget. ureq 2 gave every
    /// agent an implicit 30s connect timeout; ureq 3 has none by default, so
    /// the deadline is pinned here: a wedged token endpoint must fail the
    /// exchange, not hang the session thread. Tests drive a short budget to
    /// pin exactly that contract.
    fn http_agent_with(timeout: std::time::Duration) -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .new_agent()
    }

    /// Parsed token endpoint response. Field names follow RFC 6749.
    #[derive(Debug)]
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
        let mut req = agent.post(token_url).header("Accept", "application/json");
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
                    req = req.header("Authorization", format!("Basic {creds}").as_str());
                }
            }
        }
        let mut resp = req
            .send_form(params.iter().copied())
            .map_err(|e| SecretsError::Backend(format!("{prov_id} token request failed: {e}")))?;
        if resp.status().as_u16() >= 400 {
            let code = resp.status().as_u16();
            let detail = resp.body_mut().read_to_string().unwrap_or_default();
            let detail: String = detail.chars().take(300).collect();
            return Err(SecretsError::Backend(format!(
                "{prov_id} token endpoint error {code}: {detail}"
            )));
        }
        let body: serde_json::Value = resp.body_mut().read_json().map_err(|e| {
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
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
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
        use rand::RngExt;
        const CHARSET: &[u8] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";
        let mut rng = rand::rng();
        let verifier: String = (0..64)
            .map(|_| CHARSET[rng.random_range(0..CHARSET.len())] as char)
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
        use rand::RngExt;
        const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        let mut rng = rand::rng();
        (0..32)
            .map(|_| CHARSET[rng.random_range(0..CHARSET.len())] as char)
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
    ///
    /// Bounded: `accept()` used to block forever, with the 300s timeout
    /// applying only after a connection arrived - so a user who abandoned
    /// the flow hung the caller indefinitely, contradicting the login
    /// flow's "blocks until user completes (or abandons)" contract. The
    /// whole wait (accept + stray connections) is now bounded by
    /// `timeout`. Stray connections (health checks, a second browser tab,
    /// a request without a matching code/state) are answered 400 and
    /// ignored instead of aborting the flow on the first one; only a real
    /// provider `error` response aborts immediately.
    fn wait_for_code(
        listener: &TcpListener,
        expected_state: &str,
        timeout: Duration,
    ) -> Result<String, SecretsError> {
        let deadline = Instant::now() + timeout;
        listener.set_nonblocking(true).map_err(SecretsError::from)?;
        loop {
            if Instant::now() >= deadline {
                return Err(SecretsError::Invalid(format!(
                    "timed out waiting for OAuth callback after {timeout:?}"
                )));
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    // Accepted sockets inherit non-blocking mode; reads
                    // below need blocking mode plus a read deadline that
                    // never extends past the overall deadline.
                    stream.set_nonblocking(false).map_err(SecretsError::from)?;
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    stream
                        .set_read_timeout(Some(remaining))
                        .map_err(SecretsError::from)?;
                    if let Some(code) = handle_callback(&stream, expected_state)? {
                        return Ok(code);
                    }
                    // Stray connection: keep waiting for the real callback.
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(SecretsError::from(e)),
            }
        }
    }

    /// Handle one accepted callback connection. `Ok(Some(code))` is the real
    /// callback; `Ok(None)` is a stray to ignore; `Err` is a real outcome
    /// (provider error, oversize head) that aborts the flow.
    fn handle_callback(
        stream: &TcpStream,
        expected_state: &str,
    ) -> Result<Option<String>, SecretsError> {
        // One small GET: cap the head so a stray connection cannot make us
        // buffer without bound.
        let mut head = Vec::new();
        {
            let mut reader = BufReader::new(stream);
            loop {
                let mut line = Vec::new();
                let n = reader
                    .read_until(b'\n', &mut line)
                    .map_err(SecretsError::from)?;
                if n == 0 {
                    break; // EOF or timeout: a stray, not a callback.
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
        if head.is_empty() {
            return Ok(None);
        }
        let head_str = String::from_utf8_lossy(&head);
        let request_line = head_str.lines().next().unwrap_or("");
        let target = request_line.split_whitespace().nth(1).unwrap_or("");
        let query = target.split('?').nth(1).unwrap_or("");
        let params = parse_query(query);
        if let Some(err) = params.get("error") {
            let _ = respond(stream, 400, "Authorization failed.");
            return Err(SecretsError::Invalid(format!(
                "provider returned error: {err}"
            )));
        }
        match (params.get("code"), params.get("state")) {
            (Some(code), Some(state)) if state == expected_state && !code.is_empty() => {
                let _ = respond(stream, 200, "Signed in.");
                Ok(Some(code.clone()))
            }
            _ => {
                let _ = respond(stream, 400, "Authorization failed.");
                Ok(None)
            }
        }
    }

    /// Run the RFC 8252 loopback authorization flow: open the provider's
    /// consent page, receive the authorization code on an ephemeral
    /// `127.0.0.1` listener, and exchange it for tokens. Blocks until the
    /// user completes (or abandons) the browser step, bounded by a 300s
    /// callback timeout. `client_id` is a public identifier, never a secret.
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
        let code = wait_for_code(&listener, &state, Duration::from_secs(300))?;
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
            // force_send_body unifies the builder typestate so the validated
            // method set can be built in one match; bodies without a body send
            // empty, which no API distinguishes from v2's `.call()`.
            let mut req = match method.as_str() {
                "GET" => agent.get(url).force_send_body(),
                "DELETE" => agent.delete(url).force_send_body(),
                "POST" => agent.post(url),
                "PUT" => agent.put(url),
                _ => agent.patch(url),
            }
            .header(
                "Authorization",
                format!("Bearer {}", token.expose()).as_str(),
            )
            .header("Accept", "application/json");
            for (k, v) in prov.extra_headers {
                req = req.header(*k, *v);
            }
            let mut resp = match body {
                Some(b) => req.header("Content-Type", "application/json").send(b),
                None => req.send_empty(),
            }
            .map_err(|e| SecretsError::Backend(format!("{app} request failed: {e}")))?;
            if resp.status().as_u16() >= 400 {
                let code = resp.status().as_u16();
                let detail = resp.body_mut().read_to_string().unwrap_or_default();
                let detail: String = detail.chars().take(300).collect();
                return Err(SecretsError::Backend(format!(
                    "{app} api error {code}: {detail}"
                )));
            }
            // The response body travels back through the tool-result path,
            // which the session scrubs against the redaction registry: even a
            // misbehaving API that echoed the token could not reach the model.
            resp.body_mut()
                .read_to_string()
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
            // No proxy: the fake endpoint is loopback, and the environment
            // proxy (on by default in ureq 3) would refuse the address.
            let agent = ureq::Agent::config_builder()
                .proxy(None)
                .http_status_as_error(false)
                .build()
                .new_agent();
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
            let agent = ureq::Agent::config_builder()
                .proxy(None)
                .http_status_as_error(false)
                .build()
                .new_agent();
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
            let agent = ureq::Agent::config_builder()
                .proxy(None)
                .http_status_as_error(false)
                .build()
                .new_agent();
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

        #[test]
        fn token_exchange_is_bounded_by_the_agent_timeout() {
            // The timeout contract this pins: every agent built by
            // `http_agent_with` carries a deadline, so an endpoint that
            // accepts and never answers fails the exchange instead of
            // hanging the caller forever (ureq 3 ships no default timeout;
            // ureq 2's implicit 30s connect bound is what this restores).
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            std::thread::spawn(move || {
                // Accept and HOLD the streams: dropping an accepted socket
                // closes it, which reads as an instant "peer disconnected"
                // instead of the wedged-server shape under test.
                let mut held = Vec::new();
                for _ in 0..4 {
                    match listener.accept() {
                        Ok((stream, _)) => held.push(stream),
                        Err(_) => break,
                    }
                }
            });
            let url = format!("http://127.0.0.1:{port}/token");
            let agent = http_agent_with(Duration::from_millis(300));
            let start = std::time::Instant::now();
            let err = post_token(
                &agent,
                "github",
                &url,
                TokenAuth::Pkce,
                "cid",
                &[("grant_type", "refresh_token")],
            )
            .unwrap_err();
            assert!(
                start.elapsed() >= Duration::from_millis(250),
                "error arrived before the budget expired; something other than \
                 the deadline failed (took {:?}, err: {err})",
                start.elapsed()
            );
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "wedged endpoint must fail within the budget, took {:?} (err: {err})",
                start.elapsed()
            );
        }
    }

    #[cfg(test)]
    mod callback_tests {
        use super::*;
        use crate::vault::{MemoryVault, SecretVault};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        /// No connection at all: the wait ends at the deadline instead of
        /// hanging forever (the old `accept()` had no timeout).
        #[test]
        fn wait_for_code_times_out_without_connections() {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let start = Instant::now();
            let err = wait_for_code(&listener, "state-1", Duration::from_millis(300)).unwrap_err();
            assert!(err.to_string().contains("timed out"), "err: {err}");
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "timeout must bound the wait, took {:?}",
                start.elapsed()
            );
        }

        /// A stray connection (no code) does not abort the flow: the real
        /// callback arriving afterwards still wins.
        #[test]
        fn stray_connection_does_not_abort_wait() {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let handle = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                // Stray: a request with no code/state.
                let mut stray = TcpStream::connect(("127.0.0.1", port)).unwrap();
                stray
                    .write_all(b"GET /other HTTP/1.1\r\nHost: x\r\n\r\n")
                    .unwrap();
                let mut buf = [0u8; 256];
                use std::io::Read as _;
                let _ = stray.read(&mut buf);
                drop(stray);
                std::thread::sleep(Duration::from_millis(100));
                // Real callback.
                let mut real = TcpStream::connect(("127.0.0.1", port)).unwrap();
                real.write_all(
                    b"GET /callback?code=code-abc&state=state-1 HTTP/1.1\r\nHost: x\r\n\r\n",
                )
                .unwrap();
                let mut buf = [0u8; 256];
                let _ = real.read(&mut buf);
            });
            let code = wait_for_code(&listener, "state-1", Duration::from_secs(10)).unwrap();
            assert_eq!(code, "code-abc");
            handle.join().unwrap();
        }

        /// A vault that fails refresh writes: a failed save restores the
        /// previous token set instead of leaving a mixed one.
        #[derive(Debug)]
        struct FailRefreshVault {
            inner: MemoryVault,
            fail: Arc<AtomicBool>,
        }

        impl SecretVault for FailRefreshVault {
            fn get(&self, name: &str) -> Result<Option<SecretValue>, SecretsError> {
                self.inner.get(name)
            }
            fn set(&self, name: &str, value: SecretValue) -> Result<(), SecretsError> {
                if self.fail.load(Ordering::SeqCst) && name.contains("refresh_token") {
                    return Err(SecretsError::Backend("refresh write failed".into()));
                }
                self.inner.set(name, value)
            }
            fn delete(&self, name: &str) -> Result<(), SecretsError> {
                self.inner.delete(name)
            }
            fn names(&self) -> Result<Vec<String>, SecretsError> {
                self.inner.names()
            }
        }

        fn token(access: &str, refresh: &str) -> OAuthToken {
            OAuthToken {
                access_token: SecretValue::new(access),
                refresh_token: Some(SecretValue::new(refresh)),
                expires_at: None,
                scopes: vec!["read:user".into()],
                token_type: "Bearer".into(),
            }
        }

        #[test]
        fn failed_save_restores_previous_token_set() {
            let fail = Arc::new(AtomicBool::new(false));
            let vault = FailRefreshVault {
                inner: MemoryVault::new(),
                fail: Arc::clone(&fail),
            };
            let broker = SecretsBroker::new()
                .with_vault(Box::new(vault))
                .with_env(crate::env::EnvVault::from_map(Vec::<(&str, &str)>::new()));
            let store = OAuthStore::new(&broker);
            store
                .save("github", &token("old-access", "old-refresh"))
                .unwrap();
            fail.store(true, Ordering::SeqCst);
            assert!(store
                .save("github", &token("new-access", "new-refresh"))
                .is_err());
            let loaded = store
                .load("github")
                .unwrap()
                .expect("previous set restored");
            assert_eq!(loaded.access_token.expose(), "old-access");
            assert_eq!(loaded.refresh_token.unwrap().expose(), "old-refresh");
            // And with the failure cleared, a fresh save lands whole.
            fail.store(false, Ordering::SeqCst);
            store
                .save("github", &token("new-access", "new-refresh"))
                .unwrap();
            let loaded = store.load("github").unwrap().expect("saved token");
            assert_eq!(loaded.access_token.expose(), "new-access");
        }
    }
}

pub mod logins {
    //! Website-login credentials for browser sessions.
    //!
    //! A login is `(id, site, username, password)`. The password lives in a
    //! [`SecretsBroker`] vault (`logins.env` inside the data dir - deliberately
    //! NOT `.env`, so login passwords never surface in the `.env` key
    //! manager); the metadata (site, username, the secret's broker name) lives
    //! in `logins.json` next to it. The password is only ever exposed through
    //! [`LoginStore::password_for`], for server-side use by the browser
    //! session. API responses use [`LoginCredential::masked`]: the password is
    //! always the `••••` placeholder, never secret material, not even partial.

    use crate::{DotenvVault, EnvVault, SecretValue, SecretsBroker, SecretsError};
    use std::path::{Path, PathBuf};

    /// Broker vault file holding login passwords (`<data_dir>/logins.env`).
    pub const LOGINS_FILE: &str = "logins.env";
    /// Metadata file holding login site/username records (`<data_dir>/logins.json`).
    pub const LOGINS_META_FILE: &str = "logins.json";

    /// Masked placeholder returned in place of every password in API
    /// responses. Never the real value, not even a prefix.
    pub const MASKED: &str = "••••";

    /// One website login: site + username are metadata; the password lives in
    /// the broker under [`LoginCredential::secret_name`] and is never stored
    /// on this struct.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
    pub struct LoginCredential {
        /// Stable id, derived from the site at creation (`github-com-3f9a`).
        pub id: String,
        /// Site this login is for (`github.com`, `https://x.com/login`
        /// stored as given, matched by substring at login time).
        pub site: String,
        /// Login username / email.
        pub username: String,
        /// Broker secret name holding the password.
        pub secret_name: String,
    }

    impl LoginCredential {
        /// The API-safe view: `site`, `username`, and the masked password
        /// placeholder. The real password never leaves the server.
        pub fn masked(&self) -> serde_json::Value {
            serde_json::json!({
                "id": self.id,
                "site": self.site,
                "username": self.username,
                "password": MASKED,
            })
        }
    }

    /// CRUD over the login store. Backed by a broker whose only vault is
    /// `logins.env` (no process-env fallback, so `names`/`resolve` reflect
    /// the file and nothing else) plus the `logins.json` metadata file.
    ///
    /// The store is deliberately dumb about the browser: it hands the
    /// password to server-side callers via [`LoginStore::password_for`]; the
    /// login *flow* (fill + submit) belongs to the browser tool surface.
    pub struct LoginStore {
        broker: SecretsBroker,
        meta_path: PathBuf,
    }

    impl LoginStore {
        /// Open the store rooted at `data_dir` (`logins.env` + `logins.json`
        /// live there). Creates nothing until the first write.
        pub fn open(data_dir: impl AsRef<Path>) -> Self {
            let data_dir = data_dir.as_ref().to_path_buf();
            let broker = SecretsBroker::new()
                .with_vault(Box::new(DotenvVault::new_file(&data_dir, LOGINS_FILE)))
                .with_env(EnvVault::from_map(Vec::<(&str, &str)>::new()));
            Self {
                broker,
                meta_path: data_dir.join(LOGINS_META_FILE),
            }
        }

        /// Broker secret name for a login id. Uppercase + digits + `_` so it
        /// passes dotenv key validation.
        fn secret_name_for(id: &str) -> String {
            format!(
                "PANTHEON_LOGIN_{}",
                id.to_ascii_uppercase().replace('-', "_")
            )
        }

        /// Read the metadata file. Missing/empty is a clean empty list;
        /// present-but-unparseable is an error - never silently treated as
        /// empty, because the next write would then overwrite (wipe) the
        /// corrupt file.
        fn read_meta(&self) -> Result<Vec<LoginCredential>, SecretsError> {
            let text = std::fs::read_to_string(&self.meta_path).unwrap_or_default();
            if text.trim().is_empty() {
                return Ok(Vec::new());
            }
            serde_json::from_str(&text).map_err(|e| {
                SecretsError::Invalid(format!(
                    "corrupt {}: {e} (not overwritten; repair or restore the file)",
                    LOGINS_META_FILE
                ))
            })
        }

        fn write_meta(&self, creds: &[LoginCredential]) -> Result<(), SecretsError> {
            let text = serde_json::to_string_pretty(creds)
                .map_err(|e| SecretsError::Invalid(format!("serialize logins.json: {e}")))?;
            if let Some(parent) = self.meta_path.parent() {
                std::fs::create_dir_all(parent).map_err(SecretsError::Io)?;
            }
            // Atomic-ish: temp file + rename, owner-only like the vault files.
            // Create with the private mode rather than writing at the umask
            // default and chmod'ing after: a `fs::write` file is briefly
            // world-readable, and ignoring a failed `set_permissions` would leave
            // credentials readable to everyone on the box. Match `filevault`'s
            // pattern: fail the write if the mode cannot be set.
            let tmp = self.meta_path.with_extension("json.tmp");
            {
                use std::io::Write as _;
                use std::os::unix::fs::OpenOptionsExt;
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o600)
                    .open(&tmp)
                    .map_err(SecretsError::Io)?;
                f.write_all(text.as_bytes()).map_err(SecretsError::Io)?;
                f.sync_all().map_err(SecretsError::Io)?;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
                    .map_err(SecretsError::Io)?;
            }
            std::fs::rename(&tmp, &self.meta_path).map_err(SecretsError::Io)?;
            Ok(())
        }

        /// All logins, metadata only (passwords are never in the metadata).
        /// A corrupt metadata file is an error, not an empty list.
        pub fn list(&self) -> Result<Vec<LoginCredential>, SecretsError> {
            self.read_meta()
        }

        /// Find one login by id.
        pub fn get(&self, id: &str) -> Result<Option<LoginCredential>, SecretsError> {
            Ok(self.read_meta()?.into_iter().find(|c| c.id == id))
        }

        /// Resolve the password for server-side use (browser login flow).
        /// `None` = unknown id or no password stored.
        pub fn password_for(&self, id: &str) -> Result<Option<SecretValue>, SecretsError> {
            let Some(cred) = self.get(id)? else {
                return Ok(None);
            };
            self.broker.resolve(&cred.secret_name)
        }

        /// Create a login. The id is derived from the site; collisions get a
        /// numeric suffix. Fails on empty site/username/password.
        ///
        /// Metadata-first: the `logins.json` row is written before the vault
        /// password, and a vault failure rolls the row back out. The old
        /// order (vault first) orphaned a password with no row whenever the
        /// metadata write failed - a secret the store could never name or
        /// delete again.
        pub fn create(
            &self,
            site: &str,
            username: &str,
            password: &str,
        ) -> Result<LoginCredential, SecretsError> {
            let site = site.trim();
            let username = username.trim();
            if site.is_empty() {
                return Err(SecretsError::Invalid("site must not be empty".into()));
            }
            if username.is_empty() {
                return Err(SecretsError::Invalid("username must not be empty".into()));
            }
            if password.is_empty() {
                return Err(SecretsError::Invalid("password must not be empty".into()));
            }
            if password.contains('\n') || password.contains('\r') {
                return Err(SecretsError::Invalid("password must be single-line".into()));
            }
            let mut creds = self.read_meta()?;
            let slug: String = site
                .to_ascii_lowercase()
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect::<String>()
                .split('-')
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join("-");
            let slug = slug.chars().take(32).collect::<String>();
            let base = if slug.is_empty() {
                "login".into()
            } else {
                slug
            };
            let mut id = base.clone();
            let mut n = 2;
            while creds.iter().any(|c| c.id == id) {
                id = format!("{base}-{n}");
                n += 1;
            }
            let secret_name = Self::secret_name_for(&id);
            let cred = LoginCredential {
                id,
                site: site.to_string(),
                username: username.to_string(),
                secret_name,
            };
            creds.push(cred.clone());
            self.write_meta(&creds)?;
            // Vault second: on failure the row is rolled back, so a failed
            // create never leaves either half behind.
            if let Err(e) = self
                .broker
                .set(&cred.secret_name, SecretValue::new(password))
            {
                creds.pop();
                let _ = self.write_meta(&creds);
                return Err(e);
            }
            Ok(cred)
        }

        /// Update username and/or password. At least one must be given.
        /// Password updates are write-only (the old value is never readable).
        ///
        /// Metadata-first, like [`create`](Self::create): the metadata row
        /// is written before the vault password, and a vault failure
        /// restores the previous metadata and reports the error - never a
        /// success for a half-applied pair. One asymmetry remains by
        /// construction: the old password value is unreadable, so a vault
        /// write that *succeeds* cannot be undone if a later step fails;
        /// nothing after the vault write can fail except the rollback
        /// itself, which is best-effort.
        pub fn update(
            &self,
            id: &str,
            site: Option<&str>,
            username: Option<&str>,
            password: Option<&str>,
        ) -> Result<LoginCredential, SecretsError> {
            if site.map(str::trim).unwrap_or("").is_empty()
                && username.map(str::trim).unwrap_or("").is_empty()
                && password.unwrap_or("").is_empty()
            {
                return Err(SecretsError::Invalid(
                    "nothing to update: supply site, username and/or password".into(),
                ));
            }
            let mut creds = self.read_meta()?;
            let pos = creds
                .iter()
                .position(|c| c.id == id)
                .ok_or_else(|| SecretsError::Invalid(format!("unknown login '{id}'")))?;
            let original = creds[pos].clone();
            if let Some(s) = site {
                let s = s.trim();
                if s.is_empty() {
                    return Err(SecretsError::Invalid("site must not be empty".into()));
                }
                creds[pos].site = s.to_string();
            }
            if let Some(u) = username {
                let u = u.trim();
                if u.is_empty() {
                    return Err(SecretsError::Invalid("username must not be empty".into()));
                }
                creds[pos].username = u.to_string();
            }
            let new_password = match password {
                Some(p) => {
                    if p.is_empty() {
                        return Err(SecretsError::Invalid("password must not be empty".into()));
                    }
                    if p.contains('\n') || p.contains('\r') {
                        return Err(SecretsError::Invalid("password must be single-line".into()));
                    }
                    Some(p)
                }
                None => None,
            };
            // Metadata first: if this write fails, the vault is untouched
            // and the call fails with nothing changed. The old order set
            // the vault password first, so a metadata failure left the
            // password changed while the update reported failure.
            self.write_meta(&creds)?;
            if let Some(p) = new_password {
                if let Err(e) = self
                    .broker
                    .set(&creds[pos].secret_name, SecretValue::new(p))
                {
                    // Roll the metadata back: the caller was told the update
                    // failed, so the row must not claim the new values.
                    // (The vault password itself cannot be restored - the
                    // old value is write-only - but a retry with the same
                    // password converges, and the row no longer lies.)
                    let mut rollback = self.read_meta().unwrap_or_default();
                    if let Some(rpos) = rollback.iter().position(|c| c.id == id) {
                        rollback[rpos] = original;
                        let _ = self.write_meta(&rollback);
                    }
                    return Err(e);
                }
            }
            Ok(creds[pos].clone())
        }

        /// Delete a login: metadata row and its vault password both go.
        /// Unknown id is a no-op returning `false`; deleted returns `true`.
        ///
        /// Metadata-first: the row is removed from `logins.json` before the
        /// vault password is deleted, and a vault-delete failure restores
        /// the row and propagates the error. The old order deleted the
        /// password first, so a metadata failure left a row whose password
        /// was already gone. (A crash between the metadata write and the
        /// vault delete orphans the password instead - an unnamed secret
        /// the store can no longer reach, but never a dangling row.)
        pub fn delete(&self, id: &str) -> Result<bool, SecretsError> {
            let mut creds = self.read_meta()?;
            let Some(pos) = creds.iter().position(|c| c.id == id) else {
                return Ok(false);
            };
            let cred = creds.remove(pos);
            self.write_meta(&creds)?;
            if let Err(e) = self.broker.delete(&cred.secret_name) {
                // Restore the row: the delete was reported failed, so the
                // login must still be whole.
                let mut rollback = self.read_meta().unwrap_or_default();
                rollback.push(cred);
                let _ = self.write_meta(&rollback);
                return Err(e);
            }
            Ok(true)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn scratch(tag: &str) -> PathBuf {
            let dir = std::env::temp_dir()
                .join(format!("pantheon-logins-test-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        /// Block metadata writes by parking a directory on the tmp path the
        /// atomic writer needs (`logins.json.tmp`).
        fn block_meta(dir: &Path) {
            std::fs::create_dir_all(dir.join("logins.json.tmp")).unwrap();
        }

        fn unblock_meta(dir: &Path) {
            let _ = std::fs::remove_dir_all(dir.join("logins.json.tmp"));
        }

        /// Happy path still works end to end after the reorder.
        #[test]
        fn create_update_delete_round_trip() {
            let dir = scratch("roundtrip");
            let store = LoginStore::open(&dir);
            let cred = store.create("example.com", "user", "pw-one").unwrap();
            assert_eq!(
                store.password_for(&cred.id).unwrap().unwrap().expose(),
                "pw-one"
            );
            store
                .update(&cred.id, None, Some("user2"), Some("pw-two"))
                .unwrap();
            assert_eq!(
                store.password_for(&cred.id).unwrap().unwrap().expose(),
                "pw-two"
            );
            assert!(store.delete(&cred.id).unwrap());
            assert!(store.get(&cred.id).unwrap().is_none());
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// A failed create leaves no orphaned vault password: metadata
        /// goes first, so when it fails the vault write never runs.
        #[test]
        fn failed_create_leaves_no_orphan_password() {
            let dir = scratch("create-fail");
            let store = LoginStore::open(&dir);
            block_meta(&dir);
            assert!(store.create("example.com", "user", "pw-one").is_err());
            unblock_meta(&dir);
            let vault_text = std::fs::read_to_string(dir.join("logins.env")).unwrap_or_default();
            assert!(
                !vault_text.contains("PANTHEON_LOGIN_EXAMPLE_COM"),
                "no orphan password may be stored: {vault_text}"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// A failed update leaves the old password in place: metadata goes
        /// first, so when it fails the vault password is never touched.
        #[test]
        fn failed_update_keeps_old_password() {
            let dir = scratch("update-fail");
            let store = LoginStore::open(&dir);
            let cred = store.create("example.com", "user", "pw-old").unwrap();
            block_meta(&dir);
            assert!(store.update(&cred.id, None, None, Some("pw-new")).is_err());
            unblock_meta(&dir);
            assert_eq!(
                store.password_for(&cred.id).unwrap().unwrap().expose(),
                "pw-old",
                "a failed update must not change the password"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }

        /// A failed delete leaves the login whole: row and password both
        /// still present, not a row whose password is gone.
        #[test]
        fn failed_delete_keeps_login_whole() {
            let dir = scratch("delete-fail");
            let store = LoginStore::open(&dir);
            let cred = store.create("example.com", "user", "pw-one").unwrap();
            block_meta(&dir);
            assert!(store.delete(&cred.id).is_err());
            unblock_meta(&dir);
            assert!(store.get(&cred.id).unwrap().is_some(), "row must survive");
            assert_eq!(
                store.password_for(&cred.id).unwrap().unwrap().expose(),
                "pw-one",
                "password must survive a failed delete"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

pub mod secure_forms {
    //! Secure forms: masked secret entry and management on top of the broker.
    //!
    //! This is Pantheon's equivalent of a secure vault entry form. It adds no
    //! second secret store: values live only in the broker's durable vaults,
    //! exactly as before. What it adds is:
    //!
    //! * A metadata sidecar (`secure-forms.json` under the data dir) holding
    //!   only names, scopes and last-updated timestamps. It never holds values,
    //!   and it is written with 0600 permissions.
    //! * A caller gate: only a live user session may save, rotate or delete.
    //!   Delegated child agents and plugins are refused outright, so a
    //!   compromised or over-eager delegate cannot harvest or replace keys.
    //! * Verification on save: a harmless check (provider models list, bot
    //!   `getMe`, or a local shape check) that reports success or failure
    //!   without ever echoing the value, not even in an error string.
    //! * Config references: which config paths name a secret, so rotation
    //!   never requires hand-editing `config.toml` — config keeps naming the
    //!   secret, and the broker resolves the current value.
    //!
    //! Failure is always closed: when the broker has no durable vault, or the
    //! metadata file cannot be written, `save` returns an error and nothing
    //! is written anywhere else. There is no fallback to `config.toml`, to a
    //! log, or to the transcript.

    use crate::broker::SecretsBroker;
    use crate::error::SecretsError;
    use crate::value::SecretValue;
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// What a secret is for. The scope picks the default verification check
    /// and is shown in listings; it never affects where the value is stored.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum SecretScope {
        /// A model/provider API key (built-in or custom provider).
        Provider,
        /// A secret an MCP server needs (header token, env value).
        Mcp,
        /// The local browser vault key (`GSD_BROWSER_VAULT_KEY`).
        BrowserVault,
        /// A chat bot token (Telegram-style `getMe` verification).
        BotToken,
        /// Anything else the user wants to keep in the broker.
        Custom,
    }

    impl SecretScope {
        pub fn as_str(&self) -> &'static str {
            match self {
                SecretScope::Provider => "provider",
                SecretScope::Mcp => "mcp",
                SecretScope::BrowserVault => "browser_vault",
                SecretScope::BotToken => "bot_token",
                SecretScope::Custom => "custom",
            }
        }

        pub fn parse(s: &str) -> Option<Self> {
            match s.to_ascii_lowercase().as_str() {
                "provider" | "model" | "api_key" => Some(SecretScope::Provider),
                "mcp" => Some(SecretScope::Mcp),
                "browser_vault" | "browser" | "gsd" => Some(SecretScope::BrowserVault),
                "bot_token" | "telegram" | "bot" => Some(SecretScope::BotToken),
                "custom" | "other" => Some(SecretScope::Custom),
                _ => None,
            }
        }

        /// Scopes in the order a form should offer them.
        pub fn all() -> &'static [SecretScope] {
            &[
                SecretScope::Provider,
                SecretScope::Mcp,
                SecretScope::BrowserVault,
                SecretScope::BotToken,
                SecretScope::Custom,
            ]
        }
    }

    /// Who is asking. Only a live user session may manage secrets.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum SecretCaller {
        /// The user, in their own TUI or dashboard session.
        UserSession,
        /// A delegated child agent. Refused, always.
        Delegate,
        /// A plugin or extension. Refused, always.
        Plugin,
    }

    impl SecretCaller {
        fn check(self) -> Result<(), SecretsError> {
            match self {
                SecretCaller::UserSession => Ok(()),
                SecretCaller::Delegate => Err(SecretsError::Unsupported(
                    "secret management is refused for delegated agents; only the user session may manage secrets".into(),
                )),
                SecretCaller::Plugin => Err(SecretsError::Unsupported(
                    "secret management is refused for plugins; only the user session may manage secrets".into(),
                )),
            }
        }
    }

    /// Outcome of a verification check. Reasons never contain the value.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum VerifyOutcome {
        /// A live or local check passed.
        Verified,
        /// A check ran and the provider rejected the value (status only).
        Failed(String),
        /// No check exists for this secret, or it could not run.
        Unavailable(String),
    }

    /// A verification check. Implementations must never put the value, or any
    /// part of it, into an outcome, an error, or a log line.
    pub trait Verifier {
        fn verify(&self, value: &SecretValue) -> VerifyOutcome;
    }

    /// Credential diagnosis for a verification failure, mirroring
    /// `pantheon_providers::credential_error::classify` (this crate
    /// cannot depend on pantheon-providers: providers depends on
    /// secrets, so the mappings live here too; keep them in sync).
    /// The diagnosis is static text only, never the body, so no
    /// credential can pass through it.
    fn credential_diagnosis(code: u16, body: &str) -> Option<&'static str> {
        if body.contains("error code: 1010") {
            return Some("Blocked at Cloudflare's edge by Browser Integrity Check: the request never reached the provider's auth layer. Do NOT replace the key. Fix: send a real User-Agent header.");
        }
        match code {
            401 => Some("Key missing, invalid, or revoked. Check the key in the provider account; replace it if it was revoked or rotated."),
            403 => Some("Key accepted but not permitted for this endpoint or model. Check permissions, scope, and plan; do not assume the key is wrong."),
            _ => None,
        }
    }

    /// Telegram credential diagnosis, mirroring
    /// `pantheon_providers::credential_error::classify_telegram`.
    fn telegram_diagnosis(code: u16) -> Option<&'static str> {
        match code {
            401 => Some("Bot token wrong or revoked. Check the token in BotFather; replace it if it was revoked or rotated."),
            403 => Some("Bot is blocked by the user, or is not allowed in that chat, or lacks permission there."),
            404 => Some("Token shape is wrong: Telegram tokens are <digits>:<letters>. Check the token format, not just its value."),
            _ => None,
        }
    }

    /// Branded User-Agent for verification requests, matching the
    /// provider transport: edge checks rejecting default agents must
    /// not block a save-time check.
    fn user_agent() -> String {
        format!("Pantheon/{}", env!("CARGO_PKG_VERSION"))
    }

    /// Verifier for OpenAI-compatible provider keys: `GET {base_url}/models`
    /// with a bearer token. On failure the HTTP status plus the
    /// credential diagnosis (never the body, never the value) is reported.
    pub struct ProviderModelsVerifier {
        pub base_url: String,
    }

    impl Verifier for ProviderModelsVerifier {
        fn verify(&self, value: &SecretValue) -> VerifyOutcome {
            let url = format!("{}/models", self.base_url.trim_end_matches('/'));
            let agent = ureq::Agent::config_builder()
                .timeout_global(Some(std::time::Duration::from_secs(15)))
                .http_status_as_error(false)
                .build()
                .new_agent();
            match agent
                .get(&url)
                .header("Authorization", format!("Bearer {}", value.expose()))
                .header("User-Agent", user_agent().as_str())
                .call()
            {
                Ok(resp) if (200..300).contains(&resp.status().as_u16()) => VerifyOutcome::Verified,
                Ok(mut resp) => {
                    let code = resp.status().as_u16();
                    let body: String = resp
                        .body_mut()
                        .read_to_string()
                        .unwrap_or_default()
                        .chars()
                        .take(300)
                        .collect();
                    let mut message = format!("provider returned HTTP {code}");
                    if let Some(d) = credential_diagnosis(code, &body) {
                        message.push_str(&format!(" [credential: {d}]"));
                    }
                    VerifyOutcome::Failed(message)
                }
                Err(_) => VerifyOutcome::Failed("provider could not be reached".to_string()),
            }
        }
    }

    /// Verifier for bot tokens: Telegram-style `getMe`. The token travels only
    /// in the request path at the execution boundary and is never logged or
    /// included in any outcome.
    pub struct BotTokenVerifier;

    impl Verifier for BotTokenVerifier {
        fn verify(&self, value: &SecretValue) -> VerifyOutcome {
            let url = format!("https://api.telegram.org/bot{}/getMe", value.expose());
            let agent = ureq::Agent::config_builder()
                .timeout_global(Some(std::time::Duration::from_secs(15)))
                .http_status_as_error(false)
                .build()
                .new_agent();
            match agent
                .get(&url)
                .header("User-Agent", user_agent().as_str())
                .call()
            {
                Ok(resp) if (200..300).contains(&resp.status().as_u16()) => VerifyOutcome::Verified,
                Ok(resp) => {
                    let code = resp.status().as_u16();
                    let mut message = format!("bot API returned HTTP {code}");
                    if let Some(d) = telegram_diagnosis(code) {
                        message.push_str(&format!(" [credential: {d}]"));
                    }
                    VerifyOutcome::Failed(message)
                }
                Err(_) => VerifyOutcome::Failed("bot API could not be reached".to_string()),
            }
        }
    }

    /// Local shape check only: non-empty, and at least `min_len` bytes.
    /// Used for browser vault keys and MCP secrets, where no harmless live
    /// endpoint exists. This proves shape, not validity.
    pub struct LocalVerifier {
        pub min_len: usize,
    }

    impl Verifier for LocalVerifier {
        fn verify(&self, value: &SecretValue) -> VerifyOutcome {
            if value.is_empty() {
                VerifyOutcome::Failed("secret is empty".to_string())
            } else if value.len() < self.min_len {
                VerifyOutcome::Failed(format!(
                    "secret is shorter than {} characters",
                    self.min_len
                ))
            } else {
                VerifyOutcome::Verified
            }
        }
    }

    /// Default verifier for a scope. Provider and bot-token scopes need a
    /// caller-supplied endpoint, so this returns `None` for them; callers pass
    /// an explicit verifier instead.
    pub fn default_verifier(scope: SecretScope) -> Option<Box<dyn Verifier>> {
        match scope {
            SecretScope::BrowserVault => Some(Box::new(LocalVerifier { min_len: 16 })),
            SecretScope::Mcp => Some(Box::new(LocalVerifier { min_len: 1 })),
            SecretScope::BotToken => Some(Box::new(BotTokenVerifier)),
            SecretScope::Provider | SecretScope::Custom => None,
        }
    }

    /// One listed secret: name, scope and last update. Never a value.
    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct SecretEntry {
        pub name: String,
        pub scope: SecretScope,
        /// Unix seconds of the last save/rotation, or 0 when unknown.
        pub updated_at: u64,
        /// Whether the broker currently resolves the name.
        pub present: bool,
    }

    #[derive(Debug, Default, Serialize, Deserialize)]
    struct MetadataFile {
        entries: BTreeMap<String, MetadataEntry>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct MetadataEntry {
        scope: SecretScope,
        updated_at: u64,
    }

    /// Result of saving a secret: the value is stored, and the verification
    /// outcome is reported separately so a failed check never blocks the
    /// user's ability to retry or clear.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct SaveOutcome {
        pub verification: VerifyOutcome,
    }

    /// The secure-forms store: broker for values, sidecar for metadata.
    pub struct SecureFormStore<'a> {
        broker: &'a SecretsBroker,
        metadata_path: PathBuf,
    }

    fn now_unix() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn load_metadata(path: &Path) -> MetadataFile {
        let Ok(text) = std::fs::read_to_string(path) else {
            return MetadataFile::default();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    fn store_metadata(path: &Path, meta: &MetadataFile) -> Result<(), SecretsError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                SecretsError::Backend(format!(
                    "secure-forms metadata dir could not be created: {e}"
                ))
            })?;
        }
        let text = serde_json::to_string_pretty(meta).map_err(|e| {
            SecretsError::Backend(format!(
                "secure-forms metadata could not be serialised: {e}"
            ))
        })?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).map_err(|e| {
            SecretsError::Backend(format!("secure-forms metadata could not be written: {e}"))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, path).map_err(|e| {
            SecretsError::Backend(format!("secure-forms metadata could not be stored: {e}"))
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(())
    }

    impl<'a> SecureFormStore<'a> {
        /// Open against a broker and a data dir. The metadata file is created
        /// lazily on first save; opening never fails on a missing file.
        pub fn new(broker: &'a SecretsBroker, data_dir: &Path) -> Self {
            Self {
                broker,
                metadata_path: data_dir.join("secure-forms.json"),
            }
        }

        pub fn metadata_path(&self) -> &Path {
            &self.metadata_path
        }

        /// Save a secret via the broker, then verify it. The value is stored
        /// before verification runs, so a failed check leaves the user with
        /// retry and clear options instead of losing the entry. On a broker
        /// failure nothing is recorded and the metadata file is untouched:
        /// fail closed, never fall back to config or a log.
        pub fn save(
            &self,
            caller: SecretCaller,
            name: &str,
            scope: SecretScope,
            value: SecretValue,
            verifier: Option<&dyn Verifier>,
        ) -> Result<SaveOutcome, SecretsError> {
            caller.check()?;
            if value.is_empty() {
                return Err(SecretsError::Unsupported(
                    "secret value must not be empty".into(),
                ));
            }
            self.broker.set(name, value.clone())?;
            // Register for redaction so any later echo is scrubbed. Best
            // effort: a registration failure must not un-save the secret.
            let _ = self.broker.inject(name);
            let mut meta = load_metadata(&self.metadata_path);
            meta.entries.insert(
                name.to_string(),
                MetadataEntry {
                    scope,
                    updated_at: now_unix(),
                },
            );
            // Metadata failure after a broker write is still an error, and the
            // caller is told; the value itself is in the broker, not lost.
            store_metadata(&self.metadata_path, &meta)?;
            let verification = match verifier {
                Some(v) => v.verify(&value),
                None => match default_verifier(scope) {
                    Some(v) => v.verify(&value),
                    None => VerifyOutcome::Unavailable(
                        "no verification check for this secret type".to_string(),
                    ),
                },
            };
            Ok(SaveOutcome { verification })
        }

        /// Replace the value of an existing secret. Rotating a name that does
        /// not exist is an error: use `save` for a new secret. Config entries
        /// keep naming the secret, so no config edit is needed.
        pub fn rotate(
            &self,
            caller: SecretCaller,
            name: &str,
            value: SecretValue,
            verifier: Option<&dyn Verifier>,
        ) -> Result<SaveOutcome, SecretsError> {
            caller.check()?;
            let exists = self.broker.resolve(name)?.is_some()
                || load_metadata(&self.metadata_path)
                    .entries
                    .contains_key(name);
            if !exists {
                return Err(SecretsError::Unsupported(format!(
                    "no secret named {name}; save it first"
                )));
            }
            let scope = load_metadata(&self.metadata_path)
                .entries
                .get(name)
                .map(|e| e.scope)
                .unwrap_or(SecretScope::Custom);
            self.save(caller, name, scope, value, verifier)
        }

        /// Delete a secret from the broker and drop its metadata.
        pub fn delete(&self, caller: SecretCaller, name: &str) -> Result<(), SecretsError> {
            caller.check()?;
            self.broker.delete(name)?;
            let mut meta = load_metadata(&self.metadata_path);
            if meta.entries.remove(name).is_some() {
                store_metadata(&self.metadata_path, &meta)?;
            }
            Ok(())
        }

        /// List secrets by name, scope and last update. Values are never
        /// included. Names the broker holds but the sidecar has never seen
        /// (saved before secure forms, or by another path) appear with scope
        /// `custom` and unknown update time, so the list is never silently
        /// partial.
        pub fn list(&self) -> Vec<SecretEntry> {
            let meta = load_metadata(&self.metadata_path);
            let mut entries: BTreeMap<String, SecretEntry> = BTreeMap::new();
            for (name, m) in &meta.entries {
                let present = self.broker.resolve(name).ok().flatten().is_some();
                entries.insert(
                    name.clone(),
                    SecretEntry {
                        name: name.clone(),
                        scope: m.scope,
                        updated_at: m.updated_at,
                        present,
                    },
                );
            }
            for name in self.broker.names() {
                entries.entry(name.clone()).or_insert(SecretEntry {
                    name,
                    scope: SecretScope::Custom,
                    updated_at: 0,
                    present: true,
                });
            }
            entries.into_values().collect()
        }

        /// Config paths that reference each secret name, from parsed TOML
        /// text. A path is recorded when a `key_env` / `api_key_env` leaf
        /// names the secret, or when a `{source, name}` table names it.
        /// Other string leaves (provider ids, model ids, titles) are not
        /// references. Unparsable config yields no references, never an error.
        pub fn references(config_text: &str) -> BTreeMap<String, Vec<String>> {
            let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
            let Ok(doc) = toml::from_str::<toml::Value>(config_text) else {
                return out;
            };
            fn walk(prefix: &str, v: &toml::Value, out: &mut BTreeMap<String, Vec<String>>) {
                match v {
                    toml::Value::Table(t) => {
                        if let Some(name) = t
                            .get("name")
                            .and_then(|n| n.as_str())
                            .filter(|_| t.contains_key("source"))
                        {
                            out.entry(name.to_string())
                                .or_default()
                                .push(prefix.to_string());
                            return;
                        }
                        for (k, child) in t {
                            let path = if prefix.is_empty() {
                                k.clone()
                            } else {
                                format!("{prefix}.{k}")
                            };
                            walk(&path, child, out);
                        }
                    }
                    toml::Value::String(s) => {
                        // Only env-name leaves are references: key_env and
                        // api_key_env. A bare string leaf (provider id, model
                        // id, title) naming a word is not a secret reference.
                        let leaf = prefix.rsplit('.').next().unwrap_or(prefix);
                        if leaf == "key_env" || leaf == "api_key_env" {
                            out.entry(s.clone()).or_default().push(prefix.to_string());
                        }
                    }
                    toml::Value::Array(items) => {
                        for (i, item) in items.iter().enumerate() {
                            walk(&format!("{prefix}[{i}]"), item, out);
                        }
                    }
                    _ => {}
                }
            }
            walk("", &doc, &mut out);
            // Only keep names that look like secret names; drop every other
            // string leaf (titles, model ids) so callers get references, not
            // a dump of the config.
            out.retain(|_, paths| !paths.is_empty());
            out
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::vault::MemoryVault;

        struct OkVerifier;
        impl Verifier for OkVerifier {
            fn verify(&self, _value: &SecretValue) -> VerifyOutcome {
                VerifyOutcome::Verified
            }
        }

        struct FailVerifier;
        impl Verifier for FailVerifier {
            fn verify(&self, _value: &SecretValue) -> VerifyOutcome {
                VerifyOutcome::Failed("provider returned HTTP 401".to_string())
            }
        }

        fn broker() -> SecretsBroker {
            SecretsBroker::new().with_vault(Box::new(MemoryVault::new()))
        }

        fn store<'a>(broker: &'a SecretsBroker, dir: &Path) -> SecureFormStore<'a> {
            SecureFormStore::new(broker, dir)
        }

        #[test]
        fn save_round_trips_through_broker_and_lists_metadata_only() {
            let dir = std::env::temp_dir().join("pantheon-secure-forms-test-1");
            let _ = std::fs::remove_dir_all(&dir);
            let b = broker();
            let s = store(&b, &dir);
            let out = s
                .save(
                    SecretCaller::UserSession,
                    "TEST_PROVIDER_KEY",
                    SecretScope::Provider,
                    SecretValue::new("test-value-123"),
                    Some(&OkVerifier),
                )
                .expect("save");
            assert_eq!(out.verification, VerifyOutcome::Verified);
            assert_eq!(
                b.resolve("TEST_PROVIDER_KEY").unwrap().unwrap().expose(),
                "test-value-123"
            );
            let listed = s.list();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].name, "TEST_PROVIDER_KEY");
            assert_eq!(listed[0].scope, SecretScope::Provider);
            assert!(listed[0].updated_at > 0);
            assert!(listed[0].present);
            // Metadata sidecar holds names and scopes, never the value.
            let meta = std::fs::read_to_string(dir.join("secure-forms.json")).unwrap();
            assert!(!meta.contains("test-value-123"));
            assert!(meta.contains("TEST_PROVIDER_KEY"));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn delegate_and_plugin_callers_are_refused() {
            let dir = std::env::temp_dir().join("pantheon-secure-forms-test-2");
            let _ = std::fs::remove_dir_all(&dir);
            let b = broker();
            let s = store(&b, &dir);
            for caller in [SecretCaller::Delegate, SecretCaller::Plugin] {
                let err = s
                    .save(
                        caller,
                        "X",
                        SecretScope::Custom,
                        SecretValue::new("v"),
                        Some(&OkVerifier),
                    )
                    .unwrap_err();
                assert!(err.to_string().contains("refused"));
                assert!(s.delete(caller, "X").is_err());
            }
            assert!(b.resolve("X").unwrap().is_none());
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn broker_failure_is_fail_closed_and_leaves_no_metadata() {
            // A broker with no durable vault cannot store anything.
            let b = SecretsBroker::new();
            let dir = std::env::temp_dir().join("pantheon-secure-forms-test-3");
            let _ = std::fs::remove_dir_all(&dir);
            let s = store(&b, &dir);
            let err = s
                .save(
                    SecretCaller::UserSession,
                    "X",
                    SecretScope::Custom,
                    SecretValue::new("distinct-test-value-999"),
                    Some(&OkVerifier),
                )
                .unwrap_err();
            assert!(!dir.join("secure-forms.json").exists());
            assert!(!err.to_string().contains("distinct-test-value-999"));
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn rotate_requires_existing_and_updates_value_and_timestamp() {
            let dir = std::env::temp_dir().join("pantheon-secure-forms-test-4");
            let _ = std::fs::remove_dir_all(&dir);
            let b = broker();
            let s = store(&b, &dir);
            assert!(s
                .rotate(
                    SecretCaller::UserSession,
                    "NOPE",
                    SecretValue::new("v"),
                    Some(&OkVerifier)
                )
                .is_err());
            s.save(
                SecretCaller::UserSession,
                "ROT",
                SecretScope::Mcp,
                SecretValue::new("old"),
                Some(&OkVerifier),
            )
            .unwrap();
            let first = s.list()[0].updated_at;
            std::thread::sleep(std::time::Duration::from_secs(1));
            s.rotate(
                SecretCaller::UserSession,
                "ROT",
                SecretValue::new("new"),
                Some(&OkVerifier),
            )
            .unwrap();
            assert_eq!(b.resolve("ROT").unwrap().unwrap().expose(), "new");
            assert!(s.list()[0].updated_at >= first);
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn delete_removes_value_and_metadata() {
            let dir = std::env::temp_dir().join("pantheon-secure-forms-test-5");
            let _ = std::fs::remove_dir_all(&dir);
            let b = broker();
            let s = store(&b, &dir);
            s.save(
                SecretCaller::UserSession,
                "DEL",
                SecretScope::Custom,
                SecretValue::new("v"),
                Some(&OkVerifier),
            )
            .unwrap();
            s.delete(SecretCaller::UserSession, "DEL").unwrap();
            assert!(b.resolve("DEL").unwrap().is_none());
            assert!(s.list().is_empty());
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn verification_failure_still_saves_and_never_echoes_value() {
            let dir = std::env::temp_dir().join("pantheon-secure-forms-test-6");
            let _ = std::fs::remove_dir_all(&dir);
            let b = broker();
            let s = store(&b, &dir);
            let out = s
                .save(
                    SecretCaller::UserSession,
                    "BAD",
                    SecretScope::Provider,
                    SecretValue::new("do-not-echo-this-value"),
                    Some(&FailVerifier),
                )
                .unwrap();
            assert!(matches!(out.verification, VerifyOutcome::Failed(_)));
            let dbg = format!("{out:?}");
            assert!(!dbg.contains("do-not-echo-this-value"));
            assert!(b.resolve("BAD").unwrap().is_some());
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn credential_diagnosis_maps_1010_401_403_and_telegram_codes() {
            assert!(credential_diagnosis(403, "error code: 1010")
                .expect("1010 must diagnose")
                .contains("Do NOT replace the key"));
            assert!(credential_diagnosis(401, "unauthorized")
                .expect("401 must diagnose")
                .contains("revoked"));
            assert!(credential_diagnosis(403, "forbidden")
                .expect("403 must diagnose")
                .contains("not permitted"));
            assert!(credential_diagnosis(429, "rate limited").is_none());
            assert!(telegram_diagnosis(404)
                .expect("telegram 404 must diagnose")
                .contains("<digits>:<letters>"));
            assert!(telegram_diagnosis(401)
                .expect("telegram 401 must diagnose")
                .contains("revoked"));
            assert!(telegram_diagnosis(403)
                .expect("telegram 403 must diagnose")
                .contains("blocked"));
        }

        #[test]
        fn local_verifier_checks_shape_only() {
            assert_eq!(
                LocalVerifier { min_len: 16 }.verify(&SecretValue::new("short")),
                VerifyOutcome::Failed("secret is shorter than 16 characters".to_string())
            );
            assert_eq!(
                LocalVerifier { min_len: 16 }.verify(&SecretValue::new("long-enough-vault-key")),
                VerifyOutcome::Verified
            );
        }

        #[test]
        fn references_find_key_env_paths() {
            let refs = SecureFormStore::references(
                "[model]\nprovider = \"exoseed\"\nkey_env = \"EXO_KEY\"\n\n[custom_providers.nvidia]\nbase_url = \"https://x\"\nkey_env = \"NVIDIA_KEY\"\n",
            );
            assert_eq!(
                refs.get("EXO_KEY").unwrap(),
                &vec!["model.key_env".to_string()]
            );
            assert_eq!(
                refs.get("NVIDIA_KEY").unwrap(),
                &vec!["custom_providers.nvidia.key_env".to_string()]
            );
            assert!(!refs.contains_key("exoseed"));
        }

        #[test]
        fn scope_parsing_accepts_common_names() {
            assert_eq!(SecretScope::parse("provider"), Some(SecretScope::Provider));
            assert_eq!(SecretScope::parse("gsd"), Some(SecretScope::BrowserVault));
            assert_eq!(SecretScope::parse("telegram"), Some(SecretScope::BotToken));
            assert_eq!(SecretScope::parse("nope"), None);
        }
    }
}

pub mod gateway_tokens {
    //! Gateway chat-surface tokens: the Telegram and Discord bot tokens that
    //! `gateway run` needs.
    //!
    //! Collected by the Full Setup wizard's Gateway screen. The tokens live in
    //! `<data_dir>/gateway.env` - a [`DotenvVault`] namespace deliberately
    //! separate from `.env`, so bot tokens never surface in the `.env` key
    //! manager - and a process env var always wins over the file when both are
    //! set (an exported rotation must beat a stale stored token).
    //!
    //! Tokens travel only as [`SecretValue`]. The one display helper,
    //! [`mask_token`], shows the last four characters; nothing here ever logs
    //! or prints a full value.

    use crate::{
        vault::SecretVault, DotenvVault, EnvVault, MemoryVault, ReadOnlyVault, SecretValue,
        SecretsBroker, SecretsError,
    };
    use std::path::Path;

    /// Secrets-file namespace for gateway tokens (`<data_dir>/gateway.env`).
    pub const GATEWAY_SECRETS_FILE: &str = "gateway.env";
    /// Secret name (and env var) for the Telegram bot token.
    pub const TELEGRAM_TOKEN_NAME: &str = "PANTHEON_TELEGRAM_BOT_TOKEN";
    /// Secret name (and env var) for the Discord bot token.
    pub const DISCORD_TOKEN_NAME: &str = "PANTHEON_DISCORD_TOKEN";

    /// Broker resolving the two channel tokens: process env first, then
    /// `<data_dir>/gateway.env`. The env values are mirrored into a read-only
    /// front vault - the same trick as
    /// [`SecretsBroker::from_system_env_with_api_key`] - so an exported var
    /// beats the file on read, while `set`/`delete` fall through to the file
    /// vault and can never touch the process environment.
    pub fn gateway_token_broker(data_dir: &Path) -> SecretsBroker {
        let mirror = MemoryVault::new();
        for name in [TELEGRAM_TOKEN_NAME, DISCORD_TOKEN_NAME] {
            if let Ok(v) = std::env::var(name) {
                if !v.trim().is_empty() {
                    let _ = mirror.set(name, SecretValue::new(v));
                }
            }
        }
        SecretsBroker::new()
            .with_vault(Box::new(DotenvVault::new_file(
                data_dir,
                GATEWAY_SECRETS_FILE,
            )))
            .with_vault_front(Box::new(ReadOnlyVault::new(mirror)))
            .with_env(EnvVault::from_map(Vec::<(&str, &str)>::new()))
    }

    /// Read one channel token. `None` = not configured anywhere; blank values
    /// are treated as missing. The value is never logged.
    pub fn gateway_token(data_dir: &Path, name: &str) -> Option<SecretValue> {
        gateway_token_broker(data_dir)
            .resolve(name)
            .ok()
            .flatten()
            .filter(|v| !v.expose().trim().is_empty())
    }

    /// Store one channel token in `<data_dir>/gateway.env`.
    pub fn set_gateway_token(
        data_dir: &Path,
        name: &str,
        token: SecretValue,
    ) -> Result<(), SecretsError> {
        file_broker(data_dir).set(name, token)
    }

    /// Remove one channel token from `<data_dir>/gateway.env`. A name no
    /// vault knows is a no-op.
    pub fn delete_gateway_token(data_dir: &Path, name: &str) -> Result<(), SecretsError> {
        file_broker(data_dir).delete(name)
    }

    /// Broker over the secrets file only - no env mirror, so writes and
    /// deletes land in the file and nothing else.
    fn file_broker(data_dir: &Path) -> SecretsBroker {
        SecretsBroker::new()
            .with_vault(Box::new(DotenvVault::new_file(
                data_dir,
                GATEWAY_SECRETS_FILE,
            )))
            .with_env(EnvVault::from_map(Vec::<(&str, &str)>::new()))
    }

    /// Masked display for a token: `••••1234`. The last four characters let
    /// the user tell tokens apart; never the full value. Tokens of four
    /// characters or fewer are fully masked.
    pub fn mask_token(value: &SecretValue) -> String {
        let chars: Vec<char> = value.expose().chars().collect();
        if chars.len() <= 4 {
            return "••••".to_string();
        }
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("{}{tail}", "•".repeat(chars.len() - 4))
    }
}

pub mod migration {
    //! Plaintext store migration: backup, migrate, verify, never delete.
    //!
    //! The gateway and login stores live as plaintext dotenv files today.
    //! This migrator copies every entry into the encrypted file vault,
    //! verifies each value by reading it back, and records a report.
    //! Plaintext originals are left in place. Deleting them is a separate
    //! human approved step after a verified restart.

    use crate::{EncryptedFileVault, SecretValue, SecretVault};
    use std::path::Path;

    pub struct MigrationReport {
        pub file: String,
        pub migrated: usize,
        pub verified: bool,
    }

    /// Migrate one dotenv file into the encrypted vault at `vault_path`
    /// sealed by the key at `key_path`. Backup of the source file is written
    /// to `<data_dir>/backups/<file>.<timestamp>.bak` when possible.
    pub fn migrate_dotenv_to_filevault(
        data_dir: &Path,
        file_name: &str,
        vault_path: &Path,
        key_path: &Path,
    ) -> Result<MigrationReport, String> {
        let src = data_dir.join(file_name);
        let text = std::fs::read_to_string(&src).map_err(|e| format!("read {file_name}: {e}"))?;
        let entries = pantheon_api::dotenv::parse_dotenv(&text);
        if !entries.is_empty() {
            let backup_dir = data_dir.join("backups");
            std::fs::create_dir_all(&backup_dir).map_err(|e| format!("create backup dir: {e}"))?;
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let backup = backup_dir.join(format!("{file_name}.{stamp}.bak"));
            std::fs::write(&backup, &text).map_err(|e| format!("backup: {e}"))?;
        }
        let vault = EncryptedFileVault::open(vault_path, key_path)
            .map_err(|e| format!("open vault: {e}"))?;
        let mut migrated = 0usize;
        for (name, value) in &entries {
            vault
                .set(name, SecretValue::new(value.clone()))
                .map_err(|e| format!("migrate {name}: {e}"))?;
            migrated += 1;
        }
        // Verify by reopening and reading back every entry.
        let reopened = EncryptedFileVault::open(vault_path, key_path)
            .map_err(|e| format!("reopen vault: {e}"))?;
        for (name, value) in &entries {
            let back = reopened
                .get(name)
                .map_err(|e| format!("verify {name}: {e}"))?
                .ok_or_else(|| format!("verify {name}: missing after migration"))?;
            if back.expose() != value {
                return Err(format!("verify {name}: value mismatch"));
            }
        }
        Ok(MigrationReport {
            file: file_name.to_string(),
            migrated,
            verified: true,
        })
    }
}

pub mod dotenv {
    //! Dotenv-file vault: `<data_dir>/.env` as a [`SecretVault`].
    //!
    //! The dashboard's `.env` key manager used to read and write the file
    //! directly, bypassing the secrets crate. Routing it through this vault
    //! means dotenv keys get the same name validation, value hygiene, and
    //! broker contracts as every other backend. Reads and writes reuse the
    //! atomic helpers in [`pantheon_api::dotenv`] (line-preserving, owner-only
    //! permissions), so behavior matches the rest of the codebase.

    use crate::error::SecretsError;
    use crate::value::SecretValue;
    use crate::vault::SecretVault;
    use std::path::{Path, PathBuf};

    /// `SecretVault` over `<data_dir>/.env`.
    #[derive(Debug)]
    pub struct DotenvVault {
        data_dir: PathBuf,
        file_name: String,
    }

    impl DotenvVault {
        /// Vault over the `.env` file inside `data_dir`.
        pub fn new(data_dir: impl AsRef<Path>) -> Self {
            Self::new_file(data_dir, ".env")
        }

        /// Vault over a different dotenv file inside `data_dir` (same
        /// atomic/permission semantics). Used for secret namespaces that
        /// must not appear in the `.env` key manager - e.g. website-login
        /// passwords live in `logins.env`, never in `.env`.
        pub fn new_file(data_dir: impl AsRef<Path>, file_name: &str) -> Self {
            Self {
                data_dir: data_dir.as_ref().to_path_buf(),
                file_name: file_name.to_string(),
            }
        }

        fn path(&self) -> PathBuf {
            self.data_dir.join(&self.file_name)
        }
    }

    impl SecretVault for DotenvVault {
        fn get(&self, name: &str) -> Result<Option<SecretValue>, SecretsError> {
            crate::error::validate_name(name)?;
            Ok(
                pantheon_api::dotenv::read_dotenv_file_value(&self.data_dir, &self.file_name, name)
                    .map(SecretValue::new),
            )
        }

        fn set(&self, name: &str, value: SecretValue) -> Result<(), SecretsError> {
            crate::error::validate_name(name)?;
            if !pantheon_api::dotenv::valid_key(name) {
                return Err(SecretsError::Invalid(format!(
                    "invalid dotenv key name {name:?}"
                )));
            }
            if value.expose().contains('\n') || value.expose().contains('\r') {
                return Err(SecretsError::Invalid(format!(
                    "value for {name:?} must be single-line"
                )));
            }
            pantheon_api::dotenv::upsert_dotenv_file(
                &self.data_dir,
                &self.file_name,
                name,
                value.expose(),
            )
            .map_err(SecretsError::Io)
        }

        fn delete(&self, name: &str) -> Result<(), SecretsError> {
            crate::error::validate_name(name)?;
            // Removing an unknown name is a no-op, per the trait contract.
            pantheon_api::dotenv::delete_dotenv_file_key(&self.data_dir, &self.file_name, name)
                .map_err(SecretsError::Io)?;
            Ok(())
        }

        fn names(&self) -> Result<Vec<String>, SecretsError> {
            let text = std::fs::read_to_string(self.path()).unwrap_or_default();
            let mut names: Vec<String> = pantheon_api::dotenv::parse_dotenv(&text)
                .into_iter()
                .map(|(k, _)| k)
                .collect();
            names.sort();
            names.dedup();
            Ok(names)
        }
    }

    #[cfg(test)]
    mod dotenv_vault_concurrency_tests {
        use super::*;
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);

        /// Item 2b: 8 threads x 25 keys through the real write path
        /// (`DotenvVault::set`, the same path `PUT /api/env` uses). Without
        /// an inter-request lock around read-modify-write, concurrent
        /// writers lose updates (the audit empirically saw 100/200 survive).
        #[test]
        fn concurrent_sets_all_survive() {
            let dir = std::env::temp_dir().join(format!(
                "pantheon-secrets-conc-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let vault = DotenvVault::new(&dir);
            std::thread::scope(|s| {
                for t in 0..8 {
                    let vault = &vault;
                    s.spawn(move || {
                        for k in 0..25 {
                            let name = format!("CONC_{t}_{k}");
                            vault
                                .set(&name, SecretValue::new(format!("v{t}-{k}")))
                                .expect("concurrent set");
                        }
                    });
                }
            });
            let mut missing = Vec::new();
            for t in 0..8 {
                for k in 0..25 {
                    let name = format!("CONC_{t}_{k}");
                    match vault.get(&name).expect("get") {
                        Some(v) if v.expose() == format!("v{t}-{k}") => {}
                        other => missing.push((name, other.map(|v| v.expose().to_string()))),
                    }
                }
            }
            assert!(
                missing.is_empty(),
                "lost {} of 200 concurrent writes, e.g. {:?}",
                missing.len(),
                &missing[..missing.len().min(3)]
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

pub mod env {
    //! Environment compatibility vault.
    //!
    //! Lets 12-factor style deployments feed secrets through the process
    //! environment without touching the runtime secret store:
    //!
    //! - Name `db.password` resolves to env var `PANTHEON_SECRET_DB_PASSWORD`
    //!   (uppercase, `.`/`-` become `_`).
    //! - Name `env:FOO` resolves to the literal env var `FOO` - but ONLY when
    //!   `FOO` matches the vault's allowlist (see
    //!   [`EnvVault::with_env_allowlist`]). Without an allowlist entry the
    //!   lookup fails closed (`Ok(None)`), so a name can never be used to
    //!   exfiltrate an arbitrary host variable like `env:AWS_SECRET_ACCESS_KEY`.
    //!
    //! The system vault is read-only; store in a durable vault instead.

    use crate::error::SecretsError;
    use crate::value::SecretValue;
    use crate::vault::SecretVault;
    use std::collections::HashMap;
    use std::sync::{Mutex, MutexGuard};

    const PREFIX: &str = "PANTHEON_SECRET_";

    /// Match one allowlist entry against an env var name.
    ///
    /// - Exact entry `"FOO"` matches only `FOO`.
    /// - Trailing-star entry `"PANTHEON_"` ... `"PANTHEON_*"` matches any name
    ///   with that prefix.
    /// - `"*"` alone matches everything: the explicit opt-out for operators
    ///   who accept the old open behavior.
    ///
    /// Entries are trimmed; empty entries never match. The same matcher gates
    /// both `env:` secret lookups and plugin-subprocess env passthrough, so
    /// the two boundaries cannot drift apart.
    pub fn env_var_allowed(allowlist: &[String], var: &str) -> bool {
        allowlist.iter().any(|entry| {
            let entry = entry.trim();
            if entry.is_empty() {
                return false;
            }
            match entry.strip_suffix('*') {
                Some(prefix) => var.starts_with(prefix),
                None => var == entry,
            }
        })
    }

    /// Reads secrets from the process environment.
    #[derive(Debug, Default)]
    pub struct EnvVault {
        vars: Mutex<HashMap<String, SecretValue>>,
        /// `true` when this vault mirrors the real process environment
        /// (`system()`), which forbids `set`/`delete`.
        system: bool,
        /// Env var names readable through the `env:` literal form. Exact names
        /// or `PREFIX_*` entries (see [`env_var_allowed`]). Empty (default) =
        /// `env:` resolves nothing: fail closed.
        env_allowlist: Vec<String>,
    }

    impl Clone for EnvVault {
        fn clone(&self) -> Self {
            Self {
                vars: Mutex::new(self.vars.lock().expect("EnvVault mutex poisoned").clone()),
                system: self.system,
                env_allowlist: self.env_allowlist.clone(),
            }
        }
    }

    impl EnvVault {
        /// Snapshot the real process environment, keeping only
        /// `PANTHEON_SECRET_*` variables.
        pub fn system() -> Self {
            let vars = std::env::vars()
                .filter(|(k, _)| k.starts_with(PREFIX))
                .map(|(k, v)| (k, SecretValue::new(v)))
                .collect();
            Self {
                vars: Mutex::new(vars),
                system: true,
                env_allowlist: Vec::new(),
            }
        }

        /// Build a vault from an explicit map (tests, embedded runtimes).
        pub fn from_map<K, V>(vars: impl IntoIterator<Item = (K, V)>) -> Self
        where
            K: Into<String>,
            V: Into<String>,
        {
            Self {
                vars: Mutex::new(
                    vars.into_iter()
                        .map(|(k, v)| (k.into(), SecretValue::new(v.into())))
                        .collect(),
                ),
                system: false,
                env_allowlist: Vec::new(),
            }
        }

        fn state(&self) -> MutexGuard<'_, HashMap<String, SecretValue>> {
            self.vars.lock().expect("EnvVault mutex poisoned")
        }

        /// Restrict the `env:` literal form to the given allowlist. Entries are
        /// exact var names or `PREFIX_*` wildcards (see [`env_var_allowed`]).
        /// Reads of non-allowlisted vars fail closed (`Ok(None)`).
        pub fn with_env_allowlist(mut self, allowlist: Vec<String>) -> Self {
            self.env_allowlist = allowlist;
            self
        }

        /// The allowlist governing `env:` reads on this vault.
        pub fn env_allowlist(&self) -> &[String] {
            &self.env_allowlist
        }

        /// Map a requested name to the env var key it reads.
        pub fn env_key(name: &str) -> String {
            if let Some(literal) = name.strip_prefix("env:") {
                return literal.to_string();
            }
            let normalized = name.replace(['.', '-'], "_").to_ascii_uppercase();
            format!("{PREFIX}{normalized}")
        }
    }

    impl SecretVault for EnvVault {
        fn get(&self, name: &str) -> Result<Option<SecretValue>, SecretsError> {
            crate::error::validate_name(name)?;
            if let Some(literal) = name.strip_prefix("env:") {
                // Fail closed: a literal env read needs an explicit allowlist
                // hit, otherwise the `env:` form is an arbitrary host-variable
                // read gadget.
                if !env_var_allowed(&self.env_allowlist, literal) {
                    return Ok(None);
                }
                let value = if self.system {
                    // System vaults mirror the live process environment.
                    std::env::var(literal).ok().map(SecretValue::new)
                } else {
                    self.state().get(literal).cloned()
                };
                return Ok(value);
            }
            let key = Self::env_key(name);
            Ok(self.state().get(&key).cloned())
        }

        fn set(&self, _name: &str, _value: SecretValue) -> Result<(), SecretsError> {
            if self.system {
                return Err(SecretsError::Unsupported(
                    "process environment is read-only; store in a durable vault".into(),
                ));
            }
            Err(SecretsError::Unsupported(
                "use MemoryVault or EncryptedFileVault to store; EnvVault is for reads only".into(),
            ))
        }

        fn delete(&self, _name: &str) -> Result<(), SecretsError> {
            if self.system {
                return Err(SecretsError::Unsupported(
                    "process environment is read-only".into(),
                ));
            }
            Err(SecretsError::Unsupported(
                "EnvVault is for reads only".into(),
            ))
        }

        fn names(&self) -> Result<Vec<String>, SecretsError> {
            let mut names: Vec<String> = self
                .state()
                .keys()
                .filter(|k| k.starts_with(PREFIX))
                .map(|k| k.trim_start_matches(PREFIX).to_ascii_lowercase())
                .collect();
            names.sort();
            Ok(names)
        }
    }
}
