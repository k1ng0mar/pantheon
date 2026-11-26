//! Provider registry for the setup wizard's web-search picker.
//!
//! [`all_providers`] lists every built-in provider with its auth requirement
//! and a one-line blurb so the TUI wizard can present the choice without
//! knowing any provider internals. Ids are stable: they are the values
//! accepted for `[websearch] provider` in config.toml and the strings from
//! [`SearchProvider::name`].
//!
//! [`build_provider`] is the runtime construction seam: given an id, an
//! optional resolved key, and an optional base-URL override, it returns a
//! boxed [`SearchProvider`] or a human-readable reason it can't.

use super::brave::{BraveProvider, BRAVE_API_KEY, DEFAULT_BASE_URL as BRAVE_BASE};
use super::ddgs::DdgsProvider;
use super::exa::{ExaProvider, DEFAULT_BASE_URL as EXA_BASE, EXA_API_KEY};
use super::exa_free::{ExaFreeProvider, DEFAULT_BASE_URL as EXA_FREE_BASE};
use super::firecrawl::{FirecrawlProvider, DEFAULT_BASE_URL as FIRECRAWL_BASE, FIRECRAWL_API_KEY};
use super::firecrawl_free::{FirecrawlFreeProvider, DEFAULT_BASE_URL as FIRECRAWL_FREE_BASE};
use super::keenable::{KeenableFreeProvider, DEFAULT_BASE_URL as KEENABLE_BASE};
use super::marginalia::{MarginaliaProvider, DEFAULT_BASE_URL as MARGINALIA_BASE};
use super::ollama::{OllamaProvider, DEFAULT_BASE_URL as OLLAMA_BASE, OLLAMA_API_KEY};
use super::parallel::{ParallelFreeProvider, DEFAULT_BASE_URL as PARALLEL_FREE_BASE};
use super::perplexity::{
    PerplexityProvider, DEFAULT_BASE_URL as PERPLEXITY_BASE, PERPLEXITY_API_KEY,
};
use super::provider::SearchProvider;
use super::searxng::{SearxngProvider, DEFAULT_INSTANCE_URL, SEARXNG_URL};
use super::tavily::{TavilyProvider, DEFAULT_BASE_URL as TAVILY_BASE};
use super::tinyfish::{TinyFishProvider, DEFAULT_BASE_URL as TINYFISH_BASE, TINYFISH_API_KEY};
use super::tools::TAVILY_API_KEY;
use std::sync::Arc;

