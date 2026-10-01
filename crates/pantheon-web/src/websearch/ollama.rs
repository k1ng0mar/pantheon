//! Ollama Web Search-backed [`SearchProvider`]:
//! `POST https://ollama.com/api/web_search`.
//!
//! This is the hosted Ollama search API (a free Ollama account API key),
//! not the local model daemon. Auth is `Authorization: Bearer <key>`.
//! Responses carry no date fields: `published` is always `None` rather
//! than fabricated.
//!
//! The API key is handed to the constructor by the caller (resolved from the
//! secrets broker or the env by the parent) and never appears in any log
//! line, error, or tool result.

use super::error::SearchError;
use super::http;
use super::provider::{opt_str, SearchOptions, SearchProvider, SearchResult};

/// Default API base URL (no trailing path; `/api/web_search` is appended).
pub const DEFAULT_BASE_URL: &str = "https://ollama.com";
/// Env/secret name the parent resolves the Ollama key from.
pub const OLLAMA_API_KEY: &str = "OLLAMA_API_KEY";

/// Ollama Web Search provider.
pub struct OllamaProvider {
    api_key: String,
    base_url: String,
}

impl OllamaProvider {
    /// Production constructor: default Ollama endpoint.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL)
    }

    /// Test/local-mirror constructor: overrides the API base URL (e.g. a
    /// local daemon at `http://localhost:11434` proxies the same path when
    /// signed in).
    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
        }
    }

    /// The base URL in use (without the `/api/web_search` path).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl SearchProvider for OllamaProvider {
    fn name(&self) -> &str {
        "ollama"
    }

    fn search(&self, query: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>, SearchError> {
        // The API caps max_results at 10.
        let body = serde_json::json!({
            "query": query,
            "max_results": opts.max_results.min(10),
        });
        let url = format!("{}/api/web_search", self.base_url.trim_end_matches('/'));
        let auth = format!("Bearer {}", self.api_key);
        let text = http::post_json("ollama", &url, &[("Authorization", &auth)], body)?;
        parse_ollama_response(&text)
    }
}

/// Parse an Ollama web-search JSON envelope. Pure and unit-testable.
///
/// Hits live at `results` with `title`/`url`/`content`. Ollama returns no
/// date fields, so `published` is always `None`. A single malformed hit is
/// skipped, never fatal; a hit without a usable URL is skipped. A missing
/// or non-array `results` field is a `BadResponse`.
pub fn parse_ollama_response(body: &str) -> Result<Vec<SearchResult>, SearchError> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| SearchError::BadResponse(format!("response is not valid JSON: {e}")))?;
    let results = v
        .get("results")
        .and_then(|r| r.as_array())
        .ok_or_else(|| SearchError::BadResponse("response has no 'results' array".to_string()))?;
    let mut out = Vec::with_capacity(results.len());
    for r in results {
        let Some(url) = opt_str(r, "url") else {
            continue;
        };
        if url.trim().is_empty() {
            continue;
        }
        out.push(SearchResult {
            title: opt_str(r, "title").unwrap_or_default(),
            url,
            snippet: opt_str(r, "content").unwrap_or_default(),
            published: None,
            score: None,
        });
    }
    Ok(out)
}
