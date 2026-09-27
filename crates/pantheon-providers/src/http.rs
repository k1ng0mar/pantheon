//! HTTP transport for provider adapters (spec §5 + §14): one request shape
//! for the whole provider plane, plus SSE streaming so adapters can emit
//! normalized `ModelEvent`s as chunks arrive. Sync, rustls, no tokio.
//!
//! Fallback policy does NOT live here — see `chain.rs`. Adapters are
//! single-attempt; the chain decides what happens on failure.

use crate::catalog::{self, ApiMode};
use crate::model_event::{ModelUsage, NoopModelSink};
use crate::{anthropic, openai};
use pantheon_agent::TurnOutcome;
use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::message::Message;
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

/// Transport seam. Test doubles implement this in tests; `HttpTransport` in prod.
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
            timeout: http_timeout(),
        }
    }
}

/// Provider HTTP timeout. Overridable via `PANTHEON_HTTP_TIMEOUT_MS`
/// (default 120_000). Applies to single-shot calls as an overall
/// deadline and to streams as a per-chunk read idle deadline so a
/// hanging provider cannot wedge the session thread forever.
/// The turn watchdog covers process liveness; this covers provider hang.
pub fn http_timeout() -> Duration {
    std::env::var("PANTHEON_HTTP_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(120))
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

/// Resolve a provider's effective base URL, interpolating `{var}`
/// endpoint templates (azure resource, bedrock region, vertex
/// project/location, cloudflare account). Missing values are a
/// *config* error (`PROVIDER_CONFIG`, fail-fast with remediation),
/// never a network error — a raw `{placeholder}` must not reach the wire.
pub fn resolve_base(provider_id: &str) -> Result<String, PantheonError> {
    catalog::resolve_base_url(provider_id).map_err(|cause| {
        PantheonError::new(
            "PROVIDER_CONFIG",
            Layer::Provider,
            false,
            cause,
            "run `pantheon model` to fill the provider's required values",
            "",
        )
    })
}

/// Resolved single-shot wire target for aux clients (title, judge,
/// compression): base URL + wire mode + key, in one place so the three
/// clients can't drift.
pub struct AuxWire {
    pub base: String,
    pub key: String,
    pub key_header: String,
    pub api_mode: ApiMode,
    pub max_tokens: u32,
}

/// Resolve everything one aux turn needs: base, mode, key.
pub fn resolve_aux_wire(
    provider_id: &str,
    configured_key: &str,
    max_tokens: u32,
) -> Result<AuxWire, PantheonError> {
    let base = resolve_base(provider_id)?;
    let api_mode = catalog::provider(provider_id)
        .map(|p| p.api_mode)
        .unwrap_or(ApiMode::OpenAi);
    let key = catalog::key_for(provider_id, configured_key);
    let key_header = catalog::key_header_for(provider_id);
    Ok(AuxWire {
        base,
        key,
        key_header,
        api_mode,
        max_tokens,
    })
}

/// Build the wire request for one prompt: no tools, no streaming.
pub fn aux_request(wire: &AuxWire, model: &str, prompt: String) -> WireRequest {
    let messages = vec![Message::user(prompt)];
    match wire.api_mode {
        ApiMode::OpenAi => openai::request(
            &wire.base,
            &wire.key,
            &wire.key_header,
            model,
            &messages,
            &[],
            false,
            // Aux turns stay fast and cheap: no reasoning effort, ever.
            pantheon_api::model::ReasoningLevel::Off,
        ),
        ApiMode::Anthropic => anthropic::request(
            &wire.base,
            &wire.key,
            model,
            &messages,
            &[],
            false,
            wire.max_tokens,
            pantheon_api::model::ReasoningLevel::Off,
            None,
        ),
    }
}

/// Complete one single-shot aux turn.
pub fn aux_complete(
    transport: &dyn ChatTransport,
    wire: &AuxWire,
    req: WireRequest,
) -> Result<AdapterTurn, PantheonError> {
    match wire.api_mode {
        ApiMode::OpenAi => openai::complete(transport, req, &NoopModelSink),
        ApiMode::Anthropic => anthropic::complete(transport, req, &NoopModelSink),
    }
}

/// Short-timeout transport for single-shot aux clients.
pub fn aux_transport(timeout_secs: u64) -> Box<dyn ChatTransport> {
    Box::new(HttpTransport {
        timeout: Duration::from_secs(timeout_secs),
    })
}

/// Boxed transport: lets runtime code pick the HTTP transport at runtime while
/// keeping one concrete `ProviderChain` type.
impl ChatTransport for Box<dyn ChatTransport> {
    fn post(&self, req: &WireRequest) -> Result<String, PantheonError> {
        (**self).post(req)
    }
    fn post_stream(
        &self,
        req: &WireRequest,
        on_payload: &mut dyn FnMut(&str) -> Result<(), PantheonError>,
    ) -> Result<(), PantheonError> {
        (**self).post_stream(req, on_payload)
    }
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