/// how a provider authenticates, as the picker needs to know
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderAuth {
    /// user-supplied API key; `env_var` is the default secret/env name the
    /// parent resolves the key from (e.g. `"EXA_API_KEY"`)
    ApiKey { env_var: &'static str },
    /// no key, works out of the box
    Keyless,
    /// self-hosted: no key, but the user must point it at their instance;
    /// `default_url` is the prefill when nothing is configured
    SelfHosted { default_url: &'static str },
    /// local binary: no key, but a program must exist on `PATH`; the wizard
    /// detects it and offers install-or-skip
    Local {
        detect_cmd: &'static str,
        install_hint: &'static str,
        install_cmd: Option<&'static str>,
    },
}

/// one picker row; all `&'static str` because the registry is a compile-time
/// constant table
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderInfo {
    /// stable id: the `[websearch] provider` config value and
    /// [`SearchProvider::name`]. never rename an id.
    pub id: &'static str,
    /// display name for the picker
    pub name: &'static str,
    /// what the user must supply to use it
    pub auth: ProviderAuth,
    /// one-line blurb. cost honesty lives here: free tiers, cards, prepaid
    /// only, all stated plainly.
    pub blurb: &'static str,
    /// whether the picker highlights this as recommended; exactly one at a
    /// time, and [`all_providers`] sorts recommended ones first
    pub recommended: bool,
}

/// every provider, pre-sorted: recommended first (TinyFish today), then the
/// rest. add new providers here and only here; set `recommended` on at most
/// one.
pub fn all_providers() -> Vec<ProviderInfo> {
    vec![
        ProviderInfo {
            id: "tinyfish",
            name: "TinyFish",
            auth: ProviderAuth::ApiKey { env_var: TINYFISH_API_KEY },
            blurb: "Recommended default. Rank-stable, p50 <0.5s; free, no card. Paced at 2s between requests (≈30/min) with Retry-After honoured - but free-tier limits vary by plan and some report 5 req/min, which 2s pacing does NOT stay under, so those plans still throttle.",
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
        // keyless fallbacks: shared quotas, expect rate limits. never
        // recommended; they catch what keyed providers miss with no key
        ProviderInfo {
            id: "exa_free",
            name: "Exa (anonymous)",
            auth: ProviderAuth::Keyless,
            blurb: "Exa's public endpoint, no key; shared quota, expect throttling.",
            recommended: false,
        },
        ProviderInfo {
            id: "parallel_free",
            name: "Parallel (anonymous)",
            auth: ProviderAuth::Keyless,
            blurb: "Parallel's public endpoint, no key; shared quota, expect throttling.",
            recommended: false,
        },
        ProviderInfo {
            id: "firecrawl_free",
            name: "Firecrawl (anonymous)",
            auth: ProviderAuth::Keyless,
            blurb: "Firecrawl's public endpoint, no key; shared quota, expect throttling.",
            recommended: false,
        },
        ProviderInfo {
            id: "keenable_free",
            name: "Keenable (anonymous)",
            auth: ProviderAuth::Keyless,
            blurb: "Keenable's public endpoint, no key; shared quota, expect throttling.",
            recommended: false,
        },
        ProviderInfo {
            id: "ddgs",
            name: "DuckDuckGo (local ddgs)",
            auth: ProviderAuth::Local {
                detect_cmd: "command -v ddgs",
                install_hint: "DuckDuckGo metasearch needs the `ddgs` package (`python3 -m pip install ddgs`; on Debian/Ubuntu you may need `apt install python3-pip` first); scraping-adjacent, results vary.",
                install_cmd: Some("python3 -m pip install ddgs"),
            },
            blurb: "Local DuckDuckGo metasearch via the `ddgs` binary; no key, scraping-adjacent.",
            recommended: false,
        },
    ]
}

/// look up one provider's [`ProviderInfo`] by id, case-insensitive
pub fn provider_info(id: &str) -> Option<ProviderInfo> {
    all_providers()
        .into_iter()
        .find(|p| p.id.eq_ignore_ascii_case(id.trim()))
}

/// default secret/env name for `id`'s key, or `None` for keyless /
/// self-hosted
pub fn default_key_env(id: &str) -> Option<&'static str> {
    match provider_info(id)?.auth {
        ProviderAuth::ApiKey { env_var } => Some(env_var),
        ProviderAuth::Keyless | ProviderAuth::SelfHosted { .. } | ProviderAuth::Local { .. } => {
            None
        }
    }
}

/// wrapper enforcing the empty-query rule at the provider layer for direct
/// `build_provider` use (the tool layer rejects them too). No provider
/// rejects one on its own, so without this an empty query goes upstream.
/// Also normalizes `max_results == 0` to 1 (see
/// [`super::provider::SearchOptions::max_results`]), so a registry-built
/// provider never sees it even if its own trim forgets `.max(1)`.
struct QueryGuardProvider {
    inner: Arc<dyn SearchProvider>,
}

impl SearchProvider for QueryGuardProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn search(
        &self,
        query: &str,
        opts: &super::provider::SearchOptions,
    ) -> Result<Vec<super::provider::SearchResult>, super::error::SearchError> {
        if query.trim().is_empty() {
            return Err(super::error::SearchError::RequestFailed(
                "search query must not be empty".to_string(),
            ));
        }
        let mut opts = opts.clone();
        opts.max_results = opts.max_results.max(1);
        self.inner.search(query, &opts)
    }
}

/// Build a boxed provider for `id` (case-insensitive).
///
/// - `api_key`: already-resolved key for keyed providers; blank/missing is
///   an error naming the provider and its env var.
/// - `base_url_override`: for self-hosted providers (SearXNG instance URL)
///   and as a test mirror for any provider. `None` -> each provider's
///   default. PER-PROVIDER ONLY: pass the override for the `id` being
///   built, never a global value — a global override follows a provider
///   switch and sends another provider's calls (and key) to the wrong host.
///   The session passes `WebsearchSection::base_url_for(&provider)`; the
///   legacy global `[websearch] base_url` is deprecated and ignored.
///
/// Error is a human-readable string for logs/diagnostics, never key material.
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
        "exa_free" => Arc::new(ExaFreeProvider::with_base_url(base(EXA_FREE_BASE))),
        "parallel_free" => Arc::new(ParallelFreeProvider::with_base_url(base(
            PARALLEL_FREE_BASE,
        ))),
        "firecrawl_free" => Arc::new(FirecrawlFreeProvider::with_base_url(base(
            FIRECRAWL_FREE_BASE,
        ))),
        "keenable_free" => Arc::new(KeenableFreeProvider::with_base_url(base(KEENABLE_BASE))),
        "ddgs" => Arc::new(DdgsProvider::new()),
        _ => return Err(format!("unknown web search provider '{id}'")),
    };
    Ok(Arc::new(QueryGuardProvider { inner: provider }))
}

/// env var carrying the SearXNG instance URL override, for the config layer
pub fn searxng_url_env() -> &'static str {
    SEARXNG_URL
}

#[cfg(test)]
mod tests {
    use super::super::provider::SearchOptions;
    use super::*;

    #[test]
    fn build_provider_rejects_empty_query_at_provider_layer() {
        let provider = build_provider("tinyfish", Some("test-key".to_string()), None)
            .expect("tinyfish builds with a key");
        assert_eq!(provider.name(), "tinyfish");
        let err = provider
            .search("   ", &SearchOptions::default())
            .expect_err("empty query must be rejected before any network call");
        assert!(matches!(
            err,
            super::super::error::SearchError::RequestFailed(_)
        ));
    }

    /// guard's `max_results` normalization through a stub (no network):
    /// 0 arrives at the inner provider as 1.
    #[test]
    fn query_guard_clamps_zero_max_results_to_one() {
        use super::super::error::SearchError;
        use super::super::provider::{SearchProvider, SearchResult};
        use std::sync::Mutex;

        struct CapturingProvider {
            seen: Mutex<Vec<u8>>,
        }

        impl SearchProvider for CapturingProvider {
            fn name(&self) -> &str {
                "stub"
            }

            fn search(
                &self,
                _query: &str,
                opts: &SearchOptions,
            ) -> Result<Vec<SearchResult>, SearchError> {
                self.seen.lock().unwrap().push(opts.max_results);
                Ok(Vec::new())
            }
        }

        let stub = Arc::new(CapturingProvider {
            seen: Mutex::new(Vec::new()),
        });
        let guard = QueryGuardProvider {
            inner: stub.clone(),
        };
        let opts = SearchOptions {
            max_results: 0,
            ..SearchOptions::default()
        };
        guard.search("hello", &opts).expect("guard passes through");
        assert_eq!(*stub.seen.lock().unwrap(), vec![1]);
    }
}
