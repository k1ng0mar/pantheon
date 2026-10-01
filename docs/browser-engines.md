# Browser Backend Lineup — Pantheon

Locked 2026-09-29 by Umar. Six backends behind the replaceable
[`BrowserBackend`](../../crates/pantheon-web/src/browser/backend.rs) trait
(navigate, DOM snapshots, ref-based interaction, extraction, screenshots,
headless operation). Nothing else gets added without Umar's explicit call.

## The lineup

| # | Backend | Role | Drive model from Rust | Weight | License / cost | Live status |
|---|---------|------|----------------------|--------|----------------|-------------|
| 1 | **GSD Browser** | Default interactive | Subprocess CLI (`--json`), daemon over `--cdp-url` or its own Chrome launch | Full Chromium + tiny Rust daemon | MIT/Apache-2.0, free | ✅ Verified live 2026-09-29 (gsd-browser 0.1.24): navigate, snapshot, extract, act, wait-for, daemon stop — see [verification](browser-verification.md) |
| 2 | **`chromiumoxide` 0.9** | Native CDP fallback | Native Rust crate, CDP websocket, tokio | Chrome's footprint only | Apache-2.0/MIT, free | ✅ Verified live 2026-09-29 (Chrome 152): navigate, snapshot, extract, wait-for, stale refs — no schema-drift breakage |
| 3 | **Steel** | Cloud / self-host | REST + raw CDP websocket; official Rust SDK (`steel-rs`) | Self-host: ~4GB base, ~200–500MB/session; cloud: zero | Apache-2.0 core; cloud Launch $0 + usage ($0.10/hr browser) | ⛔ Blocked: no API key on 2026-09-29 |
| 4 | **Browserbase** | Cloud | `POST /v1/sessions` → raw CDP `wss://` URL; no SDK needed | Cloud only | Free tier (1 browser-hr); Dev $20/mo (100 hrs, then $0.12/hr) | ⛔ Blocked: no API key on 2026-09-29 |
| 5 | **Lightpanda** | Extraction only | Sidecar `lightpanda serve` → CDP WS (preferred), or direct `--cdp-url` | ~10–24MB RSS baseline; ~140 instances on 8GB | AGPL-3.0, free (commercial license on request) — Umar explicitly waived the AGPL concern | ⛔ Blocked: no binary/server on 2026-09-29 |
| 6 | **`@playwright/cli`** | Official fallback | Subprocess CLI (`--json`), stateful sessions | Heaviest local (~200MB–1GB/instance) + Node | Apache-2.0, free | ⚠️ CLI help verified live; no real browser session yet |

## Backend details

### 1. GSD Browser (default)
Native **Rust** CDP client — not a browser itself; needs Chrome/Chromium on the
host. 90+ CLI commands with `--json` output. The Pantheon wrapper uses the
verified command vocabulary (0.1.24):

- Global shape: `gsd-browser --session <name> --json <command> [args]`
- `navigate <url>`, `snapshot` (returns `version` + `refs` keyed `e1`, `e2`),
  `click-ref @vN:eM` / `fill-ref` / `hover-ref`
- `extract --schema '{"properties": {...}}'` — the envelope is **required**;
  bare property maps are rejected by the binary, so Pantheon normalizes them
  before invoking
- `act --intent <submit_form|close_dialog|primary_cta|search_field|next_step|dismiss|auth_action|back_navigation>`
  — fixed semantic vocabulary; upstream scores candidates and clicks the top
  one with **no minimum score**, which is why `browser_act` carries the
  `browser.act` capability and parks for approval by default
- `wait-for --condition <selector_visible|selector_hidden|url_contains|network_idle|delay|text_visible|text_hidden|request_completed|console_message|element_count|region_stable>`
- `screenshot --output <path> --format <jpeg|png>`
- `daemon {start,stop,health}`; some failures return `{"error": ...}` on
  stdout **with exit 0** — the wrapper detects that and reports a failure
- Auth vault key is injected via `GSD_BROWSER_VAULT_KEY`, resolved through
  the secrets broker at registration; never logged, never in the ledger

Install: `npm install -g @opengsd/gsd-browser` (npm latest 0.2.2 on 2026-09-29;
the verified binary was 0.1.24) or build from
https://github.com/open-gsd/gsd-browser.

### 2. `chromiumoxide` 0.9 (native CDP)
`mattsse/chromiumoxide`, pinned `=0.9.1`. Tokio-native: `Browser::launch`/`connect`
+ `Page`; typed bindings for every CDP domain. Pantheon builds the ref model
(versioned `@vN:eM` refs over the Accessibility tree), extraction JS, and
session management on top — this is the "Pantheon builds ref model itself"
work, bounded and done.

