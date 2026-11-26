//! Shared blocking HTTP for the search providers. Everything funnels through
//! [`post_json`] / [`get_json`]: 30s timeout, accurate timeout errors, and
//! failure descriptions that name the status plus a truncated slice of the
//! response body — never request material (bodies carry API keys, headers
//! carry tokens).

use super::error::SearchError;
use std::io::ErrorKind as IoKind;
use std::time::Duration;

/// per-request bound shared by every provider (Tavily included)
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// ureq 3 gives a dedicated `Error::Timeout`; an `io::Error` keeps the
/// `TimedOut` kind. Cover both so WEBSEARCH_TIMEOUT is accurate.
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

/// credential diagnosis mirroring `pantheon_providers::credential_error::classify`
/// (this crate doesn't depend on pantheon-providers, so the three static
/// mappings live here too; keep in sync). Static text only, never the body.
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

/// branded User-Agent so edge checks rejecting default agents don't block us
fn user_agent() -> String {
    format!("Pantheon/{}", env!("CARGO_PKG_VERSION"))
}

/// `Retry-After` delta-seconds, if sane. HTTP-date form isn't parsed (same
/// stance as gateway channel transports). Capped at 10s: long hints still
/// get one retry after a bounded wait, not a hung tool call.
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

/// POST JSON, returns response text. On 429 with a `Retry-After` hint:
/// wait (capped at 10s) and retry once — provider told us how long to wait.
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

/// GET, returns response text. Same one-retry `Retry-After` as [`post_json`].
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

/// minimal percent-encoding: keep unreserved ASCII, `%`-escape the rest
/// (UTF-8 bytes included)
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

/// Text content of an anonymous MCP `tools/call`: every non-empty text block,
/// joined with newlines.
///
/// Two framings in the wild: plain JSON (Parallel) and SSE `data: {...}` lines
/// (Exa). Split only on SSE terminators (CR/LF) — never Unicode line breaks,
/// those occur legitimately inside CJK text. A JSON-RPC `error` or `isError`
/// tool result (free-tier rate limit) is an error, not an empty result.
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
        // join every non-empty block: taking only the first silently
        // dropped later blocks (multi-part MCP results), and parsers then
        // read a truncated document
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
    // SSE framing: split on transport terminators only
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
