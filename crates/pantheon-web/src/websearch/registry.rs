//! Provider registry for the setup wizard's web-search picker.
//!
//! [`all_providers`] enumerates every built-in provider with its auth
//! requirement and a one-line blurb, so the TUI setup wizard can present
//! the choice without knowing any provider's details. Provider ids are
//! stable: they are the values accepted for `[websearch] provider` in
//! config.toml and the strings returned by [`SearchProvider::name`].
//!
//! [`build_provider`] is the runtime's construction seam: given a provider
//! id, an optional resolved API key, and an optional base-URL override, it
//! returns a boxed [`SearchProvider`] or a human-readable reason it can't.

use super::brave::{BraveProvider, BRAVE_API_KEY, DEFAULT_BASE_URL as BRAVE_BASE};
use super::exa::{ExaProvider, DEFAULT_BASE_URL as EXA_BASE, EXA_API_KEY};
use super::firecrawl::{FirecrawlProvider, DEFAULT_BASE_URL as FIRECRAWL_BASE, FIRECRAWL_API_KEY};
use super::marginalia::{MarginaliaProvider, DEFAULT_BASE_URL as MARGINALIA_BASE};
use super::ollama::{OllamaProvider, DEFAULT_BASE_URL as OLLAMA_BASE, OLLAMA_API_KEY};
use super::perplexity::{
    PerplexityProvider, DEFAULT_BASE_URL as PERPLEXITY_BASE, PERPLEXITY_API_KEY,
};
use super::provider::SearchProvider;
use super::searxng::{SearxngProvider, DEFAULT_INSTANCE_URL, SEARXNG_URL};
use super::tavily::{TavilyProvider, DEFAULT_BASE_URL as TAVILY_BASE};
use super::tinyfish::{TinyFishProvider, DEFAULT_BASE_URL as TINYFISH_BASE, TINYFISH_API_KEY};
use super::tools::TAVILY_API_KEY;
use std::sync::Arc;

/// How a provider authenticates, as the setup picker needs to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderAuth {
    /// User-supplied API key. `env_var` is the default secret/env name the
    /// parent resolves the key from (e.g. `"EXA_API_KEY"`).
    ApiKey { env_var: &'static str },
    /// No key at all: works out of the box.
    Keyless,
    /// Self-hosted: no key, but the user must point it at their instance.
    /// `default_url` is the prefill when nothing is configured.
    SelfHosted { default_url: &'static str },
}

/// One row in the setup picker's provider list. All fields are
/// `&'static str`: the registry is a compile-time constant table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderInfo {
    /// Stable id: the `[websearch] provider` config value and the
    /// [`SearchProvider::name`] string. Never rename an id.
    pub id: &'static str,
    /// Human display name for the picker.
    pub name: &'static str,
    /// What the user must supply to use it.
    pub auth: ProviderAuth,
    /// One-line picker blurb. Cost honesty lives here: free tiers, cards,
    /// and prepaid-only plans are stated plainly.
    pub blurb: &'static str,
    /// Whether the picker should highlight this as the recommended
    /// default. Exactly one provider is recommended at a time, and
    /// [`all_providers`] sorts recommended providers first.
    pub recommended: bool,
}

/// Every built-in provider, pre-sorted for the picker: recommended
/// providers first (TinyFish today), then the rest. Add new providers
/// here (and only here) to surface them in the setup wizard; set
/// `recommended` on at most one.
pub fn all_providers() -> Vec<ProviderInfo> {
    vec![
        ProviderInfo {
            id: "tinyfish",
            name: "TinyFish",
            auth: ProviderAuth::ApiKey { env_var: TINYFISH_API_KEY },
            blurb: "Recommended default. Rank-stable, p50 <0.5s; free, no card. ~30 req/min free tier.",
            recommended: true,
        },
        ProviderInfo {
            id: "tavily",
            name: "Tavily",
            auth: ProviderAuth::ApiKey { env_var: TAVILY_API_KEY },
            blurb: "Agent-native search; 1,000 credits/month free, no card required.",
            recommended: false,
        },
        ProviderInfo {
            id: "ollama",
            name: "Ollama Web Search",
            auth: ProviderAuth::ApiKey { env_var: OLLAMA_API_KEY },
            blurb: "Hosted Ollama search; free account key, generous free tier, limits unpublished.",
            recommended: false,
        },
        ProviderInfo {
            id: "exa",
            name: "Exa",
            auth: ProviderAuth::ApiKey { env_var: EXA_API_KEY },
            blurb: "Neural/semantic search; $20 signup credit + $10/month recurring, no card.",
            recommended: false,
        },
        ProviderInfo {
            id: "marginalia",
            name: "Marginalia",
            auth: ProviderAuth::Keyless,
            blurb: "Keyless niche index (blogs, forums, indie web); shared quota, results are CC-BY-NC-SA 4.0.",
            recommended: false,
        },
        ProviderInfo {
            id: "brave",
            name: "Brave Search",
            auth: ProviderAuth::ApiKey { env_var: BRAVE_API_KEY },
            blurb: "Independent 40B+ page index; $5/month credits but a credit card is required.",
            recommended: false,
        },
        ProviderInfo {
            id: "firecrawl",
            name: "Firecrawl",
            auth: ProviderAuth::ApiKey { env_var: FIRECRAWL_API_KEY },
            blurb: "Search with page metadata; 1,000 credits/month free, no card (snippets only, no full scrapes).",
            recommended: false,
        },
        ProviderInfo {
            id: "searxng",
            name: "SearXNG",
            auth: ProviderAuth::SelfHosted { default_url: DEFAULT_INSTANCE_URL },
            blurb: "Self-hosted metasearch (70+ engines); free forever on your hardware. Never use a public instance.",
            recommended: false,
        },
        ProviderInfo {
            id: "perplexity",
            name: "Perplexity Search",
            auth: ProviderAuth::ApiKey { env_var: PERPLEXITY_API_KEY },
            blurb: "Raw ranked results with best-in-class dates; prepaid only, no free tier.",
            recommended: false,
        },
    ]
}

