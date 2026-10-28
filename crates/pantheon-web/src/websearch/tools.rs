//! The `web_search` tool: web **lookup**, deliberately distinct from browser
//! automation.
//!
//! Registration policy: a tool that can never work must not appear in the
//! model's tool list. When web search is disabled or no API key was resolved,
//! `register_websearch_tools` registers nothing and returns `Ok(0)`; the
//! parent logs the reason. The API key is passed in via [`WebsearchOptions`]
//! and never logged or embedded in errors.

use super::provider::{SearchOptions, SearchProvider};
use super::tavily::TavilyProvider;
use pantheon_api::capability::Capability;
use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::message::ToolSchema;
use pantheon_tools::tools::{parse_args, ToolRegistry};
use std::sync::Arc;

/// Env/secret name the parent resolves the Tavily key from.
pub const TAVILY_API_KEY: &str = "TAVILY_API_KEY";

/// Snippets longer than this are cut before they hit context.
const SNIPPET_MAX_CHARS: usize = 500;
/// Hard ceiling on per-call results, regardless of what the args ask for.
const MAX_RESULTS_CEILING: u8 = 10;

/// Options for `register_websearch_tools`. The `api_key` is resolved by the
/// caller (via `pantheon_secrets::SecretsBroker` or the env) and handed in as
/// a plain string; this crate never reads env or the vault itself.
pub struct WebsearchOptions {
    pub enabled: bool,
    pub max_results: u8,
    pub api_key: Option<String>,
}

impl Default for WebsearchOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            max_results: 5,
            api_key: None,
        }
    }
}

/// Register the Tavily-backed `web_search` tool.
///
/// Returns the number of tools registered. When `enabled` is false or the
/// key is missing/blank, registers **nothing** and returns `Ok(0)` - the
/// parent logs why; the model must not see a tool that can never work.
pub fn register_websearch_tools(
    reg: &mut ToolRegistry,
    opts: WebsearchOptions,
) -> Result<usize, PantheonError> {
    if !opts.enabled {
        return Ok(0);
    }
    let key = match opts.api_key {
        Some(k) if !k.trim().is_empty() => k,
        _ => return Ok(0),
    };
    register_search_tools(reg, Arc::new(TavilyProvider::new(key)), opts.max_results)
}

/// Register `web_search` against any [`SearchProvider`]. This is the seam the
/// mock-provider contract test drives; [`register_websearch_tools`] is the
/// Tavily-backed production entry point. Returns the number of tools
/// registered (always 1).
pub fn register_search_tools(
    reg: &mut ToolRegistry,
    provider: Arc<dyn SearchProvider>,
    default_max_results: u8,
) -> Result<usize, PantheonError> {
    let default_max = default_max_results.clamp(1, MAX_RESULTS_CEILING);
    reg.register(
        ToolSchema {
            name: "web_search".into(),
            description: "Look something up on the web: facts, news, docs, prices, \"what is X\". \
                Returns a compact JSON array of {title, url, snippet, published?} results.\n\
                \n\
                LOOKUP vs AUTOMATION - pick the right tool:\n\
               - web_search is for KNOWING something: search the open web for information.\n\
               - browser_* tools are for DOING something on a live site: filling forms, clicking \
                through JS-heavy pages, working inside authenticated flows, or extracting content \
                from a specific page you already have the URL for.\n\
                \n\
                web_search cannot interact with pages (no clicks, no forms, no login). The browser_* \
                tools are not a search engine - search here first, then open a result's URL with a \
                browser tool only if you need that page's full content or must act on the site.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "The search query (required, non-empty)" },
                    "max_results": { "type": "number", "description": "Max results to return; default 5, hard ceiling 10" },
                    "include_domains": { "type": "array", "items": { "type": "string" }, "description": "Only search these domains, e.g. [\"example.com\"]" },
                    "exclude_domains": { "type": "array", "items": { "type": "string" }, "description": "Never search these domains" }
                },
                "required": ["query"]
            }),
        },
        Capability::NetworkOutbound,
        move |args| {
            let v = parse_args(args)?;
            let query = v
                .get("query")
                .and_then(|q| q.as_str())
                .map(str::trim)
                .unwrap_or_default();
            if query.is_empty() {
                return Err(PantheonError::new(
                    "TOOL_BAD_ARGS",
                    Layer::Execution,
                    false,
                    "missing string arg 'query'",
                    "pass a non-empty search query",
                    "",
                ));
            }
            let max = v
                .get("max_results")
                .and_then(serde_json::Value::as_u64)
                // Clamp in u64 first: casting to u8 first wrapped
                // (256 -> 0 -> clamped to 1, 300 -> 44 -> clamped to 10).
                .map(|n| n.clamp(1, u64::from(MAX_RESULTS_CEILING)) as u8)
                .unwrap_or(default_max);
            let domains = |key: &str| -> Vec<String> {
                v.get(key)
                    .and_then(|d| d.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str())
                            .map(|s| s.to_string())
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let include = domains("include_domains");
            let exclude = domains("exclude_domains");
            let results = provider.search(
                query,
                &SearchOptions {
                    max_results: max,
                    include_domains: include.clone(),
                    exclude_domains: exclude.clone(),
                },
            )?;
            // Client-side domain filtering: only a few providers honour
            // include/exclude natively, but the tool schema promises
            // them for every provider. Filtering here makes the promise
            // true uniformly (native filtering + this pass is harmless:
            // both keep the same set).
            let results = filter_domains(results, &include, &exclude);
            let compact: Vec<serde_json::Value> = results
                .into_iter()
                .map(|r| {
                    let mut o = serde_json::json!({
                        "title": r.title,
                        "url": r.url,
                        "snippet": truncate(&r.snippet, SNIPPET_MAX_CHARS),
                    });
                    if let Some(p) = r.published {
                        o["published"] = serde_json::json!(p);
                    }
                    o
                })
                .collect();
            Ok(serde_json::to_string(&compact).unwrap_or_else(|_| "[]".to_string()))
        },
    );
    Ok(1)
}

