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
/// non-array `results` field is a `BadResponse` — the envelope itself is
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
