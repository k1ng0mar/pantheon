//! Brave Search-backed [`SearchProvider`]:
//! `GET https://api.search.brave.com/res/v1/web/search`.
//!
//! Auth is the `X-Subscription-Token` header (NOT `Authorization`).
//! Response nests hits under `web.results`; each hit carries `title`,
//! `url`, `description`, and `age` (a human-relative date string like
//! "2 days ago", mapped honestly into `published` as returned).
//!
//! Brave has no domain allow/deny request parameters: `include_domains`
//! / `exclude_domains` are ignored by this provider.
//!
//! The API key is handed to the constructor by the caller (resolved from the
//! secrets broker or the env by the parent) and never appears in any log
//! line, error, or tool result.

use super::error::SearchError;
use super::http;
use super::provider::{opt_str, SearchOptions, SearchProvider, SearchResult};

/// Default API base URL (no trailing path).
pub const DEFAULT_BASE_URL: &str = "https://api.search.brave.com";
/// Env/secret name the parent resolves the Brave key from.
pub const BRAVE_API_KEY: &str = "BRAVE_API_KEY";

/// Brave Search provider.
pub struct BraveProvider {
    api_key: String,
    base_url: String,
}

impl BraveProvider {
    /// Production constructor: default Brave endpoint.
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

    /// The base URL in use (without the `/res/v1/web/search` path).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl SearchProvider for BraveProvider {
    fn name(&self) -> &str {
        "brave"
    }

    fn search(&self, query: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>, SearchError> {
        let url = format!(
            "{}/res/v1/web/search?q={}&count={}",
            self.base_url.trim_end_matches('/'),
            http::percent_encode(query),
            opts.max_results,
        );
        let text = http::get_json("brave", &url, &[("X-Subscription-Token", &self.api_key)])?;
        parse_brave_response(&text)
    }
}

/// Parse a Brave web-search JSON envelope. Pure and unit-testable.
///
/// Hits live at `web.results`. Each hit's `description` is the snippet and
/// `age` (a relative date string) maps honestly into `published`. A single
/// malformed hit is skipped, never fatal; a hit without a usable URL is
/// skipped. Missing `web` / `web.results` is a `BadResponse`.
pub fn parse_brave_response(body: &str) -> Result<Vec<SearchResult>, SearchError> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| SearchError::BadResponse(format!("response is not valid JSON: {e}")))?;
    let results = v
        .get("web")
        .and_then(|w| w.get("results"))
        .and_then(|r| r.as_array())
        .ok_or_else(|| {
            SearchError::BadResponse("response has no 'web.results' array".to_string())
        })?;
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
            snippet: opt_str(r, "description").unwrap_or_default(),
            published: opt_str(r, "age").or_else(|| opt_str(r, "page_age")),
            score: None,
        });
    }
    Ok(out)
}
