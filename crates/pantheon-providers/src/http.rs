//! HTTP transport for provider adapters (spec §5 + §14): one request shape
//! for the whole provider plane, plus SSE streaming so adapters can emit
//! normalized `ModelEvent`s as chunks arrive. Sync, rustls, no tokio.
//!
//! Fallback policy does NOT live here - see `chain.rs`. Adapters are
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
/// transport executes them - wire format never leaks into the chain.
#[derive(Debug, Clone)]
pub struct WireRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
    pub use_sigv4: bool,
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

/// Default provider HTTP timeout (120s) and its ceiling (1h): a
/// configured value above the ceiling is clamped, never a silent
/// forever-hang from a typo'd extra zero.
pub const DEFAULT_HTTP_TIMEOUT: Duration = Duration::from_secs(120);
pub const MAX_HTTP_TIMEOUT: Duration = Duration::from_secs(3600);

/// Resolve the provider HTTP timeout. Precedence: the
/// `PANTHEON_HTTP_TIMEOUT_MS` env var (when set to a positive number)
/// overrides everything; otherwise the `[model] timeout_ms` config
/// value (when positive); otherwise the 120s default. A zero or
/// unparseable value at one level falls through to the next, never to
/// a zero timeout. Values above the 1h ceiling are clamped to it.
/// Takes the env value as a parameter so the precedence is unit
/// testable without mutating process-global env in tests.
pub fn resolve_http_timeout(env_ms: Option<u64>, config_ms: Option<u64>) -> Duration {
    let chosen = env_ms
        .filter(|&ms| ms > 0)
        .or_else(|| config_ms.filter(|&ms| ms > 0));
    match chosen {
        Some(ms) => Duration::from_millis(ms).min(MAX_HTTP_TIMEOUT),
        None => DEFAULT_HTTP_TIMEOUT,
    }
}

/// Provider HTTP timeout, from the env var alone (config-file values
/// are resolved by the runtime session, which owns the data dir, and
/// passed in via [`resolve_http_timeout`] + the transport's `timeout`
/// field). Applies to single-shot calls as an overall deadline and
/// to streams as a per-read deadline so a hanging provider cannot
/// wedge the session thread forever. The turn watchdog covers process
/// liveness; this covers provider hang.
pub fn http_timeout() -> Duration {
    let env_ms = std::env::var("PANTHEON_HTTP_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok());
    resolve_http_timeout(env_ms, None)
}

/// Shared request agent honoring [`http_timeout`]. Every provider-plane
/// HTTP call (chat, aux, voice, embeddings) goes through this so none
/// runs without a deadline. Statuses are responses, not errors: the
/// error mapping below reads bodies and Retry-After headers.
pub fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(http_timeout()))
        .max_redirects(0)
        .max_redirects_will_error(false)
        .http_status_as_error(false)
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                .build(),
        )
        .build()
        .new_agent()
}

/// Egress policy for outbound provider requests.
#[derive(Debug, Clone)]
pub struct EgressPolicy {
    /// `Off` = no restriction, `Allowlist` = only listed hosts, `Offline` = deny all.
    pub mode: EgressMode,
    /// Host suffixes allowed when mode is `Allowlist`. Empty = deny all.
    pub allowlist: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressMode {
    Off,
    Allowlist,
    Offline,
}

impl Default for EgressPolicy {
    fn default() -> Self {
        Self {
            mode: EgressMode::Off,
            allowlist: Vec::new(),
        }
    }
}

static EGRESS: std::sync::RwLock<EgressPolicy> = std::sync::RwLock::new(EgressPolicy {
    mode: EgressMode::Off,
    allowlist: Vec::new(),
});

/// Install the egress policy. Called at CLI startup from config.
pub fn set_egress_policy(policy: EgressPolicy) {
    if let Ok(mut guard) = EGRESS.write() {
        *guard = policy;
    }
}

/// Read the current egress policy.
pub fn egress_policy() -> EgressPolicy {
    EGRESS.read().map(|g| g.clone()).unwrap_or_default()
}

/// Check whether `host` is permitted by the policy.
fn egress_allows_host(host: &str) -> bool {
    let policy = egress_policy();
    match policy.mode {
        EgressMode::Off => true,
        EgressMode::Offline => false,
        EgressMode::Allowlist => {
            let host = host.to_lowercase();
            policy.allowlist.iter().any(|allowed| {
                let allowed = allowed.to_lowercase();
                host == allowed || host.ends_with(&format!(".{allowed}"))
            })
        }
    }
}

/// SSRF guard: resolve the URL's host and reject anything that is not a
/// public IP. Called on every outbound provider request before ureq runs.
/// Also enforces the egress policy (allowlist/offline).
pub fn validate_url(url: &str) -> Result<(), PantheonError> {
    let err = |code: &str, cause: String| {
        PantheonError::new(
            code,
            pantheon_api::error::Layer::Provider,
            false,
            cause,
            "remove the private URL from config or use a public provider",
            url.to_string(),
        )
    };
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return Err(err("PROVIDER_URL", format!("invalid URL: {url}"))),
    };
    let host = match parsed.host_str() {
        Some(h) => h,
        None => return Err(err("PROVIDER_URL", format!("no host in URL: {url}"))),
    };
    if !egress_allows_host(host) {
        return Err(err(
            "EGRESS_BLOCKED",
            format!("{host} is not permitted by egress policy"),
        ));
    }
    let port = parsed.port_or_known_default().unwrap_or(0);
    use std::net::ToSocketAddrs;
    let addrs: Vec<std::net::IpAddr> = match (host, port).to_socket_addrs() {
        Ok(iter) => iter.map(|s| s.ip()).collect(),
        Err(e) => return Err(err("PROVIDER_DNS", format!("cannot resolve {host}: {e}"))),
    };
    if addrs.is_empty() {
        return Err(err(
            "PROVIDER_DNS",
            format!("no addresses for host: {host}"),
        ));
    }
    for ip in &addrs {
        if !is_public_ip(*ip) {
            return Err(err(
                "SSRF_BLOCKED",
                format!("{host} resolves to private address {ip}"),
            ));
        }
    }
    Ok(())
}

