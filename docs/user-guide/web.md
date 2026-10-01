# Web access

Pantheon can use the web in two different ways, and the distinction matters:

- **`web_search`** — *looking something up.* Facts, news, docs, prices, "what is X". Query in, snippets out.
- **`browser_*` tools** — *doing something on a live site.* Navigate, click, fill forms, read JS-heavy pages, extract structured data from a specific page.

The model picks by intent. "What does this error mean" → search. "Check my order status on this site" → browser.

## Web search

Nine providers are built in, with **TinyFish** the recommended default (free, no card, rank-stable): TinyFish, Tavily, Ollama Web Search, Exa, Marginalia (keyless), Brave Search, Firecrawl, SearXNG (self-hosted), and Perplexity Search. Keyed providers need an API key, e.g.:

```sh
pantheon secrets set TINYFISH_API_KEY <your-key>  # or TAVILY_API_KEY, EXA_API_KEY, … for another provider
```

Without the provider's auth requirement met, `web_search` simply doesn't appear in the model's tool list — a tool that can never work stays out of the way. Config (`[websearch]` in `config.toml`):

```toml
[websearch]
enabled = true
provider = "tinyfish"   # one of: tinyfish (recommended), tavily, ollama, exa, marginalia, brave, firecrawl, searxng, perplexity
# api_key_secret = "TINYFISH_API_KEY"  # optional override; absent = the provider's default secret name
max_results = 8
```

## Browser automation

Pantheon drives a real Chromium through the [gsd-browser](https://github.com/gsd-build/gsd-browser) binary (MIT/Apache licensed). Install it once:

```sh
curl -fsSL https://install.gsd.build/browser | bash
```

If the binary is missing, the `browser_*` tools fail with a clear error telling you exactly that — nothing crashes.

How it works:

- **One browser session per Pantheon run.** The daemon starts lazily on the first `browser_*` call and stops when the run ends or after 15 minutes idle (configurable via `idle_timeout_secs`), so headless Chromes don't accumulate.
- **Refs, not selectors.** `browser_snapshot` returns stable refs like `@v1:e1`; `browser_click_ref` / `browser_fill_ref` act on them. Refs are invalidated on page change by design — a stale ref errors loudly instead of clicking the wrong thing, and the model re-snapshots.
- **Auth vault.** Site credentials live in gsd-browser's encrypted vault; Pantheon injects the vault key from its own secrets (`vault_key_secret`, default `GSD_BROWSER_VAULT_KEY`) into the browser subprocess environment. The key is never logged.

Config (`[browser]` in `config.toml`):

```toml
[browser]
enabled = true
# binary = "/usr/local/bin/gsd-browser"  # absent = resolved from PATH
act_require_approval = true
idle_timeout_secs = 900
vault_key_secret = "GSD_BROWSER_VAULT_KEY"
```

### `browser_act` and approvals

`browser_act` is the one browser tool that acts without a verified target: it clicks the top semantic-intent candidate (`accept_cookies`, `primary_cta`, …) with no minimum confidence threshold upstream. It carries the `browser.act` capability, which the default policies mark as approval — the run parks for your permission before it runs. The safer pattern, which the tool description steers the model toward, is snapshot → verify the ref → `browser_click_ref`. Leave `act_require_approval = true` unless you trust autonomous low-confidence clicks.

### Sandboxing

The browser subprocess intentionally runs outside Pantheon's namespace sandbox: a browser that can't reach the network or its own Unix socket can't browse. The capability gate (`browser` / `browser.act`) is the authorization boundary, and the binary is user-installed trusted software, like the browser itself.