/// Look up one provider's [`ProviderInfo`] by id (case-insensitive).
pub fn provider_info(id: &str) -> Option<ProviderInfo> {
    all_providers()
        .into_iter()
        .find(|p| p.id.eq_ignore_ascii_case(id.trim()))
}

/// Default secret/env name holding `id`'s API key, or `None` when the
/// provider needs no key (keyless / self-hosted).
pub fn default_key_env(id: &str) -> Option<&'static str> {
    match provider_info(id)?.auth {
        ProviderAuth::ApiKey { env_var } => Some(env_var),
        ProviderAuth::Keyless | ProviderAuth::SelfHosted { .. } => None,
    }
}

/// Build a boxed provider for `id` (case-insensitive).
///
/// - `api_key`: the already-resolved key for keyed providers; blank or
///   missing is an error naming the provider and its env var.
/// - `base_url_override`: used for self-hosted providers (SearXNG instance
///   URL) and as a test mirror for any provider. When `None`, each
///   provider uses its default base URL.
///
/// The error is a human-readable string for logs/diagnostics; it never
/// contains key material.
pub fn build_provider(
    id: &str,
    api_key: Option<String>,
    base_url_override: Option<&str>,
) -> Result<Arc<dyn SearchProvider>, String> {
    let id = id.trim().to_ascii_lowercase();
    let key = || -> Result<String, String> {
        match api_key {
            Some(k) if !k.trim().is_empty() => Ok(k),
            _ => {
                let env = default_key_env(&id).unwrap_or("<unknown>");
                Err(format!(
                    "web search provider '{id}' needs an API key (set {env}); the tool is not registered"
                ))
            }
        }
    };
    let base = |default: &'static str| -> String {
        base_url_override
            .filter(|u| !u.trim().is_empty())
            .unwrap_or(default)
            .to_string()
    };
    let provider: Arc<dyn SearchProvider> = match id.as_str() {
        "tinyfish" => Arc::new(TinyFishProvider::with_base_url(key()?, base(TINYFISH_BASE))),
        "tavily" => Arc::new(TavilyProvider::with_base_url(key()?, base(TAVILY_BASE))),
        "exa" => Arc::new(ExaProvider::with_base_url(key()?, base(EXA_BASE))),
        "brave" => Arc::new(BraveProvider::with_base_url(key()?, base(BRAVE_BASE))),
        "firecrawl" => Arc::new(FirecrawlProvider::with_base_url(
            key()?,
            base(FIRECRAWL_BASE),
        )),
        "ollama" => Arc::new(OllamaProvider::with_base_url(key()?, base(OLLAMA_BASE))),
        "perplexity" => Arc::new(PerplexityProvider::with_base_url(
            key()?,
            base(PERPLEXITY_BASE),
        )),
        "marginalia" => Arc::new(MarginaliaProvider::with_base_url(base(MARGINALIA_BASE))),
        "searxng" => Arc::new(SearxngProvider::new(base(DEFAULT_INSTANCE_URL))),
        _ => return Err(format!("unknown web search provider '{id}'")),
    };
    Ok(provider)
}

/// The env var carrying the SearXNG instance URL override, for the
/// config-resolution layer.
pub fn searxng_url_env() -> &'static str {
    SEARXNG_URL
}