fn is_public_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => is_public_v4(v4),
        std::net::IpAddr::V6(v6) => {
            // IPv4-mapped (::ffff:a.b.c.d) and IPv4-compatible
            // (::a.b.c.d) addresses route to the IPv4 host: judge them
            // by the IPv4 rules, or ::ffff:127.0.0.1 sails past the V6
            // branch untouched (same hole as the link-preview guard).
            if let Some(v4) = v6.to_ipv4() {
                return is_public_v4(v4);
            }
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || v6.is_unicast_link_local()
                || is_unique_local_v6(v6))
        }
    }
}

fn is_public_v4(v4: std::net::Ipv4Addr) -> bool {
    if v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_documentation()
        || v4.is_unspecified()
    {
        return false;
    }
    let o = v4.octets();
    // Carrier-grade NAT: 100.64.0.0/10.
    if o[0] == 100 && (64..128).contains(&o[1]) {
        return false;
    }
    // IETF protocol assignments: 192.0.0.0/24.
    if o[0] == 192 && o[1] == 0 && o[2] == 0 {
        return false;
    }
    // Benchmarking: 198.18.0.0/15.
    if o[0] == 198 && (o[1] == 18 || o[1] == 19) {
        return false;
    }
    // Reserved: 240.0.0.0/4.
    if o[0] >= 240 {
        return false;
    }
    true
}

fn is_unique_local_v6(ip: std::net::Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xfe00) == 0xfc00
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
/// (`Sun, 06 Nov 1994 08:49:37 GMT`); anything else is `None` - no wait
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

/// Wire attribution headers stamped on every outbound provider request.
/// Lets providers identify traffic coming from pantheon (abuse tracking,
/// quota attribution, debugging) without parsing user agents.
pub fn attribution_headers() -> Vec<(String, String)> {
    vec![
        ("x-app".into(), "pantheon".into()),
        ("x-client-version".into(), env!("CARGO_PKG_VERSION").into()),
    ]
}

fn send(
    agent: &ureq::Agent,
    req: &WireRequest,
) -> Result<ureq::http::Response<ureq::Body>, PantheonError> {
    validate_url(&req.url)?;
    let mut r = agent.post(&req.url);
    if req.use_sigv4 {
        if let Some(creds) = crate::sigv4::SigV4Creds::from_env() {
            let region = req.url.split('.').nth(1).unwrap_or("us-east-1");
            let auth = crate::sigv4::sign_request(
                "POST",
                &req.url,
                "",
                &req.headers,
                req.body.as_bytes(),
                region,
                "bedrock",
                &creds,
            );
            r = r.header("Authorization", auth.as_str());
        }
    }
    for (k, v) in &req.headers {
        r = r.header(k.as_str(), v.as_str());
    }
    for (k, v) in attribution_headers() {
        r = r.header(k.as_str(), v.as_str());
    }
    let mut resp = r
        .send(req.body.as_str())
        .map_err(|e| perr("PROVIDER_HTTP", format!("{}: {e}", req.url), true))?;
    let code = resp.status().as_u16();
    if code >= 400 {
        // A 429 carries the provider's asked-for wait: parse
        // `Retry-After` (delta-seconds or HTTP-date, capped) and stamp
        // it on the cause so the chain can sleep before rotating keys
        // or falling back instead of hammering a rate-limited endpoint.
        let retry_after = if code == 429 {
            resp.headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(parse_retry_after)
        } else {
            None
        };
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        let snippet: String = body.chars().take(300).collect();
        // 5xx / 408-class server trouble and transport errors are
        // retryable (fallback-eligible); 4xx is a client/config problem
        // - except 429, which is transient: another key or provider may
        // still have quota, and the stamped Retry-After paces the retry.
        let retryable = !(400..500).contains(&code) || code == 429;
        let mut cause = format!("{}: HTTP {code} {snippet}", req.url);
        if let Some(secs) = retry_after {
            cause.push_str(&format!(" (retry-after: {secs}s)"));
        }
        return Err(perr("PROVIDER_HTTP", cause, retryable));
    }
    Ok(resp)
}

