# Bundled MCP Server Research - Pantheon catalog

Research date: 2026-09-29. All facts verified against current official docs/registries (not memory).
"PINNED" = latest release/version observed on 2026-09-29; re-verify at implementation time.

---

## 1. GitHub - `github/github-mcp-server`

- **Doc URL:** https://github.com/github/github-mcp-server
- **Package:** Docker image `ghcr.io/github/github-mcp-server`
- **PINNED:** `ghcr.io/github/github-mcp-server:v1.12.2` (latest GitHub release, observed 2026-09-29)
- **Transport:** stdio via Docker. Exact command/args:
  ```json
  { "command": "docker",
    "args": ["run", "-i", "--rm", "-e", "GITHUB_PERSONAL_ACCESS_TOKEN", "ghcr.io/github/github-mcp-server:v1.12.2"] }
  ```
  No-Docker alternative: build from source (`go build` in `cmd/github-mcp-server`), then
  `command: "/path/to/github-mcp-server", args: ["stdio"]`.
- **Secrets (env var names only):**
- `GITHUB_PERSONAL_ACCESS_TOKEN` - classic PAT (takes precedence over OAuth)
- `GITHUB_OAUTH_CALLBACK_PORT` - set to `8085` with `-p 127.0.0.1:8085:8085` for the browser-based OAuth login flow (no token needed: the official image already bundles app credentials; resulting token kept in memory only)
- `GITHUB_HOST` / `--gh-host` - GitHub Enterprise Server or `*.ghe.com` data-residency host (HTTPS enforced)
- `GITHUB_TOOLSETS` / `--toolsets`, `GITHUB_TOOLS` / `--tools` - toolset/tool allow-lists
- **Setup notes:**
- PAT needs `repo`, `read:org`, `read:packages` (minimum scopes; grant only what the toolsets need).
- Headless/device-code OAuth fallback and "bring your own OAuth/GitHub App" are documented (see "Local Server OAuth Login" and "GitHub App Authentication" docs pages).
- Modes: `--read-only`, lockdown mode, `--insiders` / `GITHUB_INSIDERS=true`.
- **Local-only vs cloud:** Local container/binary; calls the GitHub cloud API. (Separate fully-hosted option exists: HTTP remote server `https://api.githubcopilot.com/mcp/` with OAuth or PAT `Authorization: Bearer` header - NOT part of this local bundle.)
- **Deprecation warnings:** none.

## 2. Google Workspace - pick: `taylorwilsdon/google_workspace_mcp`

- **Doc URL:** https://github.com/taylorwilsdon/google_workspace_mcp - full docs at https://workspacemcp.com
- **Why this pick:** most complete and best-maintained community server - 3,257 stars, 3,117 commits, 107 releases, actively committed (Sept 2026), MIT license, single server covering 12 services (Gmail, Drive, Calendar, Docs, Sheets, Slides, Forms, Tasks, Contacts, Chat, Custom Search, Apps Script) with 120+ tools, full docs site, published PyPI package. Alternatives rejected: `smkeramati/google-workspace-mcp` (only 4 services, multi-account focus, less active), `datatorag/gws-mcp` (thin wrapper over the `gws` CLI), `mindstone` connector (FSL-1.1-MIT license, npm not yet published).
- **Package:** PyPI `workspace-mcp`
- **PINNED:** `workspace-mcp==1.30.0` (latest PyPI release, observed 2026-09-29)
- **Transport:** stdio by default (`uvx workspace-mcp`); streamable HTTP with `--transport streamable-http` → `http://localhost:<WORKSPACE_MCP_PORT>/mcp`.
- **Secrets (env var names only):**
- `GOOGLE_OAUTH_CLIENT_ID`, `GOOGLE_OAUTH_CLIENT_SECRET` (or `GOOGLE_CLIENT_SECRET_PATH` → a `client_secret.json`)
- `MCP_ENABLE_OAUTH21=true` - OAuth 2.1 PKCE mode (HTTP transport only)
- `WORKSPACE_MCP_PORT`, `GOOGLE_OAUTH_REDIRECT_URI`, `OAUTHLIB_INSECURE_TRANSPORT=1` (local plain-HTTP only)
- **Setup notes (non-obvious):**
- You must create your own OAuth client in your own Google Cloud project and enable the API for each service you use; the redirect URI must match, e.g. `http://localhost:${WORKSPACE_MCP_PORT}/oauth2callback`.
- Launch tiers: `uvx workspace-mcp --tool-tier core|extended|complete`; or cherry-pick `--tools gmail drive calendar`; `--read-only` and per-service `--permissions` available.
- CLI: `workspace-cli` is installed from this repo (`uv tool install .`) - the maintainer explicitly warns: do NOT use `uvx workspace-cli`, that PyPI name is squatted by an abandoned package.
- Google Chat needs a one-time Chat app configuration and a Workspace account (not a free Gmail).
- Prompt-injection caution from maintainer: emails/docs can carry hidden instructions; be deliberate with write tools.
- **Local-only vs cloud:** Runs locally; calls Google cloud APIs.
- **"Use that" note:** Google now ships first-party remote MCP servers for three products only - `https://calendarmcp.googleapis.com/mcp/v1`, `https://gmailmcp.googleapis.com/mcp/v1`, `https://drivemcp.googleapis.com/mcp/v1` (remote HTTP + your own GCP OAuth client). They cover less than this server (no Drive-wide, Docs, Sheets, etc.), so the community server remains the pick for a bundle.

