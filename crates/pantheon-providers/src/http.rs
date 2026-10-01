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

/// Tool-call policy for one turn.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ToolChoice {
    /// Provider default: the model may or may not call tools.
    #[default]
    Auto,
    /// The model must call at least one tool this turn.
    Required,
    /// The model must not call tools this turn.
    None,
    /// The model must call this specific tool.
    Named(String),
}

/// Per-turn wire knobs, threaded from the chain into both adapters.
/// Everything defaults to "as before", so turns built without options
/// are byte-identical on the wire to turns built before this existed.
#[derive(Debug, Clone, Default)]
pub struct TurnOptions {
    /// Opt-in structured output: JSON Schema the response must conform
    /// to. `None` sends no schema constraint.
    pub response_schema: Option<serde_json::Value>,
    /// Tool-call policy for this turn.
    pub tool_choice: ToolChoice,
}

/// Transport seam. Test doubles implement this in tests; `HttpTransport` in prod.
pub trait ChatTransport: Send + Sync {
    /// POST the request and return the full response body.
    fn post(&self, req: &WireRequest) -> Result<String, PantheonError>;
    /// POST with `stream: true`. `on_payload` receives each SSE `data:`
    /// payload verbatim (including `[DONE]`). Returning `Err` aborts the
    /// stream and propagates; the stream ends on `data: [DONE]`
    /// ([`StreamEnd::Done`]) or EOF ([`StreamEnd::Eof`]).
    fn post_stream(
        &self,
        req: &WireRequest,
        on_payload: &mut dyn FnMut(&str) -> Result<(), PantheonError>,
    ) -> Result<StreamEnd, PantheonError>;
}

/// How an SSE stream ended. `HttpTransport` reports this; adapters
/// decide what it means for the turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamEnd {
    /// The provider sent `data: [DONE]`: the turn is complete.
    Done,
    /// EOF arrived without `[DONE]`: the stream may be truncated. The
    /// adapter accepts the turn only when it already saw a finish
    /// signal (`finish_reason`); otherwise it fails with
    /// `PROVIDER_TRUNCATED` (retryable) so the chain can retry or fall
    /// back instead of presenting a silently cut turn as complete.
    Eof,
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

/// Shared request agent honoring [`http_timeout`]. Every provider-plane
/// HTTP call (chat, aux, voice, embeddings) goes through this so none
/// runs without a deadline.
pub fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(http_timeout()).build()
}

/// `(name, value)` for the key header: `Authorization` sends
/// `Bearer <key>`; any other configured name (e.g. Xiaomi MiMo's
/// `api-key`) sends the raw key. One place so chat, embeddings, and
/// voice can't drift on auth shape.
pub fn auth_header_pair(key_header: &str, api_key: &str) -> (String, String) {
    let name = if key_header.trim().is_empty() {
        "Authorization".to_string()
    } else {
        key_header.trim().to_string()
    };
    let value = if name.eq_ignore_ascii_case("authorization") {
        format!("Bearer {api_key}")
    } else {
        api_key.to_string()
    };
    (name, value)
}

/// Cap for honoring `Retry-After` on 429s: a provider may ask for a long
/// wait, but one slow endpoint must not wedge the turn past this.
pub const MAX_RETRY_AFTER_SECS: u64 = 60;

/// Parse a `Retry-After` header value into a capped wait in seconds.
/// Accepts delta-seconds (`120`) and an HTTP-date
/// (`Sun, 06 Nov 1994 08:49:37 GMT`); anything else is `None` — no wait
/// rather than a wrong wait. Pure, so the backoff math is unit-testable
/// without touching the network.
pub fn parse_retry_after(value: &str) -> Option<u64> {
    let v = value.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(secs.min(MAX_RETRY_AFTER_SECS));
    }
    let at = http_date_to_epoch(v)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Some(at.saturating_sub(now).min(MAX_RETRY_AFTER_SECS))
}

/// IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`) → unix seconds.
/// Hand-rolled: the provider plane has no date crate, and only this one
/// format is worth accepting (RFC 850 / asctime dates are vanishingly
/// rare on `Retry-After`).
fn http_date_to_epoch(value: &str) -> Option<u64> {
    let mut parts = value.split_whitespace();
    let weekday = parts.next()?;
    if !weekday.ends_with(',') {
        return None;
    }
    let day: i64 = parts.next()?.parse().ok()?;
    if !(1..=31).contains(&day) {
        return None;
    }
    let month: i64 = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts.next()?.parse().ok()?;
    let time = parts.next()?;
    if parts.next()? != "GMT" || parts.next().is_some() {
        return None;
    }
    let mut t = time.split(':');
    let h: i64 = t.next()?.parse().ok()?;
    let m: i64 = t.next()?.parse().ok()?;
    let s: i64 = t.next()?.parse().ok()?;
    if t.next().is_some() || h > 23 || m > 59 || s > 60 {
        return None;
    }
    // Howard Hinnant's days-from-civil.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400; // [0, 399]
    let mp = (month + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    u64::try_from(days * 86400 + h * 3600 + m * 60 + s).ok()
}

/// Re-read the capped `Retry-After` wait a 429 asked for, from the marker
/// `send()` stamped on the error cause. `None` = no wait requested.
/// The parser lives in [`error_kind`]; the TUI card reuses it for the
/// rate-limit "retry in Ns" line.
pub fn retry_after_secs(err: &PantheonError) -> Option<u64> {
    crate::error_kind::retry_after_secs_from_cause(&err.cause)
}

fn send(agent: &ureq::Agent, req: &WireRequest) -> Result<ureq::Response, PantheonError> {
    let mut r = agent.post(&req.url);
    for (k, v) in &req.headers {
        r = r.set(k, v);
    }
    r.send_string(&req.body).map_err(|e| match e {
        ureq::Error::Status(code, resp) => {
            // A 429 carries the provider's asked-for wait: parse
            // `Retry-After` (delta-seconds or HTTP-date, capped) and stamp
            // it on the cause so the chain can sleep before rotating keys
            // or falling back instead of hammering a rate-limited endpoint.
            let retry_after = if code == 429 {
                resp.header("retry-after").and_then(parse_retry_after)
            } else {
                None
            };
            let body = resp.into_string().unwrap_or_default();
            let snippet: String = body.chars().take(300).collect();
            // 5xx / 408-class server trouble and transport errors are
            // retryable (fallback-eligible); 4xx is a client/config problem
            // — except 429, which is transient: another key or provider may
            // still have quota, and the stamped Retry-After paces the retry.
            let retryable = !(400..500).contains(&code) || code == 429;
            let mut cause = format!("{}: HTTP {code} {snippet}", req.url);
            if let Some(secs) = retry_after {
                cause.push_str(&format!(" (retry-after: {secs}s)"));
            }
            perr("PROVIDER_HTTP", cause, retryable)
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
    aux_vision_request(wire, model, prompt, Vec::new())
}

/// Build the wire request for one prompt plus image parts: no tools, no
/// streaming. Empty `images` degrades exactly to [`aux_request`] — the
/// adapters keep text-only rows byte-identical.
pub fn aux_vision_request(
    wire: &AuxWire,
    model: &str,
    prompt: String,
    images: Vec<pantheon_api::message::ImagePart>,
) -> WireRequest {
    let messages = vec![Message::user(prompt).with_images(images)];
    match wire.api_mode {
        ApiMode::OpenAi => openai::request(
            &wire.base,
            &wire.key,
            &wire.key_header,
            model,
            &messages,
            &[],
            false,
            // Aux turns carry their own fixed wire budget (resolve_aux_wire);
            // the session max_tokens directive never reaches them.
            wire.max_tokens,
            // Aux turns stay fast and cheap: no reasoning effort, ever.
            pantheon_api::model::ReasoningLevel::Off,
            &TurnOptions::default(),
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
            &TurnOptions::default(),
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
    ) -> Result<StreamEnd, PantheonError> {
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
    ) -> Result<StreamEnd, PantheonError> {
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
                    return Ok(StreamEnd::Done);
                }
            }
        }
        Ok(StreamEnd::Eof)
    }
}
