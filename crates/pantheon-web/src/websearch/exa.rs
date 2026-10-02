//! Exa-backed [`SearchProvider`]: `POST https://api.exa.ai/search`.
//!
//! Auth is the `x-api-key` header (NOT `Authorization`). The `type` field
//! accepts only `instant|fast|auto|deep-lite|deep|deep-reasoning` - the old
//! `neural`/`keyword` values were removed server-side and must not be sent.
//! We always send `auto`: agent-suitable quality without deep-search cost.
//!
//! The API key is handed to the constructor by the caller (resolved from the
//! secrets broker or the env by the parent) and never appears in any log
//! line, error, or tool result.

use super::error::SearchError;
use super::http;
use super::provider::{opt_str, SearchOptions, SearchProvider, SearchResult};

/// Default API base URL (no trailing path; `/search` is appended).
pub const DEFAULT_BASE_URL: &str = "https://api.exa.ai";
/// Env/secret name the parent resolves the Exa key from.
pub const EXA_API_KEY: &str = "EXA_API_KEY";

/// Exa search provider.
pub struct ExaProvider {
    api_key: String,
    base_url: String,
}

impl ExaProvider {
    /// Production constructor: default Exa endpoint.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self::with_base_url(api_key, DEFAULT_BASE_URL)
    }

    /// Test/local-mirror constructor: overrides the API base URL.
    pub fn with_base_url(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: base_url.into(),
        }
    }

    /// The base URL in use (without the `/search` path).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl SearchProvider for ExaProvider {
    fn name(&self) -> &str {
        "exa"
    }

    fn search(&self, query: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>, SearchError> {
        let mut body = serde_json::json!({
            "query": query,
            "numResults": opts.max_results,
            // `neural`/`keyword` were removed server-side; `auto` is the
            // documented agent-suitable default.
            "type": "auto",
        });
        if !opts.include_domains.is_empty() {
            body["includeDomains"] = serde_json::json!(opts.include_domains);
        }
        if !opts.exclude_domains.is_empty() {
            body["excludeDomains"] = serde_json::json!(opts.exclude_domains);
        }
        let url = format!("{}/search", self.base_url.trim_end_matches('/'));
        let text = http::post_json("exa", &url, &[("x-api-key", &self.api_key)], body)?;
        parse_exa_response(&text)
    }
}

/// Parse an Exa `/search` JSON envelope. Pure and unit-testable.
///
/// Defensive by contract: any of `title`, `url`, `text`,
/// `publishedDate` may be missing or null. A single malformed result is
/// skipped, never fatal; a result without a usable URL is skipped. A
/// missing or non-array `results` field is a `BadResponse`.
pub fn parse_exa_response(body: &str) -> Result<Vec<SearchResult>, SearchError> {
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
            snippet: opt_str(r, "text").unwrap_or_default(),
            published: opt_str(r, "publishedDate"),
            score: None,
        });
    }
    Ok(out)
}
