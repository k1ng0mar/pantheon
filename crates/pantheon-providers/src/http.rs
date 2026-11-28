//! HTTP transport for provider adapters (spec §5 + §14): one request shape
//! for the whole provider plane, plus SSE streaming so adapters can emit
//! normalized `ModelEvent`s as chunks arrive. Sync, rustls, no tokio.
//!
//! Fallback policy does NOT live here - see `chain.rs`. Adapters are
//! single-attempt; the chain decides what happens on failure.
//!
//! Provider endpoints are operator config: `validate_url` checks the URL
//! shape and the configured egress policy, then gets out of the way. Local
//! routers and fake providers on loopback work without a special opt in.

use crate::catalog::{self, ApiMode};
use crate::model_event::{ModelUsage, NoopModelSink};
use crate::{anthropic, openai};
use pantheon_agent::TurnOutcome;
use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::message::Message;
use std::io::{BufRead, BufReader};
use std::time::Duration;

/// Upper bound for one SSE event's accumulated `data:` payload.
const MAX_SSE_EVENT_BYTES: usize = 16 * 1024 * 1024;

/// Redact values we actually sent before an error body can carry them back.
/// Pattern redaction alone misses custom provider keys with no known prefix,
/// so scrub every credential-shaped header value out of the cause we create.
fn scrub_error_text(text: &str, headers: &[(String, String)]) -> String {
    let mut out = text.to_string();
    for (name, value) in headers {
        let secret_header = name.eq_ignore_ascii_case("authorization")
            || name.eq_ignore_ascii_case("proxy-authorization")
            || name.eq_ignore_ascii_case("x-api-key")
            || name.to_ascii_lowercase().contains("token")
            || name.to_ascii_lowercase().contains("key");
        if secret_header && !value.is_empty() {
            out = out.replace(value.as_str(), "[redacted]");
            // An error body usually echoes the token alone, not the
            // `Bearer <token>` header value, so scrub the credential part
            // too. Short values stay untouched: replacing a 3-char token
            // would shred the message without protecting anything.
            if let Some((_scheme, rest)) = value.split_once(' ') {
                let rest = rest.trim();
                if rest.len() >= 8 {
                    out = out.replace(rest, "[redacted]");
                }
            }
        }
    }
    pantheon_api::logging::redact(&out)
}

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
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
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

/// Per-turn wire knobs, threaded from the chain into both adapters. Everything
/// defaults to "as before", so a turn built without options produces the same bytes.
#[derive(Debug, Clone, Default)]
pub struct TurnOptions {
    /// Opt-in structured output: JSON Schema the response must conform
    /// to. `None` sends no schema constraint.
    pub response_schema: Option<serde_json::Value>,
    /// Tool-call policy for this turn.
    pub tool_choice: ToolChoice,
    /// Cooperative cancel token checked per SSE line. None means no cancel.
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
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

/// Provider URLs are trusted operator config. Check the shape and the
/// explicit egress policy; do not second guess the address the operator
/// chose. This is the same posture Hermes takes with custom endpoints.
pub fn validate_url(url: &str) -> Result<(), PantheonError> {
    let err = |code: &str, cause: String| {
        PantheonError::new(
            code,
            pantheon_api::error::Layer::Provider,
            false,
            cause,
            "check the provider URL and egress policy",
            url.to_string(),
        )
    };
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return Err(err("PROVIDER_URL", format!("invalid URL: {url}"))),
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(err(
            "PROVIDER_URL",
            format!(
                "provider URL must use http or https, got {}",
                parsed.scheme()
            ),
        ));
    }
    let host = match parsed.host_str() {
        Some(h) if !h.is_empty() => h,
        _ => return Err(err("PROVIDER_URL", format!("no host in URL: {url}"))),
    };
    if !egress_allows_host(host) {
        return Err(err(
            "EGRESS_BLOCKED",
            format!("{host} is not permitted by egress policy"),
        ));
    }
    Ok(())
}

