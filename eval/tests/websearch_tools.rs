//! `web_search` tool: mock-provider contract test (no network) plus an
//! optional live Tavily run. Run with `cargo test -p pantheon-eval`.
//!
//! The mock test drives the same registration path the production tool uses
//! (`register_search_tools`), so the contract it asserts - schema, arg
//! parsing, capability, result shape - is the real one.
use pantheon_tools::tools::ToolRegistry;
use pantheon_web::websearch::error::SearchError;
use pantheon_web::websearch::provider::{SearchOptions, SearchProvider, SearchResult};
use pantheon_web::websearch::tools::{
    register_search_tools, register_websearch_tools, WebsearchOptions, TAVILY_API_KEY,
};
use std::sync::Arc;

/// Fake provider: asserts the tool layer forwarded args correctly and
/// returns one fixed hit.
struct FakeProvider;

impl SearchProvider for FakeProvider {
    fn name(&self) -> &str {
        "fake"
    }

    fn search(&self, query: &str, opts: &SearchOptions) -> Result<Vec<SearchResult>, SearchError> {
        assert_eq!(query, "rust borrow checker");
        assert_eq!(opts.max_results, 3);
        assert_eq!(opts.include_domains, vec!["doc.rust-lang.org".to_string()]);
        assert!(opts.exclude_domains.is_empty());
        Ok(vec![SearchResult {
            title: "References and Borrowing".to_string(),
            url: "https://doc.rust-lang.org/book/ch04-02-references-and-borrowing.html".to_string(),
            snippet: "At any given time, you can have either one mutable reference or any number of immutable references.".to_string(),
            published: Some("2023-05-25".to_string()),
            score: Some(0.987),
        }])
    }
}

#[test]
fn web_search_routes_through_provider_with_parsed_args() {
    let mut reg = ToolRegistry::new();
    let n = register_search_tools(&mut reg, Arc::new(FakeProvider), 5).unwrap();
    assert_eq!(n, 1);

    let out = reg
        .execute(
            "web_search",
            r#"{"query": "rust borrow checker", "max_results": 3, "include_domains": ["doc.rust-lang.org"]}"#,
        )
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let arr = v.as_array().expect("result is a JSON array");
    assert_eq!(arr.len(), 1);
    let hit = &arr[0];
    assert_eq!(hit["title"], "References and Borrowing");
    assert_eq!(
        hit["url"],
        "https://doc.rust-lang.org/book/ch04-02-references-and-borrowing.html"
    );
    assert!(hit["snippet"]
        .as_str()
        .unwrap()
        .contains("mutable reference"));
    assert_eq!(hit["published"], "2023-05-25");
    // score is internal ranking signal, not part of the model-facing shape
    assert!(hit.get("score").is_none());
}

#[test]
fn web_search_uses_default_max_results_when_arg_missing() {
    struct Probe;
    impl SearchProvider for Probe {
        fn name(&self) -> &str {
            "probe"
        }
        fn search(
            &self,
            _query: &str,
            opts: &SearchOptions,
        ) -> Result<Vec<SearchResult>, SearchError> {
            assert_eq!(opts.max_results, 7);
            Ok(Vec::new())
        }
    }
    let mut reg = ToolRegistry::new();
    register_search_tools(&mut reg, Arc::new(Probe), 7).unwrap();
    let out = reg.execute("web_search", r#"{"query": "x"}"#).unwrap();
    assert_eq!(out, "[]");
}

#[test]
fn web_search_propagates_provider_errors() {
    struct Failing;
    impl SearchProvider for Failing {
        fn name(&self) -> &str {
            "failing"
        }
        fn search(
            &self,
            _query: &str,
            _opts: &SearchOptions,
        ) -> Result<Vec<SearchResult>, SearchError> {
            Err(SearchError::Timeout)
        }
    }
    let mut reg = ToolRegistry::new();
    register_search_tools(&mut reg, Arc::new(Failing), 5).unwrap();
    let err = reg.execute("web_search", r#"{"query": "x"}"#).unwrap_err();
    let dbg = format!("{err:?}");
    assert!(dbg.contains("WEBSEARCH_TIMEOUT"), "got: {dbg}");
}

#[test]
fn websearch_tools_not_registered_without_key() {
    for opts in [
        WebsearchOptions {
            enabled: true,
            max_results: 5,
            api_key: None,
        },
        WebsearchOptions {
            enabled: false,
            max_results: 5,
            api_key: Some("k".to_string()),
        },
    ] {
        let mut reg = ToolRegistry::new();
        let n = register_websearch_tools(&mut reg, opts).unwrap();
        assert_eq!(n, 0);
        assert!(reg.get("web_search").is_none());
    }
}

/// Live Tavily run. Skips with a notice unless TAVILY_API_KEY is set
/// no key on CI means no network call, by construction.
#[test]
fn live_tavily_search_when_key_present() {
    let key = match std::env::var(TAVILY_API_KEY) {
        Ok(k) if !k.trim().is_empty() => k,
        _ => {
            eprintln!("SKIP live_tavily_search_when_key_present: TAVILY_API_KEY not set");
            return;
        }
    };
    let mut reg = ToolRegistry::new();
    let n = register_websearch_tools(
        &mut reg,
        WebsearchOptions {
            enabled: true,
            max_results: 3,
            api_key: Some(key),
        },
    )
    .unwrap();
    assert_eq!(n, 1);
    let out = reg
        .execute("web_search", r#"{"query": "Rust programming language"}"#)
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let arr = v.as_array().expect("result is a JSON array");
    assert!(!arr.is_empty(), "expected at least one live result");
    let first = &arr[0];
    let url = first["url"].as_str().unwrap_or_default();
    assert!(
        url.starts_with("http"),
        "first result has a usable URL, got: {url}"
    );
    assert!(!first["snippet"].as_str().unwrap_or_default().is_empty());
}
