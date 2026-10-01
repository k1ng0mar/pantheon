//! Browserbase backend: cloud browser sessions created through the
//! Browserbase REST API, driven over the returned raw CDP websocket.
//!
//! Create: `POST /v1/sessions` with the `X-BB-API-Key` header and a body
//! of `{"projectId": ...}`; the response carries `id` and `connectUrl`.
//! Release: `POST /v1/sessions/{id}` with `{"status":"REQUEST_RELEASE"}`.
//! Driving uses the shared raw-CDP [`CdpDriver`](super::cdp::CdpDriver)
//! over `connectUrl`, exactly the cloud-fallback role this backend has in
//! the lineup. The API key is never logged.

use super::backend::BrowserBackend;
use super::error::BrowserError;
use super::remote::{post_json, HttpError, RemoteCdpBackend, SessionProvisioner};

/// Browserbase REST base URL.
pub const DEFAULT_BASE_URL: &str = "https://api.browserbase.com";

/// Environment variable consulted when no explicit key is configured.
pub const BROWSERBASE_API_KEY_ENV: &str = "BROWSERBASE_API_KEY";

/// Browserbase backend configuration.
#[derive(Debug, Clone)]
pub struct BrowserbaseConfig {
    /// Resolved API key (never logged). `None` means unset.
    pub api_key: Option<String>,
    /// Browserbase project id — required by `POST /v1/sessions`.
    pub project_id: Option<String>,
    /// Override for tests/proxies. `None`/empty = [`DEFAULT_BASE_URL`].
    pub base_url: Option<String>,
    /// Timeout for the REST calls, in seconds.
    pub timeout_secs: u64,
}

impl Default for BrowserbaseConfig {
    fn default() -> Self {
        Self {
            api_key: None,
            project_id: None,
            base_url: None,
            timeout_secs: 60,
        }
    }
}

impl BrowserbaseConfig {
    /// The effective REST base URL, without a trailing slash.
    pub fn base_url(&self) -> &str {
        match self.base_url.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(b) => b.trim_end_matches('/'),
            None => DEFAULT_BASE_URL,
        }
    }

    fn api_key(&self) -> Result<&str, BrowserError> {
        self.api_key
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| BrowserError::Failed {
                argv: Vec::new(),
                exit: None,
                stderr: format!(
                    "browserbase backend needs an API key: set `browser.browserbase_api_key_secret` \
                     or the {BROWSERBASE_API_KEY_ENV} secret"
                ),
            })
    }

    fn project_id(&self) -> Result<&str, BrowserError> {
        self.project_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| BrowserError::Failed {
                argv: Vec::new(),
                exit: None,
                stderr: "browserbase backend needs `browser.browserbase_project_id`".to_string(),
            })
    }

    /// Fail fast when the key or project id is missing.
    pub fn validate(&self) -> Result<(), BrowserError> {
        self.api_key()?;
        self.project_id()?;
        Ok(())
    }

    /// Create a session: `POST /v1/sessions` → `(id, connectUrl)`.
    pub fn create_session(&self) -> Result<(String, String), BrowserError> {
        let key = self.api_key()?;
        let project = self.project_id()?;
        let url = format!("{}/v1/sessions", self.base_url());
        let body = serde_json::json!({ "projectId": project });
        let resp = post_json(
            &url,
            &[("X-BB-API-Key", key), ("Content-Type", "application/json")],
            &body,
            self.timeout_secs,
            &[],
        )?;
        let id = resp
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| missing_field(&resp, "id"))?;
        let connect_url = resp
            .get("connectUrl")
            .and_then(|v| v.as_str())
            .ok_or_else(|| missing_field(&resp, "connectUrl"))?;
        if !(connect_url.starts_with("ws://") || connect_url.starts_with("wss://")) {
            return Err(BrowserError::Failed {
                argv: Vec::new(),
                exit: None,
                stderr: "browserbase returned a non-websocket connectUrl".to_string(),
            });
        }
        Ok((id.to_string(), connect_url.to_string()))
    }

    /// Release a session via `{"status":"REQUEST_RELEASE"}`.
    /// A 404 (already gone) is `Ok`.
    pub fn release_session(&self, session_id: &str) -> Result<(), BrowserError> {
        let key = self.api_key()?;
        let url = format!("{}/v1/sessions/{session_id}", self.base_url());
        let body = serde_json::json!({ "status": "REQUEST_RELEASE" });
        match super::remote::post_json_raw(
            &url,
            &[("X-BB-API-Key", key), ("Content-Type", "application/json")],
            &body,
            self.timeout_secs,
        ) {
            Ok(_) => Ok(()),
            Err(HttpError::Status(404, _)) => Ok(()),
            Err(e) => Err(map_http_err(e)),
        }
    }
}

fn map_http_err(e: HttpError) -> BrowserError {
    BrowserError::Failed {
        argv: Vec::new(),
        exit: None,
        stderr: match e {
            HttpError::Status(code, body) => {
                if code == 401 || code == 403 {
                    format!("HTTP {code}: authentication rejected — check the API key")
                } else {
                    format!("HTTP {code} from session API: {body}")
                }
            }
            HttpError::Transport(t) => format!("session API transport error: {t}"),
            HttpError::Decode(d) => format!("session API returned non-JSON: {d}"),
        },
    }
}

fn missing_field(resp: &serde_json::Value, field: &str) -> BrowserError {
    BrowserError::Failed {
        argv: Vec::new(),
        exit: None,
        stderr: format!("browserbase session response missing `{field}`: {resp}"),
    }
}

struct BrowserbaseProvisioner {
    config: BrowserbaseConfig,
}

impl SessionProvisioner for BrowserbaseProvisioner {
    fn name(&self) -> &'static str {
        "browserbase"
    }

    fn provision(&self, _pantheon_session: &str) -> Result<(String, String), BrowserError> {
        self.config.create_session()
    }

    fn release(&self, provider_session_id: &str) -> Result<(), BrowserError> {
        self.config.release_session(provider_session_id)
    }
}

/// Browserbase browser backend.
pub struct BrowserbaseBackend {
    inner: RemoteCdpBackend<BrowserbaseProvisioner>,
}

impl BrowserbaseBackend {
    pub fn new(config: BrowserbaseConfig) -> Self {
        Self {
            inner: RemoteCdpBackend::new(BrowserbaseProvisioner { config }),
        }
    }
}

impl BrowserBackend for BrowserbaseBackend {
    fn invoke(&self, argv: &[String], session: &str) -> Result<serde_json::Value, BrowserError> {
        self.inner.invoke(argv, session)
    }

    fn stop_daemon(&self, session: &str) -> Result<(), BrowserError> {
        self.inner.stop_daemon(session)
    }
}