/// Resolve a provider's effective base URL, interpolating `{var}`
/// endpoint templates (azure resource, bedrock region, vertex
/// project/location, cloudflare account). Missing values are a
/// *config* error (`PROVIDER_CONFIG`, fail-fast with remediation),
/// never a network error - a raw `{placeholder}` must not reach the wire.
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
    pub provider_id: String,
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
        provider_id: provider_id.to_string(),
    })
}

/// Build the wire request for one prompt: no tools, no streaming.
pub fn aux_request(wire: &AuxWire, model: &str, prompt: String) -> WireRequest {
    aux_vision_request(wire, model, prompt, Vec::new())
}

/// Build the wire request for one prompt plus image parts: no tools, no
/// streaming. Empty `images` degrades exactly to [`aux_request`] - the
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
            &wire.provider_id,
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
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(self.timeout))
            .max_redirects(0)
            .max_redirects_will_error(false)
            .http_status_as_error(false)
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .new_agent();
        let resp = send(&agent, req)?;
        resp.into_body()
            .read_to_string()
            .map_err(|e| perr("PROVIDER_READ", e.to_string(), true))
    }

    fn post_stream(
        &self,
        req: &WireRequest,
        on_payload: &mut dyn FnMut(&str) -> Result<(), PantheonError>,
    ) -> Result<StreamEnd, PantheonError> {
        // No total timeout: a stream may run long, and a slow-but-alive
        // provider must not be cut off mid-answer. NOTE the old code
        // capped streams at 15s via `timeout_per_call`, believing it
        // covered only "connect through response headers" - that is
        // wrong for ureq 3.5.0: `timeout_per_call` is the max total
        // duration for one HTTP exchange, headers AND body included,
        // which is exactly how long streamed answers were being cut
        // off with PROVIDER_STREAM_INTERRUPTED after partial output.
        // Hang protection for a stream is therefore the per-read
        // deadline (each transport read must complete within
        // `self.timeout`; a provider that stops sending trips it) plus
        // the connection-setup deadline, plus the existing turn
        // watchdog for process liveness - never a wall-clock cap on a
        // live stream. Single-shot `post` above keeps its overall
        // `timeout_global` deadline.
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_connect(Some(self.timeout))
            .timeout_per_read(Some(self.timeout))
            .max_redirects(0)
            .max_redirects_will_error(false)
            .http_status_as_error(false)
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::PlatformVerifier)
                    .build(),
            )
            .build()
            .new_agent();
        let resp = send(&agent, req)?;
        let reader = BufReader::new(resp.into_body().into_reader());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_default_when_nothing_set() {
        assert_eq!(resolve_http_timeout(None, None), DEFAULT_HTTP_TIMEOUT);
    }

    #[test]
    fn timeout_config_value_used_when_env_absent() {
        assert_eq!(
            resolve_http_timeout(None, Some(30_000)),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn timeout_env_overrides_config() {
        assert_eq!(
            resolve_http_timeout(Some(45_000), Some(30_000)),
            Duration::from_secs(45)
        );
    }

    #[test]
    fn timeout_zero_falls_through_to_next_level() {
        // Zero env -> config wins; zero config -> default wins. A zero
        // timeout must never mean "no deadline" by accident.
        assert_eq!(
            resolve_http_timeout(Some(0), Some(30_000)),
            Duration::from_secs(30)
        );
        assert_eq!(resolve_http_timeout(None, Some(0)), DEFAULT_HTTP_TIMEOUT);
        assert_eq!(resolve_http_timeout(Some(0), Some(0)), DEFAULT_HTTP_TIMEOUT);
    }

    #[test]
    fn timeout_clamped_to_one_hour_ceiling() {
        assert_eq!(
            resolve_http_timeout(None, Some(10_000_000)),
            MAX_HTTP_TIMEOUT
        );
    }
}
