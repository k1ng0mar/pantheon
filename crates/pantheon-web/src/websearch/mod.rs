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
//! Perplexity Search.
//!
//! API keys are resolved by the caller (via `pantheon_secrets::SecretsBroker`
//! or the env) and passed in through [`WebsearchOptions::api_key`] or the
//! provider constructors. This crate never reads the environment, never
//! touches the vault, and never puts a key in a log line or an error
//! message.

pub mod brave;
pub mod error;
pub mod exa;
pub mod firecrawl;
mod http;
pub mod marginalia;
pub mod ollama;
pub mod perplexity;
pub mod provider;
pub mod registry;
pub mod searxng;
pub mod tavily;
pub mod tinyfish;
pub mod tools;

pub use brave::{BraveProvider, BRAVE_API_KEY};
pub use error::SearchError;
pub use exa::{ExaProvider, EXA_API_KEY};
pub use firecrawl::{FirecrawlProvider, FIRECRAWL_API_KEY};
pub use marginalia::MarginaliaProvider;
pub use ollama::{OllamaProvider, OLLAMA_API_KEY};
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