/// `(name, value)` for the key header: `Authorization` sends
/// `Bearer <key>`; any other configured name (e.g. Xiaomi MiMo's
/// `api-key`) sends the raw key. One place so chat, embeddings, and
/// voice can't drift on auth shape.
pub fn auth_header_pair(key_header: &str, key: &str) -> (String, String) {
    let name = if key_header.trim().is_empty() {
        "Authorization".to_string()
    } else {
        key_header.trim().to_string()
    };
    let value = if name.eq_ignore_ascii_case("authorization") {
        format!("Bearer {key}")
    } else {
        key.to_string()
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

/// Normalize streamed/single-shot tool arguments. Empty (an
/// argument-less call) becomes `"{}"`, matching the Anthropic finish
/// path and both single-shot parsers; the OpenAI streamed path used
/// to leave `""`, which failed JSON parsing downstream. Invalid JSON
/// is never passed to the tool runner as an ordinary tool error: on a
/// `length`/`max_tokens` finish it is a truncation
/// (`PROVIDER_TRUNCATED`, retryable: the args were cut off, not
/// wrong), otherwise a parse error.
pub fn normalise_tool_args(
    args: &str,
    finish_reason: Option<&str>,
) -> Result<String, PantheonError> {
    if args.trim().is_empty() {
        return Ok("{}".to_string());
    }
    if serde_json::from_str::<serde_json::Value>(args).is_ok() {
        return Ok(args.to_string());
    }
    match finish_reason {
        Some("length") | Some("max_tokens") => Err(perr(
            "PROVIDER_TRUNCATED",
            "streamed tool arguments are not valid JSON (output limit hit mid-call)".into(),
            true,
        )),
        _ => Err(perr(
            "PROVIDER_PARSE",
            "streamed tool arguments are not valid JSON".into(),
            false,
        )),
    }
}

/// Retryability of an HTTP status for `send()`: 5xx / 408-class server trouble and
/// transport errors are retryable; 4xx is a client/config problem, except 429, which
/// is transient.
pub fn is_retryable_status(code: u16) -> bool {
    !(400..500).contains(&code) || code == 429 || code == 408
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
        // Disabled until it is correct (audit COR-12). The hand-rolled
        // signer computes the date with 365-day years and 30-day months,
        // signs the full URL as the canonical URI, signs `x-amz-date` and
        // the session token without sending them, and the request went out
        // unsigned when credentials were missing: every real Bedrock call
        // failed with an opaque 403. Fail loudly instead.
        return Err(PantheonError::new(
            "PROVIDER_SIGV4_UNSUPPORTED",
            pantheon_api::error::Layer::Provider,
            false,
            "AWS SigV4 request signing (Amazon Bedrock) is not supported in this build".to_string(),
            "use Bedrock through an OpenAI-compatible gateway, or another provider",
            req.url.clone(),
        ));
    }
    for (k, v) in &req.headers {
        r = r.header(k.as_str(), v.as_str());
    }
    for (k, v) in attribution_headers() {
        r = r.header(k.as_str(), v.as_str());
    }
    // Branded User-Agent: edge checks that reject default or missing
    // User-Agents (Cloudflare error 1010, see credential_error) must
    // not block provider requests. Applied last so it wins over any
    // caller-supplied User-Agent.
    r = r.header("User-Agent", crate::credential_error::user_agent().as_str());
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
        let retryable = is_retryable_status(code);
        let mut cause = format!("{}: HTTP {code} {snippet}", req.url);
        if let Some(secs) = retry_after {
            cause.push_str(&format!(" (retry-after: {secs}s)"));
        }
        // Actionable diagnosis, advisory only: retryability above is
        // unchanged. The diagnosis is static text, never the body or
        // the URL, so no credential can pass through it.
        if let Some(d) = crate::credential_error::classify(code, &body) {
            cause.push_str(&format!(" [credential: {} {}]", d.summary, d.remediation));
        }
        return Err(perr(
            "PROVIDER_HTTP",
            scrub_error_text(&cause, &req.headers),
            retryable,
        ));
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
        ApiMode::Gemini => crate::gemini::request(
            &wire.base,
            &wire.key,
            model,
            &messages,
            &[],
            false,
            wire.max_tokens,
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
        // Native Gemini speaks a different wire shape, but its adapter's
        // `complete` normalizes back into the same AdapterTurn, so aux turns
        // (title, judge, compression, vision) work on a native Gemini pin
        // instead of failing the whole call.
        ApiMode::Gemini => crate::gemini::complete(transport, req, &NoopModelSink),
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
        // No total timeout: a stream may run long, and a slow-but-alive provider must
        // not be cut off mid-answer. `timeout_per_call` cannot be used here - in ureq
        // 3.5.0 it caps the max total duration of one HTTP exchange, headers and body
        // included, which cuts streamed answers off with PROVIDER_STREAM_INTERRUPTED
        // after partial output.
        //
        // Hang protection for a stream is the per-read deadline (each transport read
        // completes within `self.timeout`), the connection-setup deadline, and the turn
        // watchdog for process liveness. Never a wall-clock cap on a live stream.
        // Single-shot `post` above keeps its `timeout_global` deadline.
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
        // Ask for SSE explicitly: some gateways default to a buffered
        // non-streaming body without it.
        let mut req2 = req.clone();
        if !req2
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("accept"))
        {
            req2.headers
                .push(("Accept".into(), "text/event-stream".into()));
        }
        let resp = send(&agent, &req2)?;
        let reader = BufReader::new(resp.into_body().into_reader());
        // SSE events are separated by blank lines, and one event's `data:` field may
        // span several lines (joined with `\n`). Accumulate consecutive `data:` lines
        // and deliver one payload per event; a buffer that already parses as complete
        // JSON is flushed before the next `data:` line, so providers that omit the blank
        // separator still deliver promptly. `[DONE]` is handled here, before adapter
        // parsing, and fully trimmed - adapters would otherwise try to JSON-parse it.
        // `lines()` only yields a chunk once its newline arrives, so a provider sending
        // JSON with no trailing newline stalls until EOF; fixing that needs a byte-level
        // reader.
        let mut pending = String::new();
        let flush = |pending: &mut String,
                     on_payload: &mut dyn FnMut(&str) -> Result<(), PantheonError>|
         -> Result<bool, PantheonError> {
            if pending.is_empty() {
                return Ok(false);
            }
            let payload = pending.trim().to_string();
            pending.clear();
            if payload.is_empty() {
                return Ok(false);
            }
            if payload == "[DONE]" {
                return Ok(true);
            }
            on_payload(&payload)?;
            Ok(false)
        };
        for line in reader.lines() {
            if let Some(cancel) = &req.cancel {
                if cancel.load(std::sync::atomic::Ordering::SeqCst) {
                    return Err(perr(
                        "PROVIDER_CANCELED",
                        "stream canceled by operator".to_string(),
                        false,
                    ));
                }
            }
            // Cooperative cancel is checked per line, not per turn. HttpTransport
            // receives cancel via the on_payload closure in adapters.
            // Adapters already thread their own cancel from TurnOptions.
            let line = line.map_err(|e| perr("PROVIDER_READ", e.to_string(), true))?;
            let line = line.trim_end_matches('\r');
            if line.is_empty() || line.starts_with(':') || line.starts_with("event:") {
                // Blank line / comment / event tag ends the current event.
                if flush(&mut pending, on_payload)? {
                    return Ok(StreamEnd::Done);
                }
                continue;
            }
            if let Some(payload) = line.strip_prefix("data:") {
                let payload = payload.trim_start();
                if payload.is_empty() {
                    continue;
                }
                if !pending.is_empty()
                    && serde_json::from_str::<serde_json::Value>(pending.trim()).is_ok()
                    && flush(&mut pending, on_payload)?
                {
                    return Ok(StreamEnd::Done);
                }
                if !pending.is_empty() {
                    pending.push('\n');
                }
                pending.push_str(payload);
                // One event is bounded: a provider that never ends its
                // `data:` event grew this buffer until memory ran out
                // (audit COR-11).
                if pending.len() > MAX_SSE_EVENT_BYTES {
                    return Err(perr(
                        "PROVIDER_STREAM_TOO_LARGE",
                        format!(
                            "one streamed event exceeded {MAX_SSE_EVENT_BYTES} bytes without ending"
                        ),
                        false,
                    ));
                }
            }
        }
        if flush(&mut pending, on_payload)? {
            return Ok(StreamEnd::Done);
        }
        Ok(StreamEnd::Eof)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The original defect: every aux turn on a native Gemini pin failed with
    /// `PROVIDER_NOT_IMPLEMENTED`, taking title generation, compression and
    /// the judge down with it. This pins that the Gemini arm now dispatches
    /// into the real adapter instead of erroring out.
    #[test]
    fn aux_complete_dispatches_native_gemini() {
        struct StubTransport;
        impl ChatTransport for StubTransport {
            fn post(&self, req: &WireRequest) -> Result<String, PantheonError> {
                // The adapter must have built a real generateContent body,
                // not the empty-contents placeholder.
                let body: serde_json::Value = serde_json::from_str(&req.body)
                    .map_err(|e| perr("STUB", e.to_string(), false))?;
                assert_ne!(
                    body["contents"],
                    serde_json::json!([]),
                    "aux request must carry the prompt, not empty contents"
                );
                Ok(r#"{"candidates":[{"content":{"parts":[{"text":"ok"}]},"finishReason":"STOP"}]}"#.to_string())
            }
            fn post_stream(
                &self,
                _req: &WireRequest,
                _on_payload: &mut dyn FnMut(&str) -> Result<(), PantheonError>,
            ) -> Result<StreamEnd, PantheonError> {
                Err(perr("STUB", "aux never streams".into(), false))
            }
        }

        let wire = AuxWire {
            base: "https://generativelanguage.googleapis.com/v1beta".into(),
            key: "k".into(),
            key_header: "x-goog-api-key".into(),
            api_mode: ApiMode::Gemini,
            max_tokens: 64,
            provider_id: "gemini".into(),
        };
        let req = aux_request(&wire, "gemini-2.5-flash", "say ok".to_string());
        let turn = aux_complete(&StubTransport, &wire, req).expect("aux must succeed");
        match turn.outcome {
            pantheon_agent::TurnOutcome::Text { text, .. } => assert_eq!(text, "ok"),
            other => panic!("expected a text turn, got {other:?}"),
        }
    }

    #[test]
    fn retryable_status_covers_408_and_5xx_not_other_4xx() {
        assert!(is_retryable_status(408));
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(500));
        assert!(is_retryable_status(503));
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(401));
        assert!(!is_retryable_status(404));
    }

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

    #[test]
    fn provider_endpoints_are_trusted_config() {
        for url in [
            "http://127.0.0.1:8015/v1",
            "http://localhost:11434/v1",
            "http://192.168.1.10:8000/v1",
            "https://api.openai.com/v1",
        ] {
            validate_url(url).unwrap_or_else(|e| panic!("{url}: {e}"));
        }
    }

    #[test]
    fn invalid_provider_url_is_rejected() {
        let err = validate_url("not a url").unwrap_err();
        assert_eq!(err.code, "PROVIDER_URL");
    }

    #[test]
    fn egress_policy_still_applies() {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = egress_policy();
        set_egress_policy(EgressPolicy {
            mode: EgressMode::Allowlist,
            allowlist: vec!["api.openai.com".into()],
        });
        assert!(validate_url("https://api.openai.com/v1").is_ok());
        let err = validate_url("https://example.com/v1").unwrap_err();
        assert_eq!(err.code, "EGRESS_BLOCKED");
        set_egress_policy(saved);
    }

    #[test]
    fn provider_error_causes_scrub_the_key_we_sent() {
        let key = "opaque-custom-key-1234567890";
        let headers = vec![
            ("Authorization".to_string(), format!("Bearer {key}")),
            ("Content-Type".to_string(), "application/json".into()),
        ];
        let cause = format!("provider echoed {key} in a 400 body");
        let safe = scrub_error_text(&cause, &headers);
        assert!(
            !safe.contains(key),
            "secret reached the error cause: {safe}"
        );
        assert!(safe.contains("[redacted]"));
    }
}
