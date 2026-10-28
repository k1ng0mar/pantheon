//! Shared blocking HTTP plumbing for search providers.
//!
//! All providers go through [`post_json`] / [`get_json`]: a 30s timeout,
//! accurate timeout errors, and failure descriptions that name the HTTP
//! status plus a truncated slice of the response body - never request
//! material (bodies carry API keys, headers carry tokens).

use super::error::SearchError;
use std::io::ErrorKind as IoKind;
use std::time::Duration;

/// Per-request upper bound shared by every search provider (all providers,
/// Tavily included, go through [`post_json`] / [`get_json`]).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// ureq 3 reports timeouts as a dedicated `Error::Timeout` variant; any
/// error funneled through an `io::Error` keeps the `TimedOut` kind. Cover
/// both so WEBSEARCH_TIMEOUT is accurate.
pub fn is_timeout(e: &ureq::Error) -> bool {
    match e {
        ureq::Error::Timeout(_) => true,
        ureq::Error::Io(ioe) => ioe.kind() == IoKind::TimedOut,
        _ => false,
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(REQUEST_TIMEOUT))
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

/// Credential diagnosis for a search-provider failure, mirroring
/// `pantheon_providers::credential_error::classify` (this crate does
/// not depend on pantheon-providers, so the three static mappings
/// live here too; keep them in sync). The diagnosis is static text
/// only, never the body, so no credential can pass through it.
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

fn status_error(provider: &str, code: u16, r: ureq::http::Response<ureq::Body>) -> SearchError {
    let detail: String = r
        .into_body()
        .read_to_string()
        .unwrap_or_default()
        .chars()
        .take(300)
        .collect();
    let mut message = format!("{provider} HTTP {code}: {detail}");
    if let Some(d) = credential_diagnosis(code, &detail) {
        message.push_str(&format!(" [credential: {d}]"));
    }
    SearchError::RequestFailed(message)
}

/// Branded User-Agent for search requests, matching the provider
/// transport: edge checks rejecting default agents must not block us.
fn user_agent() -> String {
    format!("Pantheon/{}", env!("CARGO_PKG_VERSION"))
}

/// `Retry-After` delta-seconds from a response, if present and sane.
/// HTTP-date form is not parsed (same stance as the gateway channel
/// transports). Capped at 10s: a longer hint still earns one retry
/// after a bounded wait, not a hung tool call.
fn retry_after_secs(response: &ureq::http::Response<ureq::Body>) -> Option<u64> {
    let v = response
        .headers()
        .get("retry-after")
        .and_then(|h| h.to_str().ok())?;
    let secs: u64 = v.trim().parse().ok()?;
    if secs == 0 {
        None
    } else {
        Some(secs.min(10))
    }
}

/// POST a JSON body with extra headers, returning the response text.
///
/// On a 429 with a `Retry-After` hint, waits (capped at 10s) and retries
/// once before reporting the rate limit. Previously no provider honoured
/// `Retry-After` at all, so a throttled call failed immediately even
/// when the provider said exactly how long to wait.
pub fn post_json(
    provider: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: serde_json::Value,
) -> Result<String, SearchError> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let mut req = agent().post(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        req = req.header("User-Agent", user_agent().as_str());
        let response = match req.send_json(body.clone()) {
            Ok(r) => r,
            Err(e) if is_timeout(&e) => return Err(SearchError::Timeout),
            Err(e) => {
                return Err(SearchError::RequestFailed(format!(
                    "{provider} transport error: {e}"
                )));
            }
        };
        let code = response.status().as_u16();
        if code == 429 && attempt == 1 {
            if let Some(secs) = retry_after_secs(&response) {
                std::thread::sleep(std::time::Duration::from_secs(secs));
                continue;
            }
        }
        if code >= 400 {
            return Err(status_error(provider, code, response));
        }
        return response
            .into_body()
            .read_to_string()
            .map_err(|e| SearchError::BadResponse(format!("could not read response body: {e}")));
    }
}

