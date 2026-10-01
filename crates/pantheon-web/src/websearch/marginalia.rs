//! Marginalia-backed [`SearchProvider`]:
//! `GET https://api.marginalia.nu/public/search/{query}?count={n}`.
//!
//! Keyless: the public endpoint runs on a shared rate budget (expect
//! occasional 503s when drained). No auth header is sent. Marginalia's
//! index is the "small web" (blogs, forums, docs, indie sites) — excellent
//! for technical/niche queries, weak on mainstream and very-recent results.
//!
//! License note: Marginalia results carry **CC-BY-NC-SA 4.0**. Fine for an
//! agent reading them; it matters if results are redistributed.
//!
//! Responses carry no date fields: `published` is always `None` rather
//! than fabricated.

use super::error::SearchError;
use super::http;
use super::provider::{opt_str, SearchOptions, SearchProvider, SearchResult};

/// Default API base URL (no trailing path).
pub const DEFAULT_BASE_URL: &str = "https://api.marginalia.nu";

/// Marginalia search provider. Keyless — no constructor key.
pub struct MarginaliaProvider {
    base_url: String,
}

impl MarginaliaProvider {
    /// Production constructor: the public keyless endpoint.
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_BASE_URL)
    }

    /// Test/local-mirror constructor: overrides the API base URL.
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }

    /// The base URL in use (without the `/public/search/...` path).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

impl Default for MarginaliaProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchProvider for MarginaliaProvider {
    fn name(&self) -> &str {
        "marginalia"
    }

    fn search(&self, query: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>, SearchError> {
        // The query is a path segment: percent-encode it.
        let url = format!(
            "{}/public/search/{}?count={}",
            self.base_url.trim_end_matches('/'),
            http::percent_encode(query),
            opts.max_results,
        );
        let text = http::get_json("marginalia", &url, &[])?;
        parse_marginalia_response(&text)
    }
}

/// Parse a Marginalia public-search JSON envelope. Pure and unit-testable.
///
/// Hits live at `results` with `url`/`title`/`description`. Marginalia
/// returns no date fields, so `published` is always `None`. A single
/// malformed hit is skipped, never fatal; a hit without a usable URL is
/// skipped. A missing or non-array `results` field is a `BadResponse`.
pub fn parse_marginalia_response(body: &str) -> Result<Vec<SearchResult>, SearchError> {
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
            snippet: opt_str(r, "description").unwrap_or_default(),
            published: None,
            score: None,
        });
    }
    Ok(out)
}
