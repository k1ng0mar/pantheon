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
