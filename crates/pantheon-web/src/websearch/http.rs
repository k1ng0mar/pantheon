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

/// ureq 2.12 has no dedicated timeout error kind: a timeout surfaces as a
/// transport error whose source chain bottoms out at an `io::Error` with
/// `ErrorKind::TimedOut`. Walk the chain so WEBSEARCH_TIMEOUT is accurate.
pub fn is_timeout(e: &ureq::Error) -> bool {
    let ureq::Error::Transport(t) = e else {
        return false;
    };
    let mut cur: Option<&dyn std::error::Error> = Some(t);
    while let Some(err) = cur {
        if let Some(ioe) = err.downcast_ref::<std::io::Error>() {
            if ioe.kind() == IoKind::TimedOut {
                return true;
            }
        }
        cur = err.source();
    }
    false
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new().timeout(REQUEST_TIMEOUT).build()
}

fn status_error(provider: &str, code: u16, r: ureq::Response) -> SearchError {
    let detail: String = r
        .into_string()
        .unwrap_or_default()
        .chars()
        .take(300)
        .collect();
    SearchError::RequestFailed(format!("{provider} HTTP {code}: {detail}"))
}

/// POST a JSON body with extra headers, returning the response text.
pub fn post_json(
    provider: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: serde_json::Value,
) -> Result<String, SearchError> {
    let mut req = agent().post(url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    let response = match req.send_json(body) {
        Ok(r) => r,
        Err(e) if is_timeout(&e) => return Err(SearchError::Timeout),
        Err(ureq::Error::Status(code, r)) => return Err(status_error(provider, code, r)),
        Err(ureq::Error::Transport(t)) => {
            let msg = t.message().unwrap_or("connection failed");
            return Err(SearchError::RequestFailed(format!(
                "{provider} transport error: {msg}"
            )));
        }
    };
    response
        .into_string()
        .map_err(|e| SearchError::BadResponse(format!("could not read response body: {e}")))
}

/// GET with extra headers, returning the response text.
pub fn get_json(
    provider: &str,
    url: &str,
    headers: &[(&str, &str)],
) -> Result<String, SearchError> {
    let mut req = agent().get(url);
    for (k, v) in headers {
        req = req.set(k, v);
    }
    let response = match req.call() {
        Ok(r) => r,
        Err(e) if is_timeout(&e) => return Err(SearchError::Timeout),
        Err(ureq::Error::Status(code, r)) => return Err(status_error(provider, code, r)),
        Err(ureq::Error::Transport(t)) => {
            let msg = t.message().unwrap_or("connection failed");
            return Err(SearchError::RequestFailed(format!(
                "{provider} transport error: {msg}"
            )));
        }
    };
    response
        .into_string()
        .map_err(|e| SearchError::BadResponse(format!("could not read response body: {e}")))
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

/// First text content item from an anonymous MCP `tools/call` response.
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
        Ok(texts
            .into_iter()
            .find(|t| !t.is_empty())
            .map(str::to_string))
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
