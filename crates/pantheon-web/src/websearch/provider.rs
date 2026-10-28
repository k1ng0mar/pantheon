//! Provider-agnostic search surface: result and option types, the
//! [`SearchProvider`] trait seam (Brave/Serper slot in here later), and the
//! pure Tavily response parser with its unit tests.

use super::error::SearchError;

/// One search hit.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub published: Option<String>,
    pub score: Option<f64>,
}

/// Per-call search options. The tool layer builds these from the tool args.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchOptions {
    /// Maximum results. `0` means one everywhere: the tool layer
    /// clamps to `>= 1`, the registry guard clamps again for direct
    /// `build_provider` use, and the truncating providers (`searxng`,
    /// `tinyfish`) apply `.max(1)` - so `0` can never mean "no
    /// results" in one provider and "uncapped" in another, as it did
    /// before (SearXNG/TinyFish truncated to 0, ExaFree's parser
    /// treated 0 as no cap).
    pub max_results: u8,
    pub include_domains: Vec<String>,
    pub exclude_domains: Vec<String>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            max_results: 5,
            include_domains: Vec::new(),
            exclude_domains: Vec::new(),
        }
    }
}

/// Trait seam for search providers. Sync: the tool closures are sync
/// string-in/string-out, so providers use blocking HTTP (ureq).
pub trait SearchProvider: Send + Sync {
    /// Provider name, e.g. `"tavily"`. Used in diagnostics, never in keys.
    fn name(&self) -> &str;
    /// Run one search. An empty query is the provider's problem to reject;
    /// the tool layer already rejects empty queries with TOOL_BAD_ARGS.
    fn search(&self, query: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>, SearchError>;
}

pub(crate) fn opt_str(v: &serde_json::Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
}

/// Parse a Tavily `/search` JSON envelope. Pure and unit-testable.
///
/// Defensive by contract: any of `title`, `url`, `content`,
/// `published_date`, `score` may be missing or null. A single malformed
/// result is skipped, never fatal to the whole call; a result without a
/// usable URL is skipped (there is nothing to cite). A missing or
/// non-array `results` field is a `BadResponse` - the envelope itself is
/// broken, not one hit.
pub fn parse_tavily_response(body: &str) -> Result<Vec<SearchResult>, SearchError> {
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
            published: opt_str(r, "published_date"),
            score: r.get("score").and_then(serde_json::Value::as_f64),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tavily_parser_tests {
    use super::parse_tavily_response;

    const FIXTURE: &str = r#"{
        "results": [
            {"title": "Rust", "url": "https://www.rust-lang.org/", "content": "a language", "published_date": "2024-05-01", "score": 0.9},
            {"title": "No URL here", "content": "skipped"},
            {"title": "Blank URL", "url": "   ", "content": "skipped too"},
            {"url": "https://bare.example/", "content": null, "published_date": null}
        ]
    }"#;

    #[test]
    fn parses_hits_and_skips_unusable_urls() {
        let hits = parse_tavily_response(FIXTURE).expect("parse");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Rust");
        assert_eq!(hits[0].published.as_deref(), Some("2024-05-01"));
        assert_eq!(hits[0].score, Some(0.9));
        assert_eq!(hits[1].url, "https://bare.example/");
        assert_eq!(hits[1].title, "");
        assert_eq!(hits[1].snippet, "");
        assert_eq!(hits[1].published, None);
    }

    #[test]
    fn missing_results_array_is_bad_response() {
        assert!(parse_tavily_response(r#"{"query": "x"}"#).is_err());
        assert!(parse_tavily_response("not json").is_err());
    }
}
