//! Shared lazy session provisioning for the CDP-over-websocket backends
//! (Steel, Browserbase).
//!
//! Each Pantheon session gets one remote provider session, created on
//! first use and driven through [`CdpDriver`]; `stop_daemon` releases it.
//! [`SessionProvisioner`] is the per-provider seam (create/release REST
//! calls); the lazy lifecycle and the CDP driving are shared.

use super::backend::BrowserBackend;
use super::cdp::CdpDriver;
use super::error::BrowserError;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// How a remote backend creates and releases provider sessions.
pub trait SessionProvisioner: Send + Sync {
    /// Backend name for errors (e.g. `"steel"`).
    fn name(&self) -> &'static str;
    /// Create a remote session for a Pantheon session. Returns the
    /// provider's session id and the CDP websocket URL.
    fn provision(&self, pantheon_session: &str) -> Result<(String, String), BrowserError>;
    /// Release a provider session. Best-effort: `Ok` when already gone.
    fn release(&self, provider_session_id: &str) -> Result<(), BrowserError>;
}

struct LiveSession {
    provider_id: String,
    driver: std::sync::Arc<CdpDriver>,
}

/// [`BrowserBackend`] over lazily-provisioned remote CDP sessions.
pub struct RemoteCdpBackend<P: SessionProvisioner> {
    provisioner: P,
    sessions: Mutex<HashMap<String, LiveSession>>,
}

impl<P: SessionProvisioner> RemoteCdpBackend<P> {
    pub fn new(provisioner: P) -> Self {
        Self {
            provisioner,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Get (provisioning on first use) the driver for a session.
    fn driver_for(&self, session: &str) -> Result<std::sync::Arc<CdpDriver>, BrowserError> {
        if let Some(live) = self.inner_lock().get(session) {
            return Ok(live.driver.clone());
        }
        // Provision outside the lock (network I/O).
        let (provider_id, cdp_url) = self.provisioner.provision(session)?;
        let driver =
            std::sync::Arc::new(CdpDriver::connect_lazy(cdp_url, self.provisioner.name())?);
        let mut map = self.inner_lock();
        // Double-check: a racing thread may have provisioned meanwhile;
        // keep the first, release the duplicate so we don't leak a paid
        // remote session.
        if let Some(live) = map.get(session) {
            let driver = live.driver.clone();
            drop(map);
            let _ = self.provisioner.release(&provider_id);
            return Ok(driver);
        }
        map.insert(
            session.to_string(),
            LiveSession {
                provider_id,
                driver: driver.clone(),
            },
        );
        Ok(driver)
    }

    fn inner_lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, LiveSession>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl<P: SessionProvisioner> BrowserBackend for RemoteCdpBackend<P> {
    fn invoke(&self, argv: &[String], session: &str) -> Result<serde_json::Value, BrowserError> {
        let driver = self.driver_for(session)?;
        driver.execute(argv, session)
    }

    fn stop_daemon(&self, session: &str) -> Result<(), BrowserError> {
        let live = self.inner_lock().remove(session);
        match live {
            None => Ok(()),
            Some(live) => {
                live.driver.close_session(session);
                self.provisioner.release(&live.provider_id)
            }
        }
    }
}

/// POST JSON and parse the JSON response, mapping failures to
/// [`BrowserError`] without ever including auth headers or bodies in the
/// message. `timeout_secs` bounds the whole call.
pub(crate) fn post_json(
    url: &str,
    headers: &[(&str, &str)],
    body: &serde_json::Value,
    timeout_secs: u64,
    argv: &[String],
) -> Result<serde_json::Value, BrowserError> {
    let failed = |detail: String| BrowserError::Failed {
        argv: argv.to_vec(),
        exit: None,
        stderr: detail,
    };
    post_json_raw(url, headers, body, timeout_secs).map_err(|e| match e {
        HttpError::Status(code, body_snip) => {
            if code == 401 || code == 403 {
                failed(format!(
                    "HTTP {code}: authentication rejected - check the API key (response: {body_snip})"
                ))
            } else {
                failed(format!("HTTP {code} from session API: {body_snip}"))
            }
        }
        HttpError::Transport(t) => failed(format!("session API transport error: {t}")),
        HttpError::Decode(d) => failed(format!("session API returned non-JSON: {d}")),
    })
}

/// HTTP failure without any auth material attached.
#[derive(Debug)]
pub(crate) enum HttpError {
    Status(u16, String),
    Transport(String),
    Decode(String),
}

/// [`post_json`] without the [`BrowserError`] mapping, so callers can
/// match on status codes (e.g. 404 = already released).
pub(crate) fn post_json_raw(
    url: &str,
    headers: &[(&str, &str)],
    body: &serde_json::Value,
    timeout_secs: u64,
) -> Result<serde_json::Value, HttpError> {
    // One overall budget (connect through body read); statuses surface as
    // responses, not errors, so the status arm below can read the body.
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(timeout_secs.max(1))))
        .max_redirects(5)
        .http_status_as_error(false)
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .new_agent();
    let mut req = agent.post(url);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let resp = req
        .send_json(body.clone())
        .map_err(|e| HttpError::Transport(e.to_string()))?;
    let code = resp.status().as_u16();
    if code >= 400 {
        let body_snip = resp
            .into_body()
            .read_to_string()
            .unwrap_or_default()
            .chars()
            .take(500)
            .collect::<String>();
        return Err(HttpError::Status(code, body_snip));
    }
    resp.into_body()
        .read_json::<serde_json::Value>()
        .map_err(|e| HttpError::Decode(e.to_string()))
}
