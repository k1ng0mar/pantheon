//! Search errors and their mapping to structured `PantheonError`s.
//!
//! Invariant: the API key never appears in an error message. Bodies carry the
//! key, so failures name the class (HTTP status, transport, timeout,
//! unreadable payload) without echoing anything sent.

use pantheon_api::error::{Layer, PantheonError};
use std::fmt;

/// what can go wrong in a search call
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchError {
    /// no key configured; tool not registered in this state, so this only
    /// surfaces from direct provider use
    NotConfigured,
    /// HTTP request failed (transport or non-2xx); detail names the class,
    /// never the body
    RequestFailed(String),
    /// response wasn't a search envelope
    BadResponse(String),
    /// hit the 30s timeout
    Timeout,
}

/// parse `{provider} HTTP {code}: ...` as `status_error` formats it (provider
/// name precedes the code)
fn parse_http_status(detail: &str) -> Option<u16> {
    let idx = detail.find(" HTTP ")?;
    let rest = &detail[idx + 6..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() == 3 {
        digits.parse().ok()
    } else {
        None
    }
}

impl SearchError {
    /// structured error code carried into the ledger
    pub fn code(&self) -> &'static str {
        match self {
            SearchError::NotConfigured => "WEBSEARCH_NOT_CONFIGURED",
            SearchError::RequestFailed(_) => "WEBSEARCH_REQUEST_FAILED",
            SearchError::BadResponse(_) => "WEBSEARCH_BAD_RESPONSE",
            SearchError::Timeout => "WEBSEARCH_TIMEOUT",
        }
    }

    fn retryable(&self) -> bool {
        match self {
            SearchError::Timeout => true,
            SearchError::RequestFailed(detail) => {
                // 4xx (bad key/request) isn't fixed by retrying; 429 and
                // 5xx/transport are. status errors format as "HTTP {code}: ...".
                !matches!(parse_http_status(detail), Some(code) if (400..500).contains(&code) && code != 429)
            }
            _ => false,
        }
    }

    fn remediation(&self) -> &'static str {
        match self {
            // variant carries no provider, so naming Tavily here would mislead
            // every other provider's users
            SearchError::NotConfigured => {
                "resolve the provider's API key and enable web search before calling"
            }
            SearchError::RequestFailed(_) => {
                "check network connectivity and retry; a 4xx means the key or request shape was rejected"
            }
            SearchError::BadResponse(_) => {
                "retry once; if it persists the provider changed its response format"
            }
            SearchError::Timeout => "retry; narrow the query if it keeps timing out",
        }
    }
}

impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SearchError::NotConfigured => write!(f, "web search is not configured: no API key"),
            SearchError::RequestFailed(detail) => {
                write!(f, "web search request failed: {detail}")
            }
            SearchError::BadResponse(detail) => {
                write!(f, "web search got an unreadable response: {detail}")
            }
            SearchError::Timeout => write!(f, "web search request timed out"),
        }
    }
}

impl std::error::Error for SearchError {}

impl From<SearchError> for PantheonError {
    fn from(e: SearchError) -> Self {
        PantheonError::new(
            e.code(),
            Layer::Execution,
            e.retryable(),
            e.to_string(),
            e.remediation(),
            "",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryability_follows_http_status() {
        assert!(!SearchError::RequestFailed("tinyfish HTTP 403: bad key".into()).retryable());
        assert!(!SearchError::RequestFailed("exa HTTP 400: bad request".into()).retryable());
        assert!(SearchError::RequestFailed("tinyfish HTTP 429: slow down".into()).retryable());
        assert!(SearchError::RequestFailed("tinyfish HTTP 500: boom".into()).retryable());
        assert!(SearchError::RequestFailed("tinyfish transport error: dns".into()).retryable());
        assert!(SearchError::Timeout.retryable());
        assert!(!SearchError::NotConfigured.retryable());
        assert!(!SearchError::BadResponse("x".into()).retryable());
    }
}
