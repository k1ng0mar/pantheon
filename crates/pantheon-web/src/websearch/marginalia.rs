//! Marginalia `GET .../public/search/{query}?count={n}`. Keyless; shared rate
//! budget, expect occasional 503s when drained. Index is the "small web"
//! (blogs, forums, docs, indie sites) — great for niche/technical queries,
//! weak on mainstream and brand-new stuff.
//!
//! Results are CC-BY-NC-SA 4.0 — fine for an agent reading them, matters if
//! you redistribute. No date fields, so `published` is always `None`, never
//! fabricated.

use super::error::SearchError;
use super::http;
use super::provider::{opt_str, SearchOptions, SearchProvider, SearchResult};

/// base URL
pub const DEFAULT_BASE_URL: &str = "https://api.marginalia.nu";

/// keyless, no constructor key
pub struct MarginaliaProvider {
    base_url: String,
}

impl MarginaliaProvider {
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_BASE_URL)
    }

    /// local mirror / test server
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }

    /// base URL without the `/public/search/...` path
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
        // query is a path segment, percent-encode it
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

/// Parse the envelope. Hits at `results`; no date fields so `published` stays
/// `None`. Skip bad hits / no-URL hits; no `results` array -> `BadResponse`.
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
