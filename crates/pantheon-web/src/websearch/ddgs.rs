//! DuckDuckGo metasearch through the local `ddgs` binary.
//!
//! Local: shells out to `ddgs text -q <query> -m <n> -o <owned tmp
//! file>`, then parses the `[{title,href,body}]` JSON it writes. No key,
//! but the binary must exist - the setup wizard detects it and offers
//! the install. Scraping-adjacent metasearch is inherently fragile
//! (selectors and rate limits shift without notice), so this row is a
//! fallback, never the recommended default.
//!
//! The temp file lives in the OS temp dir under a pid+timestamp name
//! and is removed after parsing, whatever the outcome.

use super::error::SearchError;
use super::provider::{opt_str, SearchOptions, SearchProvider, SearchResult};

/// Binary this provider shells out to.
pub const DDGS_BINARY: &str = "ddgs";

/// DuckDuckGo metasearch provider. Local - shells out to [`DDGS_BINARY`].
pub struct DdgsProvider;

impl DdgsProvider {
    /// Production constructor: uses `ddgs` from `PATH`.
    pub fn new() -> Self {
        Self
    }
}

impl Default for DdgsProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// Owned temp path for one `ddgs -o` run. Pid + nanos: unique without a
/// uuid dependency, and prefixed so strays are recognizable.
fn tmp_path() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "pantheon-ddgs-{}-{}.json",
        std::process::id(),
        nanos
    ))
}

impl SearchProvider for DdgsProvider {
    fn name(&self) -> &str {
        "ddgs"
    }

    fn search(&self, query: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>, SearchError> {
        let out = tmp_path();
        let output = std::process::Command::new(DDGS_BINARY)
            .args([
                "text",
                "-q",
                query,
                "-m",
                &opts.max_results.max(1).to_string(),
                "-o",
                &out.display().to_string(),
                "-nc",
            ])
            .output()
            .map_err(|e| {
                SearchError::RequestFailed(format!(
                    "ddgs binary not runnable ({e}); install it with `pip install ddgs`"
                ))
            })?;
        // The output file is ours: remove it whether the run
        // succeeded or not, then report the outcome.
        let text = std::fs::read_to_string(&out);
        let _ = std::fs::remove_file(&out);
        if !output.status.success() {
            return Err(SearchError::RequestFailed(format!(
                "ddgs exited {}",
                output.status.code().unwrap_or(-1)
            )));
        }
        let text = text.map_err(|e| {
            SearchError::BadResponse(format!("ddgs wrote no parseable output: {e}"))
        })?;
        parse_ddgs_response(&text)
    }
}

/// Parse the `[{title,href,body}]` JSON `ddgs -o` writes. Pure and
/// unit-testable. A hit without a usable `href` is skipped; a
/// non-array envelope is a `BadResponse`.
pub fn parse_ddgs_response(body: &str) -> Result<Vec<SearchResult>, SearchError> {
    let v: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| SearchError::BadResponse(format!("response is not valid JSON: {e}")))?;
    let results = v
        .as_array()
        .ok_or_else(|| SearchError::BadResponse("response is not a JSON array".to_string()))?;
    let mut out = Vec::with_capacity(results.len());
    for r in results {
        let Some(url) = opt_str(r, "href") else {
            continue;
        };
        if url.trim().is_empty() {
            continue;
        }
        out.push(SearchResult {
            title: opt_str(r, "title").unwrap_or_default(),
            url,
            snippet: opt_str(r, "body").unwrap_or_default(),
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
    fn parses_ddgs_json_file_shape() {
        let body = r#"[
            {"title":"Rust - Wikipedia","href":"https://en.wikipedia.org/wiki/Rust","body":"a language"},
            {"title":"no href here","body":"skip"},
            {"title":"empty","href":"  ","body":"skip"}
        ]"#;
        let hits = parse_ddgs_response(body).expect("parse");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://en.wikipedia.org/wiki/Rust");
        assert_eq!(hits[0].snippet, "a language");
    }

    #[test]
    fn non_array_is_bad_response() {
        assert!(parse_ddgs_response(r#"{"results":[]}"#).is_err());
    }
}
