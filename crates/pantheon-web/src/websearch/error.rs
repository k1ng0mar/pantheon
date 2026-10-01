//! Search errors and their mapping to structured `PantheonError`s.
//!
//! Invariant: the API key never appears in any error message. Request bodies
//! carry the key, so failures describe the failure class (HTTP status,
//! transport, timeout, unreadable payload) without echoing anything sent.

use pantheon_api::error::{Layer, PantheonError};
use std::fmt;

/// What can go wrong in a web search call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchError {
    /// No API key was configured; the tool is not registered in this state,
    /// so this surfaces only from direct provider use.
    NotConfigured,
    /// The HTTP request failed (transport error or non-2xx status). The
    /// detail names the failure class, never the request body.
    RequestFailed(String),
    /// The response could not be understood as a search envelope.
    BadResponse(String),
    /// The request hit the 30s timeout.
    Timeout,
}

impl SearchError {
    /// The structured error code carried into the ledger.
    pub fn code(&self) -> &'static str {
        match self {
            SearchError::NotConfigured => "WEBSEARCH_NOT_CONFIGURED",
            SearchError::RequestFailed(_) => "WEBSEARCH_REQUEST_FAILED",
            SearchError::BadResponse(_) => "WEBSEARCH_BAD_RESPONSE",
            SearchError::Timeout => "WEBSEARCH_TIMEOUT",
        }
    }

    fn retryable(&self) -> bool {
        matches!(self, SearchError::RequestFailed(_) | SearchError::Timeout)
    }

    fn remediation(&self) -> &'static str {
        match self {
            SearchError::NotConfigured => {
                "resolve the Tavily API key (TAVILY_API_KEY) and enable web search before calling"
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
