# hermes-web-search-plus

Multi-provider web search and page extraction with automatic failover.

Provider contracts ported from
[robbyczgw-cla/hermes-web-search-plus](https://github.com/robbyczgw-cla/hermes-web-search-plus)
(MIT; see NOTICE) — the endpoints, auth headers, and response shapes
verified against its `providers.py`. The agent machinery around them
was not vendored; this is a lean stdlib Pantheon tool runner.

## Tools

- `web_search` — search the web. Arguments: `query` (required), `num`
  (1–20, default 10), `provider` (optional: force one provider).
- `extract_page` — extract readable text from a URL. Arguments: `url`
  (required), `provider` (optional; `"direct"` selects the direct
  fetch), `max_chars` (default 8000).

Both tools try providers in a fixed order until one succeeds:
Serper → SerpBase → Brave → Tavily → Linkup → Firecrawl → Exa →
You.com → SearXNG → Keenable. A provider participates when its key is
set (see manifest.yaml); SearXNG participates when `SEARXNG_INSTANCE`
is set. Keenable also works with **no key** via its public endpoint,
so the plugin is usable with zero configuration — and extraction has a
final SSRF-guarded direct-fetch fallback.

## Security posture

Every result carries an untrusted-content notice first. Web content is
attacker-controlled: the agent must treat it as data, never as
instructions from any principal.

- Provider errors never reflect raw response text — fixed message plus
  HTTP status only. API keys never appear in logs or results.
- The direct-fetch fallback validates URL and DNS before connecting:
  http/https only, no userinfo, no backslash/whitespace/control
  characters, IDNA hostname, DNS resolution with rejection of
  non-global IPs (including IPv4-mapped IPv6), at most 5 redirects
  with each hop re-validated, 25s timeout, 2MB cap, text content types
  only.
- Residual risk, documented as upstream documents it: DNS rebinding
  between the pre-flight check and connect. The check happens
  immediately before the request, but a hostile resolver can still race
  it. Do not point the direct fetcher at internal hostnames.
- No network at import or enable time; no phone-home. Each tool call
  contacts only the providers needed to serve it.

## Provider privacy

Each provider receives the query text and your API key; search
providers see what you search for, extraction providers see the URLs
you extract. Consult each provider's privacy policy before sending
sensitive queries. The keyless path sends queries to Keenable's public
endpoint.

## Configuration

All `env_vars` are optional — the plugin verifies with no keys via the
keyless path. Keys live in the Pantheon environment (never in files);
Pantheon secrets never cross into plugins, so this plugin reads only
its own declared provider keys.