/// Host of a result URL, lowercased, without port or userinfo.
/// Returns `None` for URLs with no usable host, which then match no
/// include list and are dropped when one is given.
fn url_host(url: &str) -> Option<String> {
    let after = url.split("://").nth(1).unwrap_or(url);
    let host_port = after.split(['/', '?', '#']).next().unwrap_or("");
    let host = host_port.rsplit('@').next().unwrap_or("");
    let host = host
        .strip_prefix('[')
        .map_or(host, |h| h.split(']').next().unwrap_or(h));
    let host = host.split(':').next().unwrap_or("");
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

/// True when `host` is `domain` or a subdomain of it.
fn host_matches_domain(host: &str, domain: &str) -> bool {
    let domain = domain.trim().trim_start_matches('.').to_ascii_lowercase();
    !domain.is_empty() && (host == domain || host.ends_with(&format!(".{domain}")))
}

/// Apply include/exclude domain filters to a provider's results.
/// Empty lists mean "no constraint" on that side.
pub fn filter_domains(
    results: Vec<super::provider::SearchResult>,
    include: &[String],
    exclude: &[String],
) -> Vec<super::provider::SearchResult> {
    if include.is_empty() && exclude.is_empty() {
        return results;
    }
    results
        .into_iter()
        .filter(|r| {
            let Some(host) = url_host(&r.url) else {
                return include.is_empty();
            };
            if !include.is_empty() && !include.iter().any(|d| host_matches_domain(&host, d)) {
                return false;
            }
            if exclude.iter().any(|d| host_matches_domain(&host, d)) {
                return false;
            }
            true
        })
        .collect()
}

/// Char-boundary-safe truncation.
fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let end = s
        .char_indices()
        .map(|(i, _)| i)
        .nth(max_chars)
        .unwrap_or(s.len());
    format!("{}...", &s[..end])
}

#[cfg(test)]
mod domain_filter_tests {
    use super::filter_domains;
    use crate::websearch::provider::SearchResult;

    fn hit(url: &str) -> SearchResult {
        SearchResult {
            title: String::new(),
            url: url.to_string(),
            snippet: String::new(),
            published: None,
            score: None,
        }
    }

    #[test]
    fn include_keeps_domain_and_subdomains_only() {
        let results = vec![
            hit("https://example.com/a"),
            hit("https://docs.example.com/b"),
            hit("https://notexample.com/c"),
            hit("https://other.org/d"),
        ];
        let kept = filter_domains(results, &["example.com".to_string()], &[]);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn exclude_drops_domain_and_subdomains() {
        let results = vec![hit("https://a.example.com/x"), hit("https://ok.org/y")];
        let kept = filter_domains(results, &[], &["example.com".to_string()]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].url, "https://ok.org/y");
    }

    #[test]
    fn empty_lists_pass_through() {
        let results = vec![hit("https://a.org/")];
        assert_eq!(filter_domains(results, &[], &[]).len(), 1);
    }
}
