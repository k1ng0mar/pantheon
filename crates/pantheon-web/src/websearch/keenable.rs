//! Anonymous Keenable search (`POST {base}/v1/search/public`).
//!
//! Keyless: no auth header, just the `X-Keenable-Title` app id. The
//! public endpoint runs on a shared quota (expect throttling), so this
//! row is a fallback, never the recommended default. Results are plain
//! `results[].{url,title,snippet|description}` JSON.

use super::error::SearchError;
use super::http;
use super::provider::{opt_str, SearchOptions, SearchProvider, SearchResult};

/// Default API base URL (no trailing path).
pub const DEFAULT_BASE_URL: &str = "https://api.keenable.ai";

/// App id header Keenable's public endpoints require.
pub const APP_TITLE: &str = "pantheon";

/// Keenable anonymous search provider. Keyless - no constructor key.
pub struct KeenableFreeProvider {
    base_url: String,
}

impl KeenableFreeProvider {
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
}

impl Default for KeenableFreeProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchProvider for KeenableFreeProvider {
    fn name(&self) -> &str {
        "keenable_free"
    }

    fn search(&self, query: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>, SearchError> {
        let url = format!("{}/v1/search/public", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "query": query,
            "max_results": opts.max_results,
        });
        let text = http::post_json(
            "keenable_free",
            &url,
            &[("X-Keenable-Title", APP_TITLE)],
            body,
        )?;
        parse_keenable_response(&text)
    }
}

/// Parse the Keenable public-search JSON envelope. Pure and
/// unit-testable. Hits live at `results` with `url`/`title` and
/// `snippet` (falling back to `description`). A hit without a usable
/// URL is skipped; a missing or non-array `results` is a `BadResponse`.
pub fn parse_keenable_response(body: &str) -> Result<Vec<SearchResult>, SearchError> {
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
            snippet: opt_str(r, "snippet")
                .or_else(|| opt_str(r, "description"))
                .unwrap_or_default(),
            published: None,
            score: None,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_results_with_snippet_fallback() {
        let body = r#"{"results":[
            {"url":"https://a.example","title":"A","snippet":"first"},
            {"url":"https://b.example","title":"B","description":"second"},
            {"url":"","title":"skip me"},
            {"title":"no url at all"}
        ]}"#;
        let hits = parse_keenable_response(body).expect("parse");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].snippet, "first");
        assert_eq!(hits[1].snippet, "second");
    }

    #[test]
    fn missing_results_is_bad_response() {
        assert!(parse_keenable_response(r#"{"ok":true}"#).is_err());
        assert!(parse_keenable_response("not json").is_err());
    }
}
