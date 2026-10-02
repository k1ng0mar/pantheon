# Web search provider research

Researched 2026-09-29 for Pantheon's multi-provider web search (`pantheon-web`).
Requirement: server-side HTTP API returning JSON (title/url/snippet/dates), called via blocking HTTP.

## Comparison table

| # | Provider | Verdict | Auth | Free tier | Endpoint | Dates in results? |
|---|----------|---------|------|-----------|----------|-------------------|
| 0 | TinyFish | ✅ viable (recommended default) | API key (`X-API-Key`) | Free, no card (~30 req/min) | `GET https://api.search.tinyfish.ai/` | sometimes (`date`) |
| 1 | Tavily | ✅ viable (already built) | API key | 1,000 credits/mo, no card | `POST https://api.tavily.com/search` | sometimes (`published_date`) |
| 2 | Ollama Web Search | ✅ viable | Free Ollama account key | Generous free tier, limits unpublished | `POST https://ollama.com/api/web_search` | no |
| 3 | Exa | ✅ viable | API key (`x-api-key`) | $20 signup + $10/mo recurring, no card | `POST https://api.exa.ai/search` | yes (`publishedDate`) |
| 4 | Marginalia | ✅ viable w/ caveats (keyless niche) | None ("public" key) or free key on request | Keyless shared quota | `GET https://api.marginalia.nu/public/search/{q}?count={n}` | no |
| 5 | Brave Search | ✅ viable w/ caveats | API key (`X-Subscription-Token`) | $5/mo credits, **card required** | `GET https://api.search.brave.com/res/v1/web/search` | yes |
| 6 | Firecrawl | ✅ viable w/ caveats | API key | 1,000 credits/mo, no card | `POST https://api.firecrawl.dev/v2/search` | yes (metadata) |
| 7 | SearXNG | ✅ viable w/ caveats (self-host only) | None | Free (self-host) | `GET {instance}/search?q=...&format=json` | no |
| 8 | Perplexity Search | ✅ viable w/ caveats | API key | **None** (prepaid) | `POST https://api.perplexity.ai/search` | yes (`date`, `last_updated`) |
| 9 | OpenSERP | ⚠️ viable w/ caveats (local scrape) | None | Free (local binary) | `GET 127.0.0.1:7000/mega/search` (self-hosted) | varies |
| 10 | Kagi | ⚠️ opt-in only | API key (`Authorization: Bot ...`) + subscription | **None** | `GET https://kagi.com/api/v0/search?q=...` | yes |
| 11 | DuckDuckGo | ❌ not viable | - | - | - | - |
| 12 | Google CSE | ❌ dead | - | - | - | - |

## Per-provider notes

