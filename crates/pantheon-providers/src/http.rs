//! HTTP transport for provider adapters (spec §5 + §14): one request shape
//! for the whole provider plane, plus SSE streaming so adapters can emit
//! normalized `ModelEvent`s as chunks arrive. Sync, rustls, no tokio.
//!
//! Fallback policy does NOT live here — see `chain.rs`. Adapters are
//! single-attempt; the chain decides what happens on failure.

use pantheon_agent::TurnOutcome;
use pantheon_core::error::{Layer, PantheonError};
use pantheon_core::model_event::ModelUsage;
use std::io::{BufRead, BufReader};
use std::time::Duration;

pub(crate) fn perr(code: &str, cause: String, retryable: bool) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Provider,
        retryable,
        cause,
        "check provider config, key, and network",
        "",
    )
}

/// Which model the turn resolved to (for the ledger).
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub provider: String,
    pub model: String,
    /// Index in the chain: 0 = default, n>0 = n-th fallback.
    pub chain_index: usize,
}

/// What one adapter attempt produced, alongside normalized accounting.
pub struct AdapterTurn {
    pub outcome: TurnOutcome,
    pub usage: Option<ModelUsage>,
    pub finish_reason: Option<String>,
}

/// One outbound HTTP call, fully described. Adapters build these; the
/// transport executes them — wire format never leaks into the chain.
#[derive(Debug, Clone)]
pub struct WireRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

/// Transport seam. Fakes implement this in tests; `HttpTransport` in prod.
pub trait ChatTransport: Send + Sync {
    /// POST the request and return the full response body.
    fn post(&self, req: &WireRequest) -> Result<String, PantheonError>;
    /// POST with `stream: true`. `on_payload` receives each SSE `data:`
    /// payload verbatim (including `[DONE]`). Returning `Err` aborts the
    /// stream and propagates; the stream ends on `data: [DONE]` or EOF.
    fn post_stream(
        &self,
        req: &WireRequest,
        on_payload: &mut dyn FnMut(&str) -> Result<(), PantheonError>,
    ) -> Result<(), PantheonError>;
}

/// ureq-based transport.
pub struct HttpTransport {
    pub timeout: Duration,
}

impl Default for HttpTransport {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
        }
    }
}

fn send(agent: &ureq::Agent, req: &WireRequest) -> Result<ureq::Response, PantheonError> {
    let mut r = agent.post(&req.url);
    for (k, v) in &req.headers {
        r = r.set(k, v);
    }
    r.send_string(&req.body).map_err(|e| match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            let snippet: String = body.chars().take(300).collect();
            // 5xx / 408-class server trouble and transport errors are
            // retryable (fallback-eligible); 4xx is a client/config problem.
            let retryable = !(400..500).contains(&code);
            perr(
                "PROVIDER_HTTP",
                format!("{}: HTTP {code} {snippet}", req.url),
                retryable,
            )
        }
        e => perr("PROVIDER_HTTP", format!("{}: {e}", req.url), true),
    })
}

impl ChatTransport for HttpTransport {
    fn post(&self, req: &WireRequest) -> Result<String, PantheonError> {
        let agent = ureq::AgentBuilder::new().timeout(self.timeout).build();
        let resp = send(&agent, req)?;
        resp.into_string()
            .map_err(|e| perr("PROVIDER_READ", e.to_string(), true))
    }

    fn post_stream(
        &self,
        req: &WireRequest,
        on_payload: &mut dyn FnMut(&str) -> Result<(), PantheonError>,
    ) -> Result<(), PantheonError> {
        // No overall timeout: a stream may run long. Connect stays bounded;
        // the read timeout is a per-chunk idle deadline.
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(self.timeout.min(Duration::from_secs(15)))
            .timeout_read(self.timeout)
            .build();
        let resp = send(&agent, req)?;
        let reader = BufReader::new(resp.into_reader());
        for line in reader.lines() {
            let line = line.map_err(|e| perr("PROVIDER_READ", e.to_string(), true))?;
            let line = line.trim_end_matches('\r');
            if line.is_empty() || line.starts_with(':') || line.starts_with("event:") {
                continue; // blank line, keepalive comment, or event tag
            }
            if let Some(payload) = line.strip_prefix("data:") {
                let payload = payload.trim_start();
                if payload.is_empty() {
                    continue;
                }
                on_payload(payload)?;
                if payload == "[DONE]" {
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}