## 3. Chrome DevTools - `chrome-devtools-mcp`

- **Doc URL:** https://github.com/ChromeDevTools/chrome-devtools-mcp (52,745 stars, Apache-2.0)
- **Package:** npm `chrome-devtools-mcp`
- **PINNED:** `chrome-devtools-mcp@1.10.1` (latest npm, observed 2026-09-29)
- **Transport:** stdio. Exact command/args: `npx -y chrome-devtools-mcp@1.10.1`
- **Secrets:** none.
- **Setup notes (non-obvious):**
- Requires a current stable Google Chrome installed on the same machine + Node.js LTS. The server auto-starts Chrome on the first tool call that needs a browser - no Chrome window needed beforehand.
- Usage statistics are **ON by default** (Google collects tool-invocation success rates, latency, env info). Opt out with flag `--no-usage-statistics` or env `CHROME_DEVTOOLS_MCP_NO_USAGE_STATISTICS=1`; `CI=true` also disables.
- Update checks against the npm registry are ON by default: disable with `CHROME_DEVTOOLS_MCP_NO_UPDATE_CHECKS=1`.
- Performance tools call the Google CrUX API for field data: disable with `--no-performance-crux`.
- Useful flags: `--headless`, `--slim` (3 tools only: navigation, script execution, screenshots), `--browser-url=http://127.0.0.1:9222` (attach to running Chrome), `--user-data-dir=PATH`, `--channel=stable`, `--executablePath=/path/to/chrome`.
- **Local-only vs cloud:** Fully local (drives local Chrome). Privacy caveat: telemetry + CrUX calls go to Google unless disabled.
- **Deprecation warnings:** none.

## 4. Cloudflare - `cloudflare/mcp-server-cloudflare`

- **Doc URL:** https://github.com/cloudflare/mcp-server-cloudflare
- **Package:** none to bundle. The npm package `@cloudflare/mcp-server-cloudflare` is stale at `0.2.0` (published March 2025, built on MCP SDK ^0.6.0) - do NOT pin/use it. One app's CONTRIBUTING explicitly marks its local server deprecated in favor of the remote endpoints.
- **Transport:** **Remote, streamable HTTP** (hosted by Cloudflare). Code Mode (recommended, broad API coverage): `https://mcp.cloudflare.com/mcp` (maintained in repo `cloudflare/mcp`). Domain-specific servers (`*.mcp.cloudflare.com/mcp`): `docs`, `bindings`, `builds`, `observability`, `containers`, `browser`, `logs`, `ai-gateway`, `autorag`, `dns-analytics`, `dex`, `casb`, `radar`, `blog`. All expose `/mcp` (and `/sse` as a compatibility alias; legacy `GET /sse` returns `410 Gone` - use `/mcp`).
- stdio shim for clients without remote support: `command: "npx", args: ["mcp-remote", "https://mcp.cloudflare.com/mcp"]`.
- **Secrets:** none static - browser OAuth flow on first connect; for OpenAI Responses API use, a Cloudflare API token with the scopes that MCP server needs.
- **Setup notes:** some features require a paid Workers plan. Auth is per-user OAuth; account selection happens in the flow.
- **Local-only vs cloud:** Remote/cloud only. This is NOT a locally-run server.
- **Deprecation warnings:** Yes - local server implementations in this repo are deprecated; maintainers direct all new use to `mcp.cloudflare.com/mcp` (Code Mode) and the `*.mcp.cloudflare.com` remote endpoints.

