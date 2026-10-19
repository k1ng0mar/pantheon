//! `pantheon-web`: web lookup as a tool, distinct from browser automation.
//!
//! The model reaches for `web_search` to **look something up** (facts, news,
//! docs, prices, "what is X") and for the `browser_*` tools to **do something
//! on a live site** (forms, JS-heavy pages, authenticated flows, extraction
//! from a specific page). Search providers sit behind the [`SearchProvider`]
//! trait seam; the tool registration takes any provider.
//!
//! Built-in providers ([`registry::all_providers`], in picker order):
//! TinyFish (recommended default), Tavily, Ollama Web Search, Exa,
//! Marginalia (keyless), Brave, Firecrawl, SearXNG (self-hosted),
//! Perplexity Search, plus keyless fallbacks (Exa/Parallel/Firecrawl/
//! Keenable anonymous endpoints, local `ddgs` binary).
//!
//! API keys are resolved by the caller (via `pantheon_secrets::SecretsBroker`
//! or the env) and passed in through [`WebsearchOptions::api_key`] or the
//! provider constructors. This crate never reads the environment, never
//! touches the vault, and never puts a key in a log line or an error
//! message.

pub mod brave;
pub mod ddgs;
pub mod error;
pub mod exa;
pub mod exa_free;
pub mod firecrawl;
pub mod firecrawl_free;
mod http;
pub mod keenable;
pub mod marginalia;
pub mod ollama;
pub mod parallel;
pub mod perplexity;
pub mod provider;
pub mod registry;
pub mod searxng;
pub mod tavily;
pub mod tinyfish;
pub mod tools;

pub use brave::{BraveProvider, BRAVE_API_KEY};
pub use ddgs::{DdgsProvider, DDGS_BINARY};
pub use error::SearchError;
pub use exa::{ExaProvider, EXA_API_KEY};
pub use exa_free::ExaFreeProvider;
pub use firecrawl::{FirecrawlProvider, FIRECRAWL_API_KEY};
pub use firecrawl_free::FirecrawlFreeProvider;
pub use keenable::{KeenableFreeProvider, APP_TITLE as KEENABLE_APP_TITLE};
pub use marginalia::MarginaliaProvider;
pub use ollama::{OllamaProvider, OLLAMA_API_KEY};
pub use parallel::ParallelFreeProvider;
pub use perplexity::{PerplexityProvider, PERPLEXITY_API_KEY};
pub use provider::{parse_tavily_response, SearchOptions, SearchProvider, SearchResult};
pub use registry::{
    all_providers, build_provider, default_key_env, provider_info, searxng_url_env, ProviderAuth,
    ProviderInfo,
};
pub use searxng::{SearxngProvider, DEFAULT_INSTANCE_URL, SEARXNG_URL};
pub use tavily::TavilyProvider;
pub use tinyfish::{TinyFishProvider, TINYFISH_API_KEY};
pub use tools::{
    register_search_tools, register_websearch_tools, WebsearchOptions, TAVILY_API_KEY,
};