- **CDP schema drift**: Chrome 142 broke chromiumoxide 0.6's protocol types;
  0.9 restored compat. Mitigations: unknown CDP events deserialize as
  `CdpEvent::Other` (verified in the 0.9.1 sources — no hard failures), and
  the live test drives Chrome 152 to prove newer-than-crate Chrome works.
- Quirk budget: tokio-only; the `Handler` future must be polled or the
  browser stalls; locale-fragile port parsing; generated protocol code slows
  compiles.

### 3. Steel (cloud / self-host)
`POST {base}/v1/sessions` → session + `wss://` CDP URL; self-host
(`docker run ghcr.io/steel-dev/steel-browser`) exposes the **identical** API
locally, so cloud and self-host are the same code path. Official Rust SDK
`steel-rs` 0.1 (in `Cargo.toml`). Driven through Pantheon's own CDP driver
after session creation, so refs/extraction/waits behave like the native
backend.

- Auth: cloud requires `STEEL_API_KEY` (secret name configurable); a custom
  self-host base URL is allowed without a key; the key is appended as
  `?apiKey=` to the CDP URL only when non-empty — never in error strings.

### 4. Browserbase (cloud)
`POST https://www.browserbase.com/v1/sessions` with API key + project id →
returns a raw CDP websocket URL, fed into Pantheon's CDP driver. No SDK
needed. Stealth, proxies, CAPTCHA solving on by default; session recordings
and live-view debug URLs.

### 5. Lightpanda (extraction only)
A from-scratch headless engine (not Chromium): own HTML parser/DOM with V8
for JS. Deliberately a **reduced surface**: only `navigate`, `extract`,
`page-source`, and `screenshot` are registered — no interaction tools, no
waits, no refs. Transport: a running `lightpanda serve` CDP URL, the only
supported transport (binary launch is not supported — Lightpanda's CLI is
not Chromium-flag-compatible, so there is no honest way to launch it
through chromiumoxide's browser config).
Restricted to commands that can run honestly against its CDP support; anything
else fails loudly with `UnsupportedCommand` rather than pretending.

### 6. `@playwright/cli` (official fallback)
Microsoft's official stateful agent CLI (`npx @playwright/cli`), wrapped as a
subprocess. Session-scoped: `playwright-cli -s=<session> --json <command>`.
`snapshot` returns refs; `click`/`fill`/`hover` accept a ref or unique
selector; `type <text>` types into the focused element; `eval <func>` runs
arbitrary JS; `screenshot --filename --type <png|jpeg|webp>`; `close` ends the
session. Heaviest local option and needs Node — which is why it is the
fallback, not the default.

## Config surface

`[browser]` in `config.toml` (see `pantheon-api`'s `BrowserSection`):

```toml
[browser]
backend = "gsd"            # gsd | chromiumoxide | steel | browserbase | lightpanda | playwright
binary = "/path/to/gsd-browser"   # gsd-browser binary; PATH when unset
act_require_approval = true       # browser_act parks for human approval
idle_timeout_secs = 900
vault_key_secret = "GSD_BROWSER_VAULT_KEY"   # secrets broker name; unset = no vault
steel_api_key_secret = "STEEL_API_KEY"       # unset = Steel unusable
steel_base_url = "..."                       # self-hosted Steel; unset = cloud
browserbase_api_key_secret = "BROWSERBASE_API_KEY"
browserbase_project_id = "..."
lightpanda_cdp_url = "ws://..."   # running `lightpanda serve`; the only Lightpanda transport
playwright_binary = "/path/to/playwright-cli"
chrome_binary = "/path/to/chrome" # chromiumoxide; unset = auto-detect
headless = true
timeout_secs = 120
```

Unknown `backend` ids warn and fall back to `gsd` — never fail the session
over a typo. Missing secrets for the cloud backends fail registration loudly
at startup (no dead tools in the model's list).

## What this replaces

The earlier research pass evaluated ten engines; Umar locked six. The four
that were cut are gone from the codebase and docs — no hooks, no fallback
references. If a cut approach ever becomes a requirement (it isn't one
today), revisit it then; don't pre-build.

## Open items (2026-09-29)
- Steel/Browserbase: live-verify once keys exist (contract tests are written;
  blocked on credentials).
- Lightpanda: verify against a real server once available (blocked on
  binary/server).
- Playwright CLI: run a real browser session (binary never launched headless
  Chrome in this environment yet).
- `pantheon-eval` (the crate this task's test plan assumed) does not exist in
  the 19-crate workspace; live tests live in-crate as `#[ignore]`d,
  env-gated tests for now.
