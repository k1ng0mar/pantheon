//! Perplexity Search-backed [`SearchProvider`]:
//! `POST https://api.perplexity.ai/search`.
//!
//! This is the raw Search API (ranked results, no LLM synthesis)
//! separate from Sonar `/chat/completions` and the Agent API. Auth is
//! `Authorization: Bearer <key>`. There is no free tier: prepaid credits
//! only ($5/1K requests).
//!
//! Perplexity's date fields are the best in the set: `date` maps to
//! `published`, falling back to `last_updated` when `date` is absent.
//! `include_domains` maps to `search_domain_filter`; there is no deny
//! parameter, so `exclude_domains` is ignored by this provider.
//!
//! The API key is handed to the constructor by the caller (resolved from the
//! secrets broker or the env by the parent) and never appears in any log
//! line, error, or tool result.

use super::error::SearchError;
use super::http;
use super::provider::{opt_str, SearchOptions, SearchProvider, SearchResult};

/// Default API base URL (no trailing path; `/search` is appended).
pub const DEFAULT_BASE_URL: &str = "https://api.perplexity.ai";
/// Env/secret name the parent resolves the Perplexity key from.
pub const PERPLEXITY_API_KEY: &str = "PERPLEXITY_API_KEY";

/// Perplexity Search provider.
pub struct PerplexityProvider {
    api_key: String,
    base_url: String,
}

impl PerplexityProvider {
    /// Production constructor: default Perplexity endpoint.
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

impl SearchProvider for PerplexityProvider {
    fn name(&self) -> &str {
        "perplexity"
    }

    fn search(&self, query: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>, SearchError> {
        let mut body = serde_json::json!({
            "query": query,
            "max_results": opts.max_results,
        });
        if !opts.include_domains.is_empty() {
            body["search_domain_filter"] = serde_json::json!(opts.include_domains);
        }
        let url = format!("{}/search", self.base_url.trim_end_matches('/'));
        let auth = format!("Bearer {}", self.api_key);
        let text = http::post_json("perplexity", &url, &[("Authorization", &auth)], body)?;
        parse_perplexity_response(&text)
    }
}

/// Parse a Perplexity `/search` JSON envelope. Pure and unit-testable.
///
/// Hits live at `results` with `title`/`url`/`snippet`. `date` maps to
/// `published`, falling back to `last_updated` - both are real provider
/// date fields, never fabricated. A single malformed hit is skipped,
/// never fatal; a hit without a usable URL is skipped. A missing or
/// non-array `results` field is a `BadResponse`.
pub fn parse_perplexity_response(body: &str) -> Result<Vec<SearchResult>, SearchError> {
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
            snippet: opt_str(r, "snippet").unwrap_or_default(),
            published: opt_str(r, "date").or_else(|| opt_str(r, "last_updated")),
            score: None,
        });
    }
    Ok(out)
}
