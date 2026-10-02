//! Steel backend: cloud/self-hosted browser sessions through Steel's REST API,
//! driven over the returned CDP websocket.
//!
//! This backend uses the official [`steel_rs`] Rust SDK (the `steel-rs`
//! crate) for session lifecycle: `client.sessions().create(..)` on first
//! use per Pantheon session, `client.sessions().release(..)` on
//! `stop_daemon`. The returned `websocketUrl` is driven through the shared
//! raw-CDP [`CdpDriver`](super::cdp::CdpDriver) (chromiumoxide 0.9.1),
//! exactly as Steel's own Rust cookbook does - the SDK hands off the CDP
//! URL and the driver takes it from there.
//!
//! The API key is only ever sent as the `steel-api-key` header (REST) and
//! as the `apiKey` query parameter the Steel cloud requires on the
//! websocket URL (per the official `steel-rust` examples); self-hosted
//! sessions carry no key. It is never logged.

use super::backend::BrowserBackend;
use super::cdp::shared_runtime;
use super::error::BrowserError;
use super::remote::{RemoteCdpBackend, SessionProvisioner};
use std::time::Duration;

/// Default REST base URL (Steel cloud). Self-hosted deployments override
/// it through [`SteelConfig::base_url`].
pub const DEFAULT_BASE_URL: &str = "https://api.steel.dev";

/// Environment variable consulted when no explicit key is configured.
pub const STEEL_API_KEY_ENV: &str = "STEEL_API_KEY";

/// Steel backend configuration.
#[derive(Debug, Clone)]
pub struct SteelConfig {
    /// Resolved API key (never logged). `None` means unset.
    pub api_key: Option<String>,
    /// Override for self-hosted Steel. `None`/empty = [`DEFAULT_BASE_URL`].
    pub base_url: Option<String>,
    /// Timeout for the REST calls, in seconds.
    pub timeout_secs: u64,
}

impl Default for SteelConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            base_url: None,
            timeout_secs: 60,
        }
    }
}

impl SteelConfig {
    /// The effective REST base URL, without a trailing slash.
    pub fn base_url(&self) -> &str {
        match self.base_url.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(b) => b.trim_end_matches('/'),
            None => DEFAULT_BASE_URL,
        }
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.max(1))
    }

    /// The effective API key: trimmed, non-empty. `None` = unset.
    pub fn effective_api_key(&self) -> Option<&str> {
        self.api_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// True when pointing at a custom (self-hosted) base URL rather
    /// than the Steel cloud.
    pub fn is_self_hosted(&self) -> bool {
        self.base_url
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
    }

    /// Fail fast when the key is missing, before any network happens.
    /// The Steel cloud requires a key; a custom self-hosted base URL is
    /// allowed without one (self-hosted Steel needs no auth by
    /// default).
    pub fn validate(&self) -> Result<(), BrowserError> {
        if self.effective_api_key().is_none() && !self.is_self_hosted() {
            return Err(BrowserError::Failed {
                argv: Vec::new(),
                exit: None,
                stderr: format!(
                    "steel backend needs an API key: set `browser.steel_api_key_secret` \
                     or the {STEEL_API_KEY_ENV} secret"
                ),
            });
        }
        Ok(())
    }

    /// Build the official SDK client. An unset key is passed as `""`:
    /// `with_base_url` sends it as an empty `steel-api-key` header,
    /// which self-hosted Steel ignores - and unlike `Steel::new` it
    /// does not consult the process environment, keeping the secrets
    /// broker the single source of keys.
    fn client(&self) -> Result<steel::Steel, BrowserError> {
        let key = self.effective_api_key().unwrap_or("");
        Ok(steel::Steel::with_base_url(key, self.base_url()).with_timeout(self.timeout()))
    }
}

fn sdk_err(e: steel::Error) -> BrowserError {
    let stderr = match &e {
        steel::Error::Authentication(_) => {
            "steel: API key rejected (401) - check the configured key".to_string()
        }
        _ => format!("steel session API error: {e}"),
    };
    BrowserError::Failed {
        argv: Vec::new(),
        exit: None,
        stderr,
    }
}

/// Append the `apiKey` query parameter Steel's cloud requires on the
/// CDP URL - only when the key is non-empty. Self-hosted sessions
/// carry no key and must not get a dangling `?apiKey=`.
fn authed_cdp_url(websocket_url: &str, api_key: &str) -> String {
    if api_key.is_empty() {
        return websocket_url.to_string();
    }
    let sep = if websocket_url.contains('?') {
        "&"
    } else {
        "?"
    };
    format!("{websocket_url}{sep}apiKey={api_key}")
}

struct SteelProvisioner {
    config: SteelConfig,
}

impl SessionProvisioner for SteelProvisioner {
    fn name(&self) -> &'static str {
        "steel"
    }

    fn provision(&self, _pantheon_session: &str) -> Result<(String, String), BrowserError> {
        let client = self.config.client()?;
        let session = shared_runtime()
            .block_on(
                client
                    .sessions()
                    .create(steel::SessionCreateParams::default())
                    .send(),
            )
            .map_err(sdk_err)?;
        // The key is needed again for the websocket `apiKey` parameter
        // on cloud sessions; re-read from config (never from the
        // response). Self-hosted sessions carry no key.
        let key = self.config.effective_api_key().unwrap_or("");
        let url = authed_cdp_url(&session.websocket_url, key);
        Ok((session.id, url))
    }

    fn release(&self, provider_session_id: &str) -> Result<(), BrowserError> {
        let client = self.config.client()?;
        let res = shared_runtime().block_on(
            client
                .sessions()
                .release(provider_session_id, steel::SessionReleaseParams::default())
                .send(),
        );
        match res {
            Ok(_) => Ok(()),
            // Already gone: nothing to release.
            Err(steel::Error::NotFound(_)) => Ok(()),
            Err(e) => Err(sdk_err(e)),
        }
    }
}

/// Steel browser backend.
pub struct SteelBackend {
    inner: RemoteCdpBackend<SteelProvisioner>,
}

impl SteelBackend {
    pub fn new(config: SteelConfig) -> Self {
        Self {
            inner: RemoteCdpBackend::new(SteelProvisioner { config }),
        }
    }
}

impl BrowserBackend for SteelBackend {
    fn invoke(&self, argv: &[String], session: &str) -> Result<serde_json::Value, BrowserError> {
        self.inner.invoke(argv, session)
    }

    fn stop_daemon(&self, session: &str) -> Result<(), BrowserError> {
        self.inner.stop_daemon(session)
    }
}
