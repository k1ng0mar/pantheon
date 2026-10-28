//! Anonymous Exa search via its public MCP endpoint.
//!
//! Keyless: `POST https://mcp.exa.ai/mcp` with a JSON-RPC `tools/call`
//! for `web_search_exa` (`query`, `numResults`). No auth header. The
//! public endpoint runs on a shared quota (expect throttling), so this
//! row is a fallback, never the recommended default. The tool answers
//! with `---`-separated `Title:/URL:/Published:/Author:/Highlights:`
//! text blocks.

use super::error::SearchError;
use super::http;
use super::provider::{SearchOptions, SearchProvider, SearchResult};

/// Default MCP endpoint URL.
pub const DEFAULT_BASE_URL: &str = "https://mcp.exa.ai/mcp";

/// Anonymous Exa search provider. Keyless - no constructor key.
pub struct ExaFreeProvider {
    base_url: String,
}

impl ExaFreeProvider {
    /// Production constructor: the public keyless endpoint.
    pub fn new() -> Self {
        Self::with_base_url(DEFAULT_BASE_URL)
    }

    /// Test/local-mirror constructor: overrides the endpoint URL.
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
        }
    }
}

impl Default for ExaFreeProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl SearchProvider for ExaFreeProvider {
    fn name(&self) -> &str {
        "exa_free"
    }

    fn search(&self, query: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>, SearchError> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {
                "name": "web_search_exa",
                "arguments": {
                    "query": query,
                    "numResults": opts.max_results.max(1),
                },
            },
        });
        let text = http::post_json(
            "exa_free",
            self.base_url.trim_end_matches('/'),
            &[
                ("Accept", "application/json, text/event-stream"),
                ("Content-Type", "application/json"),
            ],
            body,
        )?;
        let payload = http::mcp_text_payload(&text)?;
        // `max_results == 0` means one, like every other provider (see
        // `SearchOptions::max_results`): passing 0 through would uncap
        // the parse and return the whole page.
        Ok(parse_exa_text_blocks(&payload, opts.max_results.max(1)))
    }
}

/// Labels that start a new field inside an Exa text block. Highlights
/// run until the next labelled field.
const EXA_LABELS: [&str; 5] = ["Title:", "URL:", "Highlights:", "Published:", "Author:"];

fn after(line: &str, prefix: &str) -> String {
    line.strip_prefix(prefix).unwrap_or(line).trim().to_string()
}

/// Parse Exa's `---`-separated text blocks. Pure and unit-testable. A
/// block without a usable URL is skipped; highlights join with spaces.
/// `limit == 0` means no cap. Line endings are normalized first: a
/// CRLF payload used to defeat the `\n---\n` block split (the
/// separator arrived as `\r\n---\r\n`), collapsing every hit into one
/// block and returning only the first result.
pub fn parse_exa_text_blocks(text: &str, limit: u8) -> Vec<SearchResult> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = Vec::new();
    for block in normalized.split("\n---\n") {
        if limit > 0 && out.len() >= limit as usize {
            break;
        }
        let mut title = String::new();
        let mut url = String::new();
        let mut highlights: Vec<String> = Vec::new();
        let mut published: Option<String> = None;
        let mut in_highlights = false;
        for stripped in block.split('\n').map(str::trim) {
            if stripped.is_empty() {
                continue;
            }
            if stripped.starts_with("Title:") {
                title = after(stripped, "Title:");
                in_highlights = false;
            } else if stripped.starts_with("URL:") {
                url = after(stripped, "URL:");
                in_highlights = false;
            } else if stripped.starts_with("Published:") {
                let v = after(stripped, "Published:");
                if !v.is_empty() {
                    published = Some(v);
                }
                in_highlights = false;
            } else if stripped.starts_with("Highlights:") {
                in_highlights = true;
            } else if EXA_LABELS.iter().any(|l| stripped.starts_with(l)) {
                in_highlights = false;
            } else if in_highlights {
                highlights.push(stripped.to_string());
            }
        }
        if url.trim().is_empty() {
            continue;
        }
        out.push(SearchResult {
            title,
            url,
            snippet: highlights.join(" "),
            published,
            score: None,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "Title: First hit\nURL: https://a.example/x\nPublished: 2024-01-01\nHighlights:\nfirst line\nsecond line\n---\nTitle: Second\nURL: https://b.example/\nAuthor: Someone\n---\nTitle: No URL here\nHighlights:\nskipped";

    #[test]
    fn parses_blocks_with_highlights() {
        let hits = parse_exa_text_blocks(SAMPLE, 5);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "First hit");
        assert_eq!(hits[0].url, "https://a.example/x");
        assert_eq!(hits[0].snippet, "first line second line");
        assert_eq!(hits[0].published.as_deref(), Some("2024-01-01"));
        assert_eq!(hits[1].published, None);
        assert_eq!(hits[1].snippet, "");
    }

    #[test]
    fn limit_caps_results() {
        let hits = parse_exa_text_blocks(SAMPLE, 1);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn crlf_payload_parses_all_blocks() {
        let crlf = SAMPLE.replace('\n', "\r\n");
        let hits = parse_exa_text_blocks(&crlf, 5);
        assert_eq!(hits.len(), 2, "CRLF must not collapse blocks");
        assert_eq!(hits[0].title, "First hit");
        assert_eq!(hits[0].published.as_deref(), Some("2024-01-01"));
        assert_eq!(hits[1].title, "Second");
    }
}