## 5. Playwright - `@playwright/mcp`

- **Doc URL:** https://github.com/microsoft/playwright-mcp (37,700 stars, Apache-2.0)
- **Package:** npm `@playwright/mcp`
- **PINNED:** `@playwright/mcp@0.0.83` (latest npm, observed 2026-09-29)
- **Transport:** stdio. Exact command/args: `npx @playwright/mcp@0.0.83` (docs also use `npx -y @playwright/mcp@0.0.83`). A standalone HTTP/SSE server mode also exists (see README "Standalone MCP server" section).
- **Secrets:** none required.
- **Setup notes (non-obvious):**
- Requires Node.js 18+. Browsers (Chromium by default) are downloaded by Playwright on first run.
- `--browser chrome|firefox|webkit|msedge`; `--caps vision,pdf,devtools`; `--cdp-endpoint <url>` to drive an external browser; `--user-data-dir` for persistent profiles.
- Filesystem access is restricted to workspace roots by default (`--allow-unrestricted-file-access` relaxes); host/origin allow-lists via `--allowed-hosts`/`--allowed-origins` (env: `PLAYWRIGHT_MCP_*` equivalents for every flag).
- Maintainer guidance: for high-throughput coding agents they recommend their CLI+SKILLs (more token-efficient); MCP is for exploratory/persistent browser loops.
- **Local-only vs cloud:** Fully local.
- **Deprecation warnings:** none.

## 6. Notion - `@notionhq/notion-mcp-server`

- **Doc URL:** https://github.com/makenotion/notion-mcp-server (4,651 stars, MIT)
- **Package:** npm `@notionhq/notion-mcp-server`; Docker Hub image `mcp/notion`
- **PINNED:** `@notionhq/notion-mcp-server@2.5.2` (latest npm, observed 2026-09-29)
- **Transport:** stdio (default). Exact command/args: `npx -y @notionhq/notion-mcp-server` (explicit: `--transport stdio`). Optional streamable HTTP: `--transport http` → `http://127.0.0.1:<port>/mcp` (default port 3000; `--port`, `--host` flags; bearer auth required).
- **Secrets (env var names only):**
- `NOTION_TOKEN` - internal integration token (recommended)
- `OPENAPI_MCP_HEADERS` - JSON `{"Authorization": "Bearer <token>", "Notion-Version": "2025-09-03"}` (advanced)
- `AUTH_TOKEN` (env) / `--auth-token` (flag) - bearer token for the HTTP transport only
- **Setup notes (non-obvious):**
- Create an internal integration at `https://www.notion.so/profile/integrations`, then explicitly share pages/databases with it (integration Access tab, or per-page "Connect to integration") - unshared content is invisible to the server.
- **v2.0.0 breaking change:** Notion API 2025-09-03; database tools renamed to data-source tools (`post-database-query` → `query-data-source`, `update-a-database` → `update-a-data-source`, `create-a-database` → `create-a-data-source`); 22 tools total.
- **Local-only vs cloud:** Local server; calls the Notion cloud API.
- **Deprecation warnings:** YES - maintainers state this repo is "no longer actively maintained or supported" and "may sunset this local MCP server repository in the future"; issues/PRs not monitored. They direct users to hosted **Remote Notion MCP** (OAuth, semantic search, respects per-user permissions). Bundle this at 2.5.2 with eyes open, or plan a remote-Notion path.