/// GET with extra headers, returning the response text. Same one-retry
/// `Retry-After` behaviour as [`post_json`].
pub fn get_json(
    provider: &str,
    url: &str,
    headers: &[(&str, &str)],
) -> Result<String, SearchError> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let mut req = agent().get(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        req = req.header("User-Agent", user_agent().as_str());
        let response = match req.call() {
            Ok(r) => r,
            Err(e) if is_timeout(&e) => return Err(SearchError::Timeout),
            Err(e) => {
                return Err(SearchError::RequestFailed(format!(
                    "{provider} transport error: {e}"
                )));
            }
        };
        let code = response.status().as_u16();
        if code == 429 && attempt == 1 {
            if let Some(secs) = retry_after_secs(&response) {
                std::thread::sleep(std::time::Duration::from_secs(secs));
                continue;
            }
        }
        if code >= 400 {
            return Err(status_error(provider, code, response));
        }
        return response
            .into_body()
            .read_to_string()
            .map_err(|e| SearchError::BadResponse(format!("could not read response body: {e}")));
    }
}

/// Minimal percent-encoding for query strings and path segments: keeps
/// unreserved ASCII, `%`-escapes everything else (UTF-8 bytes included).
pub fn percent_encode(s: &str) -> String {
    const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.~";
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if UNRESERVED.contains(&b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(
                char::from_digit(u32::from(b >> 4), 16)
                    .unwrap_or('0')
                    .to_ascii_uppercase(),
            );
            out.push(
                char::from_digit(u32::from(b & 0xF), 16)
                    .unwrap_or('0')
                    .to_ascii_uppercase(),
            );
        }
    }
    out
}

/// Text content of an anonymous MCP `tools/call` response: every
/// non-empty text block, joined with newlines.
///
/// Vendors answer in two framings: plain JSON bodies (Parallel) or SSE
/// `data: {...}` lines (Exa). Split only on SSE line terminators
/// (CR/LF) - never on Unicode line breaks, which legitimately occur
/// inside CJK page text. A JSON-RPC `error` or an `isError` tool result
/// (free-tier rate limit) is an error, not an empty result.
pub fn mcp_text_payload(body: &str) -> Result<String, super::error::SearchError> {
    use super::error::SearchError;

    fn from_payload(payload: &str) -> Result<Option<String>, SearchError> {
        let payload = payload.trim();
        if !payload.starts_with('{') {
            return Ok(None);
        }
        let data: serde_json::Value = serde_json::from_str(payload)
            .map_err(|_| SearchError::BadResponse("unparseable MCP envelope".to_string()))?;
        if let Some(err) = data.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("MCP error");
            return Err(SearchError::RequestFailed(format!("MCP error: {msg}")));
        }
        let result = data.get("result");
        let texts: Vec<&str> = result
            .and_then(|r| r.get("content"))
            .and_then(|c| c.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
                    .collect()
            })
            .unwrap_or_default();
        if result
            .and_then(|r| r.get("isError"))
            .and_then(|v| v.as_bool())
            == Some(true)
        {
            let msg = texts.join(" ");
            return Err(SearchError::RequestFailed(if msg.trim().is_empty() {
                "MCP tool call failed".to_string()
            } else {
                msg
            }));
        }
        // Concatenate every non-empty text block: taking only the
        // first silently dropped later blocks (e.g. multi-part MCP
        // results), which parsers then read as a truncated document.
        let joined = texts
            .into_iter()
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        Ok(if joined.is_empty() {
            None
        } else {
            Some(joined)
        })
    }

    let stripped = body.trim();
    let mut candidates: Vec<&str> = Vec::new();
    if stripped.starts_with('{') {
        candidates.push(stripped);
    }
    // SSE framing: split on transport terminators only.
    let mut rest = body;
    while let Some(pos) = rest.find(['\r', '\n']) {
        let (line, tail) = rest.split_at(pos);
        if let Some(payload) = line.strip_prefix("data: ") {
            candidates.push(payload);
        }
        rest = &tail[1..];
    }
    if let Some(payload) = rest.strip_prefix("data: ") {
        candidates.push(payload);
    }
    let mut envelope_seen = false;
    for candidate in candidates {
        match from_payload(candidate)? {
            Some(text) => return Ok(text),
            None => {
                envelope_seen = true;
                continue;
            }
        }
    }
    if envelope_seen {
        return Err(SearchError::BadResponse(
            "MCP envelope parsed but carried no text".to_string(),
        ));
    }
    Err(SearchError::BadResponse(
        "MCP response contained no text content".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnosis_maps_1010_401_403_and_nothing_else() {
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
        assert!(credential_diagnosis(500, "error").is_none());
    }

    #[test]
    fn websearch_user_agent_is_branded() {
        assert!(user_agent().starts_with("Pantheon/"));
    }
}
