//! Firecrawl-backed [`SearchProvider`]: `POST https://api.firecrawl.dev/v2/search`.
//!
//! Auth is `Authorization: Bearer <key>`. Responses include scraped page
//! content (`markdown`) - deliberately heavy. This provider keeps snippets
//! only: the snippet comes from `metadata.description` (never the full
//! page markdown), and no `scrapeOptions` are sent, so the API returns
//! search hits without full-page scrapes by default.
//!
//! Firecrawl v2 search has no domain allow/deny request parameters, so
//! `include_domains` / `exclude_domains` are ignored by this provider.
//!
//! The API key is handed to the constructor by the caller (resolved from the
//! secrets broker or the env by the parent) and never appears in any log
//! line, error, or tool result.

use super::error::SearchError;
use super::http;
use super::provider::{opt_str, SearchOptions, SearchProvider, SearchResult};

/// Default API base URL (no trailing path; `/v2/search` is appended).
pub const DEFAULT_BASE_URL: &str = "https://api.firecrawl.dev";
/// Env/secret name the parent resolves the Firecrawl key from.
pub const FIRECRAWL_API_KEY: &str = "FIRECRAWL_API_KEY";

/// Firecrawl search provider.
pub struct FirecrawlProvider {
    api_key: String,
    base_url: String,
}

impl FirecrawlProvider {
    /// Production constructor: default Firecrawl endpoint.
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

    /// The base URL in use (without the `/v2/search` path).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl SearchProvider for FirecrawlProvider {
    fn name(&self) -> &str {
        "firecrawl"
    }

    fn search(&self, query: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>, SearchError> {
        // No `scrapeOptions`: keep the response to search hits, not
        // full-page scrapes.
        let body = serde_json::json!({
            "query": query,
            "limit": opts.max_results,
        });
        let url = format!("{}/v2/search", self.base_url.trim_end_matches('/'));
        let auth = format!("Bearer {}", self.api_key);
        let text = http::post_json("firecrawl", &url, &[("Authorization", &auth)], body)?;
        parse_firecrawl_response(&text)
    }
}

/// Parse a Firecrawl v2 `/search` JSON envelope. Pure and unit-testable.
///
/// Hits live at `data`. The snippet is `metadata.description` - the full
/// `markdown` page content is never used as a snippet. `published` comes
/// from `metadata.publishedTime` (fallback `metadata.modifiedTime`).
/// A single malformed hit is skipped, never fatal; a hit without a usable
/// URL is skipped. A missing or non-array `data` field is a `BadResponse`.
pub fn parse_firecrawl_response(body: &str) -> Result<Vec<SearchResult>, SearchError> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| SearchError::BadResponse(format!("response is not valid JSON: {e}")))?;
    let results = v
        .get("data")
        .and_then(|r| r.as_array())
        .ok_or_else(|| SearchError::BadResponse("response has no 'data' array".to_string()))?;
    let mut out = Vec::with_capacity(results.len());
    for r in results {
        let Some(url) = opt_str(r, "url") else {
            continue;
        };
        if url.trim().is_empty() {
            continue;
        }
        let meta = r.get("metadata");
        out.push(SearchResult {
            title: opt_str(r, "title").unwrap_or_default(),
            url,
            // Snippets only: description, never the scraped markdown.
            snippet: meta
                .and_then(|m| opt_str(m, "description"))
                .unwrap_or_default(),
            published: meta
                .and_then(|m| opt_str(m, "publishedTime").or_else(|| opt_str(m, "modifiedTime"))),
            score: None,
        });
    }
    Ok(out)
}