### 0. TinyFish - recommended default
- **Verified against the official docs** (<https://docs.tinyfish.ai/api-reference/search-the-web>, read 2026-09-29).
- Auth: API key in the `X-API-Key` header (NOT `Authorization: Bearer`). Free key, no credit card, from `agent.tinyfish.ai` (API Keys page). Env var: `TINYFISH_API_KEY`.
- Endpoint: `GET https://api.search.tinyfish.ai/?query=<urlencoded>` - required `query` param (1-2000 chars); native comma-separated `include_domains` / `exclude_domains`; optional `purpose` (ranking signal), `location`, `language`, `domain_type` (`web`|`news`|`research_paper`), date bounds (`after_date`/`before_date`/`recency_minutes`), `page` (0-10). No result-count parameter - trim client-side.
- Response: `{query, results: [{position, site_name, title, snippet, url}], total_results, page}` - rank-stable, p50 < 0.5s. Third-party integrations also report a `date` field when known; mapped when present, never fabricated.
- Rate limit: ~30 req/min on the free tier (one integration reports 5 req/min on the default plan - back off on 429). The Pantheon implementation paces itself: 2s minimum between requests by default.
- **Fetch endpoint** (`POST https://api.fetch.tinyfish.ai`, URL → clean markdown, also free) overlaps Pantheon's extraction story - **not built on**; noted as a future extraction-backend option.
- Sources: https://docs.tinyfish.ai/api-reference/search-the-web

### 1. Brave Search - viable with caveats
- Auth: API key in `X-Subscription-Token` header. Signup: https://api-dashboard.search.brave.com/
- Endpoint: `GET https://api.search.brave.com/res/v1/web/search` (+ `/res/v1/llm/context` for LLM-ready context)
- Free tier **changed Feb 2026**: old 2,000/mo free plan retired. Now $5/month free credits (~1,000 queries) **with credit card required**; overages billed. Attribution required to claim credits.
- Pricing: ~$0.003 - $0.005/query ($5/1K).
- Response: `{web: {results: [{title, url, description, age, ...}]}}` - clean JSON, independent 40B+ page index.
- Red flag: card-required signup is the main friction for a solo user.
- Sources: https://api-dashboard.search.brave.com/documentation/pricing, https://github.com/drvivek34/free-api-bazaar/blob/HEAD/search-web/brave-search-api/README.md

### 2. Firecrawl - viable with caveats
- Auth: `Authorization: Bearer fc-...`. Signup: https://www.firecrawl.dev/
- Endpoint: `POST https://api.firecrawl.dev/v2/search` - web search + optional full-page scrape of results in one call (`scrapeOptions`).
- Free tier: 1,000 credits/month, no card. Search costs 2 credits per 10 results. Paid from $16/mo (Hobby).
- Self-hosted: AGPL-3.0, Docker Compose stack (~1-2 GB RAM); usable but operationally heavy.
- Response: `{success, data: [{url, title, markdown, metadata}]}` - note: returns scraped page content, not just snippets (heavier payloads).
- Sources: https://docs.firecrawl.dev/, https://github.com/firecrawl/firecrawl/blob/main/README.md

### 3. Tavily - viable (current provider)
- Auth: API key (`tvly-...`), in body or Bearer. Signup: https://app.tavily.com/home
- Endpoint: `POST https://api.tavily.com/search`
- Free tier: 1,000 credits/month (Researcher plan), **no card**. Basic search = 1 credit, advanced = 2.
- Response: `{results: [{title, url, content, score, published_date}]}` - agent-native, includes extract/crawl endpoints.
- Sources: https://docs.tavily.com/, https://github.com/richard-31415/ai-agents-2026/blob/HEAD/skills/tavily-search/SKILL.md

### 4. Exa - viable
- Auth: `x-api-key` header. Signup: https://dashboard.exa.ai/
- Endpoint: `POST https://api.exa.ai/search` (also `/contents`, `/answer`)
- Free tier: **$20 signup credit + $10/month recurring**, no card. Then $7/1K searches.
- Response: `{results: [{title, url, text, highlights, publishedDate}]}` - neural/semantic search, built for agents. `type` enum: `instant|fast|auto|deep-lite|deep|deep-reasoning` (`neural`/`keyword` removed server-side - don't send them).
- Sources: https://exa.ai/docs, https://github.com/bnivanov/omp-extended-search/blob/HEAD/docs/exa.md

### 5. SearXNG - viable with caveats (self-host only)
- Auth: none. Self-host: `docker run` / compose; point config at the instance URL.
- Endpoint: `GET {instance}/search?q=...&format=json` - 70+ engines, category filters (`categories=news`, `time_range`, `engines=`).
- Free tier: free forever (your hardware). **Public instances are NOT usable**: live probes return 403/429 on `format=json` - public instances bot-gate the JSON API.
- Response: `{results: [{title, url, content, engine}]}`.
- Red flag: engine breakage is silent (0 results); keep the image updated. Scraped engines' ToS applies.
- Sources: https://docs.searxng.org/, https://github.com/georgernstgraf/opencode-helpers/blob/HEAD/skills/searxng/SKILL.md

### 6. OpenSERP - viable with caveats (local scrape)
- What it is: MIT-licensed Go binary (`github.com/karust/openserp`) - self-hosted SERP API + CLI for Google, Bing, Yandex, Baidu, DuckDuckGo, Ecosia. **Actively maintained** (commits days old).
- Auth: none. Run: `openserp serve` → `http://127.0.0.1:7000`.
- Endpoint: per-engine paths + `GET /mega/search` (multi-engine, merged/deduped). JSON/Markdown/Text/NdJSON output.
- Red flags: it's a scraper - results depend on engines not blocking the host IP; ToS gray area vs. search engines' terms. Good as a no-key local fallback, not a primary.
- Sources: https://github.com/karust/openserp, https://openserp.org/docs/architecture/

### 7. Perplexity - viable with caveats
- Auth: `Authorization: Bearer pplx-...`. Signup: https://www.perplexity.ai/settings/api
- **It does have a raw search API**: `POST https://api.perplexity.ai/search` - ranked results, no LLM synthesis. Separate from Sonar chat completions (`/chat/completions`) and the newer Agent API.
- Free tier: **none** - prepaid credits only. Search API: $5/1K requests.
- Response: `{results: [{title, url, snippet, date, last_updated}]}` - the strongest date fields and filters (`search_recency_filter`, domain allow/deny lists, date bounds).
- Note: Sonar chat models were migrated toward the Agent API (Sept 2026); the **Search API is unaffected**.
- Sources: https://docs.perplexity.ai/, https://github.com/perplexityai/perplexity-node/blob/HEAD/README.md

### 8. Kagi - opt-in only
- Auth: `Authorization: Bot <token>` (note the `Bot` scheme, not Bearer). Key from https://kagi.com/settings/api - requires a Kagi account.
- Endpoint: `GET https://kagi.com/api/v0/search?q=...`
- Free tier: **none**. Pricing: **$25/1K queries** (2.5¢/search) + requires a Kagi subscription. Historically closed beta (invite via support@kagi.com); availability may vary.
- Response: `{meta: {api_balance, ...}, data: [{title, url, snippet, ...}]}` - consensus best result quality, no SEO spam.
- Verdict: power-user opt-in only, not a default. User supplies their own key + credits (no resell/ToS issue for us - we never touch their key).
- Sources: https://help.kagi.com/kagi/api/search.html, https://github.com/istar-eldritch/ai-tools/blob/HEAD/skills/kagi-search/references/api-reference.md

### 9. Ollama Web Search - viable (surprise)
- **Real hosted API**, launched Sept 2025 - not just local-model confusion.
- Auth: free Ollama account API key (`OLLAMA_API_KEY` from https://ollama.com/settings/keys). No card.
- Endpoint: `POST https://ollama.com/api/web_search` (+ `/api/web_fetch` for page fetch). Local daemon proxies the same path at `http://localhost:11434/api/web_search` when signed in (`ollama signin`).
- Free tier: "generous free tier for individuals"; exact limits unpublished - back off on 429. Paid cloud plans raise limits.
- Request: `{"query": "...", "max_results": 5}` (max 10). Response: `{results: [{title, url, content}]}`.
- Red flag: unpublished limits; single-vendor dependency. But it's the cheapest legit hosted option after the keyless ones.
- Sources: https://docs.ollama.com/capabilities/web-search, https://ollama.com/blog/web-search

### 10. DuckDuckGo - NOT viable
- All programmatic HTTP paths are now bot-walled: `html.duckduckgo.com/html/`, `lite.duckduckgo.com/lite/`, and even `api.duckduckgo.com` return **HTTP 202 + "select all squares containing a duck" CAPTCHA** to non-browser clients (live-probed Sept 2026, including from residential-looking setups).
- The popular `duckduckgo-search` Python library (deedy5) **abandoned DDG as a backend** and switched to Bing. Instant Answer API never returned full web results anyway.
- Verdict: do not build on scraping DDG - unreliable and ToS-hostile.
- Sources: https://github.com/avifenesh/tools/blob/HEAD/agent-knowledge/self-contained-websearch-backends.md, https://github.com/pontscho/prompt-heaven/blob/HEAD/docs/concepts/spec-ddg.md

### 11. Marginalia - viable with caveats (keyless niche)
- **Documented public JSON API** - not a scrape: `GET https://api.marginalia.nu/public/search/{query}?count={n}` works keyless (shared "public" key; shared rate budget, 503s when drained). Keyed tier: `https://api2.marginalia-search.com/search` with `API-Key` header - keys granted liberally on request (kontakt@marginalia.nu).
- Response: `{results: [{url, title, description, quality, ...}]}` - maps 1:1 to title/url/snippet.
- Index is the "small web": blogs, forums, docs, indie sites. Excellent for technical/niche queries; **weak on mainstream, commercial, and very-recent results**.
- License: results carry **CC-BY-NC-SA 4.0** (fine for an agent reading them; matters if redistributing).
- Verdict: best ToS-clean keyless option - ship as a fallback/supplement, never the primary.
- Sources: https://about.marginalia-search.com/article/api/, https://github.com/agntn/web/blob/HEAD/docs/content/2.providers/13.marginalia.md

### 12. Google - dead, do not build
- Custom Search JSON API (`GET https://www.googleapis.com/customsearch/v1`): **closed to new customers in 2025, full shutdown Jan 1, 2027** (official deprecation notice).
- Was 100 queries/day free, $5/1K after. No replacement announced (Google points at Vertex AI Search - site-scoped, not open web).
- Verdict: skip entirely. (Serper scrapes Google results if Google-quality is ever needed - 2,500 free one-time queries - but it's a SERP wrapper, not evaluated here.)
- Sources: https://developers.google.com/custom-search/custom-search-api-list, https://github.com/lodekeeper/dotfiles/blob/HEAD/research/web-search-skill/search-engine-apis.md

## Recommended built-in set (ranked)

0. **TinyFish** - recommended default; free, no card, rank-stable, p50 <0.5s.
1. **Tavily** - keep as implemented; 1K/mo free, no card, agent-native.
2. **Ollama Web Search** - free-account hosted API; cheapest legit hosted path.
3. **Exa** - $20+$10/mo recurring free, no card; semantic search quality.
4. **Marginalia** - keyless, documented, ToS-clean; niche-index supplement.
5. **Brave** - independent index; $5/mo credits but card required.
6. **Firecrawl** - search+scrape in one; 1K credits/mo free, no card.
7. **SearXNG** - self-hosted power-user option (never public instances).
8. **Perplexity Search** - raw results with great date filters; prepaid only.
9. **OpenSERP** - local no-key fallback; scraper brittleness.
10. **Kagi** - opt-in for users who already pay for it.

Auth summary: user-supplied API key → TinyFish (free, no card), Tavily, Ollama (free account), Exa, Brave, Firecrawl, Perplexity, Kagi.
Keyless → Marginalia. Self-hosted/local → SearXNG, OpenSERP.

## Implementation status (2026-09-29)

Nine providers are built in as `SearchProvider` implementations in
`crates/pantheon-web/src/websearch/` (blocking HTTP via ureq; the trait is
sync). OpenSERP and Kagi were not picked and are not implemented.

| Provider | File | Auth | Key env var | Dates mapped |
|---|---|---|---|---|
| TinyFish (recommended) | `tinyfish.rs` | `X-API-Key` header | `TINYFISH_API_KEY` | `date` (when present) |
| Tavily | `tavily.rs` | API key (body or Bearer) | `TAVILY_API_KEY` | `published_date` |
| Ollama Web Search | `ollama.rs` | `Authorization: Bearer` (free account key) | `OLLAMA_API_KEY` | none - `published` is `None` |
| Exa | `exa.rs` | `x-api-key` header | `EXA_API_KEY` | `publishedDate` |
| Marginalia | `marginalia.rs` | none (keyless) | - | none - `published` is `None` |
| Brave Search | `brave.rs` | `X-Subscription-Token` header | `BRAVE_API_KEY` | `age` (relative string, as returned) |
| Firecrawl | `firecrawl.rs` | `Authorization: Bearer` | `FIRECRAWL_API_KEY` | `metadata.publishedTime` (fallback `modifiedTime`) |
| SearXNG | `searxng.rs` | none (self-hosted) | - | none - `published` is `None` |
| Perplexity Search | `perplexity.rs` | `Authorization: Bearer` | `PERPLEXITY_API_KEY` | `date` (fallback `last_updated`) |

Auth-scheme double-checks (the Kagi lesson): Exa really is `x-api-key`,
Brave really is `X-Subscription-Token`, Firecrawl/Ollama/Perplexity really
are `Authorization: Bearer`. The two uncertain mappings are flagged in
code: Ollama's Bearer scheme (unverifiable without a key) and Firecrawl's
`metadata.publishedTime` field name (documented shape, unverified live).

Provider registry for the setup picker: `pantheon_web::websearch`
re-exports `all_providers() -> Vec<ProviderInfo>`, `provider_info(id)`,
`default_key_env(id)`, `build_provider(id, key, base_url_override)`,
plus `ProviderAuth::{ApiKey { env_var }, Keyless, SelfHosted { default_url }}`
and `ProviderInfo { id, name, auth, blurb }`. Provider ids are stable and
match `[websearch] provider` config values and `SearchProvider::name()`.

Keyless/self-hosted notes:
- **Marginalia** runs on a shared keyless quota (expect 503s when drained);
  its niche small-web index supplements but never replaces a primary.
  Results carry **CC-BY-NC-SA 4.0** - fine for an agent reading them, it
  matters only if redistributing.
- **SearXNG** is self-hosted only: point `[websearch] base_url` (or the
  `SEARXNG_URL` env var) at your own instance. Never use a public
  instance - they bot-gate `format=json` (403/429). When nothing is
  configured the registry default `http://localhost:8080` applies.
- **Firecrawl** responses include scraped page content; the implementation
  keeps snippets only (`metadata.description`) and sends no `scrapeOptions`,
  so no full-page scrapes happen by default.
- **TinyFish** paces itself at 2s between requests by default (~30 req/min
  free tier; override via `with_min_interval`, zero disables). Its Fetch
  endpoint (`api.fetch.tinyfish.ai`, URL → clean markdown, also free) is
  deliberately not built on - it overlaps Pantheon's extraction story and
  is noted here as a future extraction-backend option.

Config: `[websearch]` takes `provider` (one of the 8 ids), `api_key_secret`
(defaults to the provider's own env var, e.g. `EXA_API_KEY`), `base_url`
(endpoint override for self-hosted providers), `enabled`, `max_results`.
Keys are resolved by the caller via the secrets broker/env and handed to
the provider constructors; the crate never reads env or the vault and
never logs key material.

Cost honesty (also in picker blurbs): Brave needs a credit card ($5/mo
credits); Perplexity Search has no free tier (prepaid); Exa's free tier is
$20 signup + $10/mo recurring, no card; Tavily/Firecrawl are 1K credits/mo
free, no card; Ollama's free-tier limits are unpublished.

Tests: response-parsing unit tests with fixture JSON per provider live
in-crate (deterministic; live HTTP only against unreachable loopback for
error-class assertions). No live API tests - keys were not available, so
live behavior is unverified.
