# Configuration

`config.toml` lives in your data directory (`~/.pantheon` by default). `pantheon setup` writes it; everything reads it. One rule: **secrets never go in this file.** The config names environment variables; Pantheon reads the actual values from the environment when it needs them.

## Which AI to use

```toml
[model]
provider    = "openai"
model       = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"   # the NAME of the env var, never the key itself
reasoning   = "high"             # optional: how hard it thinks (off|minimal|low|medium|high|xhigh|max)
```

Environment variables that override this section: `PANTHEON_PROVIDER`, `PANTHEON_MODEL`, `PANTHEON_REASONING`, `PANTHEON_REASONING_BUDGET`.

Backups, tried in order when the main model fails:

```toml
[[model.fallbacks]]
provider = "anthropic"
model    = "claude-sonnet-4-5"
```

The `reasoning` setting is translated per provider (thinking budget on Anthropic, reasoning effort on OpenAI). An exact `reasoning_budget` number overrides the translation; `0` turns it off. Unknown spellings are treated as off, and `doctor` flags them.

## Helper models

Small models for specific background jobs. Each section has the same shape (`[compression]` additionally takes `target_percent`, documented below). Available sections: `[judge]`, `[compression]`, `[title_gen]`, `[embeddings]`, `[search_synthesis]`, `[vision]`, `[scheduled]`, `[mcp_synthesis]`, `[extraction]`, `[rerank]`, `[planner]`, `[repair]`, `[verify]`, `[reflect]`, `[consolidation]`.

Each section inherits from your main `[model]` unless you say otherwise:

```toml
[vision]
provider = "default"   # use the main model's provider (or just leave provider out)
model = ""             # empty = use the main model's model
timeout = 120          # optional: per-job timeout in seconds

[extraction]
provider = "nous"
model = "openai/gpt-6-astra"
timeout = 360
api_key_env = "NOUS_API_KEY"   # only needed when the provider isn't "default"
```

Rules:
- `provider = "default"` (or omitted) inherits the `[model]` provider.
- An empty or omitted `model` inherits the `[model]` model.
- `api_key_env` is only needed for a non-default provider. A default-inheriting provider inherits `[model].api_key_env`.
- `timeout` is optional per section. When omitted, each job keeps its built-in default (10s for title/judge, 15s embeddings, 30s compression and verify, 60s reflection/consolidation/repair, 120s everything else).

Leave a section out entirely and it uses your main model. `[embeddings]` defaults to a local embedder instead. Environment overrides look like `PANTHEON_RERANK_MODEL` and beat the config file.

`[compression]` takes one extra knob the other sections don't have:

```toml
[compression]
model = "gpt-4o-mini"
target_percent = 50   # summary size as % of the absorbed transcript chars (1-100, default 12)
```

Higher keeps more detail (less aggressive), lower compresses harder. `0` and values over `100` are config errors. The default `12` preserves the historic summary budget.

Note: `[extraction]`, `[rerank]`, `[planner]`, `[vision]`, `[search_synthesis]`, `[mcp_synthesis]`, and `[judge]` have no features using them yet. They exist so you can pin models ahead of time. (`[judge]` is validated by `doctor` but the production agent loop is built with `judge: None`, so it currently changes nothing - the engine supports it, installation into the runtime loop is still open.) Everything else is wired: `[compression]`/`[title_gen]` run in the session, `[embeddings]` powers semantic recall, `[scheduled]` sets the model for background runs, `[verify]` checks delegated results, and `[reflect]`/`[consolidation]`/`[repair]` serve the nightly pass (`[repair]` is the only slot the fix loop uses to revise drafts; `[reflect]` still handles pre-pass proposal refinement and replay transcript scoring).

`[verify]` is the exception to the "leave a section out and it uses your main model" rule: **absent `[verify]` means no verification happens at all** (off by default, no `auto` entry). Pin a section and every delegated subagent's claimed result is adversarially checked by that model before the parent accepts it:

```toml
[verify]
provider = "anthropic"
model = "claude-3-5-haiku-latest"
timeout = 30
```

The verifier starts from the assumption the child did *not* achieve its goal and answers `HOLDS`, `FALSIFIED`, or `INCONCLUSIVE` with a confidence. A `FALSIFIED` verdict fails the delegation outright (`SWARM_CHILD_FALSIFIED`); `INCONCLUSIVE` or a verifier transport failure marks the result unverified - never treated as done. Keep this on a cheap, fast model: it runs once per delegation, read-only, with no tools.

## Self-improvement

```toml
[nightly]
# enabled = true     # master switch; absent = on iff a [nightly.model] pin exists, else off (default: off)
# auto_turns = 20    # automatic pass every N completed turns; 0 disables the trigger
# min_sessions = 3   # sessions in the window needed to promote a memory
# max_age_days = 30  # only look back this far
# cron = "0 3 * * *" # default when scheduled via `pantheon schedule nightly`
# replay_command = "" # external replay driver for tasks without their own exec spec

[nightly.model]       # the pass's own model pin; presence = enabled (unless enabled = false)
provider = "openai"
model = "gpt-4o-mini"
# api_key_env = "OPENAI_API_KEY"  # only needed for a non-default provider; seeds PANTHEON_NIGHTLY_API_KEY
# timeout = 60        # per-request timeout in seconds
```

The nightly pass is **off by default** and does nothing unless enabled - one rule, four paths:

1. a `[nightly.model]` pin (absent `enabled` flag) - presence of the pin = enabled;
2. `/nightly on` in the TUI (writes `enabled = true`);
3. a config edit (`enabled = true` under `[nightly]`);
4. the dashboard / mobile-app toggle.

`enabled = false` always wins, even with a model pin. `PANTHEON_NIGHTLY_PROVIDER` / `PANTHEON_NIGHTLY_MODEL` env overrides count as a pin, like every other aux slot. `pantheon doctor` reports the on/off state and why; `/nightly status` shows it too.

The dashboard / mobile-app toggle is two HTTP endpoints on the dashboard control plane (the mobile app uses the same API):

- `GET /api/nightly/status` - the resolved state: `enabled` (the single enable rule), `reason` (explicit flag on|off, on via `[nightly.model]` pin, on via `PANTHEON_NIGHTLY_*` env, off), the raw `explicit` flag, `model_pin`, `next_run_ms` (next `pantheon schedule nightly` fire, if any), and the last pass summary.
- `POST /api/nightly/enabled` - `{enabled: bool, confirm: true}`. Writes the explicit `[nightly] enabled` flag through the shared config document (load → mutate → save) - the same flag `/nightly on|off` and a manual config edit write; there is no parallel store. Without `confirm: true` the request is rejected. The response is the fresh status document, so the client renders the toggle without a second read.

When enabled, Pantheon reviews its history in a single pass, proposes improvements (memory lessons, skills, persona notes) with ledger provenance, and gates them: memory lessons recur across N sessions before auto-applying; skills/personas must pass no-regression evals and strictly improve held-out replay before asking for your approval. Memory lessons apply automatically once confirmed; everything else waits for your yes or no (`/nightly`, or `pantheon nightly --approve <id>`). The pass's own LLM steps resolve through the `[nightly.model]` pin when one is configured, else through the Reflection/Consolidation aux slots - never the chat model directly.

The old `[reflect]` / `[consolidation]` sections are **deprecated** but still parse (old configs keep loading). `[nightly]` is the authoritative section: when it is present the legacy sections are ignored entirely for the nightly pass; when it is absent they are honored field-by-field as a migration fallback. `pantheon doctor` nudges you to move the behavior knobs across - `enabled` and `auto_turns` from `[reflect]`, `enabled`, `min_sessions`, and `cron` from `[consolidation]` - into `[nightly]`. The legacy aux-model pins (`provider`/`model`/`api_key_env`/`timeout`) stay where they are; the pass's own pin lives in `[nightly.model]`. `half_life_days` and `min_score` are ignored: the decay curve was replaced by recurrence across sessions.

Replay tasks are headless and built in: define a task with
`pantheon nightly replay-tasks add --exec-cmd ./scripts/run-task.sh --exec-arg foo --exec-env KEY=VALUE`.
Pantheon runs `<command> <args...> <prompt>` in a fresh temporary working directory, captures stdout as the transcript, and enforces a timeout; the candidate skill/persona is visible to the task through `PANTHEON_REPLAY_SKILL_DIR`. The temporary directory only controls where the task starts - it is **not** a security sandbox, so treat task specs as trusted process execution: only attach exec specs to commands you would run yourself. Tasks without an exec spec fall back to `[nightly] replay_command`; with neither configured, replays fail loudly and the replay gate rejects every proposal - strict improvement cannot be measured without a runner.

A failing proposal goes through a bounded fix loop (default 3 attempts, `max_fix_attempts`): eval failures revise the draft via the Reflection auxiliary slot (LLM steps enabled) and re-run the full eval tag set - eval tags are immutable in the loop, and zero-tagged eval gating is reported as skipped, never as a vacuous pass; fair replay failures retry the A/B and, with LLM steps enabled, sharpen the draft via the Reflection auxiliary slot; infrastructure failures (runner errors, non-finite scores, missing tasks) escalate immediately. After the bound, the proposal is marked `NeedsAttention` and recorded in `<data_dir>/nightly/nightly-escalated.json` - never queued, never applied.

Approved persona notes are written to the `persona` memory namespace and injected into every session's system prompt as a `<nightly_persona>` block. Unapproved persona notes live only in the approval queue and are never injected.

Commands: `/nightly` (run one pass now), `/nightly on|off` (toggle), `/nightly status`. From the shell: `pantheon nightly [--dry-run] [on|off|status|log|pending] [--approve ID] [--deny ID]`. To run it on a schedule: `pantheon schedule nightly --cron '0 3 * * *'`.

## Permissions

`policy = "reader" | "coder" | "coder_memory"`.

- `reader`: can look, cannot change anything.
- `coder`: can read and write files, run commands.
- `coder_memory`: like `coder`, plus it may write memories.

Every tool says what permission it needs; the policy answers allow, deny, or ask you.

## Limits

```toml
[budget]
max_turns          = 16      # back-and-forth exchanges per run
max_tool_calls     = 32      # tool uses per run
max_delegate_depth = 2       # how deep agents can delegate to other agents
max_iterations     = 3       # pipeline iterations
# max_tokens       = 50000   # optional per-response OUTPUT cap; unset = no config cap

[goal]
max_iterations     = 10      # exchanges allowed per /goal before it stops and asks
```

Every key is optional; `0` counts as unset. These are the defaults for a session. `/set <key> <value>` changes them live for the current session, and `/tokens [n|off]` manages the per-response output cap on its own. There is no spending cap: cost is tracked for statistics only.

## Time awareness

```toml
[temporal]
enabled            = true    # on by default, costs nothing
min_gap_secs       = 7200    # remind the model after this much idle time (0 = off)
notify_date_change = true    # mention when the date changed overnight
# timezone         = "Africa/Lagos"  # IANA name; unset = your system timezone
```

Pantheon quietly notices when a conversation has aged, and tells the model something like "about 5 hours" or "the previous exchange was yesterday". This hint is never saved into the conversation history; it just helps the model not act like no time passed. The wording stays coarse on purpose.

## Memory

```toml
[memory]
backend = "native"
```

Which memory store to use. See [Memory](../user-guide/memory.md).

## Secrets

```toml
[secrets]
env_allowlist = ["MY_API_KEY", "PANTHEON_*"]
plugin_env_allowlist = ["MY_PLUGIN_TOKEN"]
```

Which environment variables the assistant is allowed to read, and which ones plugins may see. Both lists are empty by default, meaning nothing is shared unless you say so:

- `env_allowlist`: variables readable through the `env:` name form. Entries are exact names or `PREFIX_*` wildcards; `"*"` alone allows everything (an explicit opt-out of the protection).
- `plugin_env_allowlist`: variables that may be passed into plugin code. A plugin asking for a variable by name is never enough on its own; it must also be listed here.

## Agents

```toml
agent = "zeus"   # which agent this install runs as; must name a table below

[agents.default]
display_name = "Default"
agents_file  = "agents/default/AGENTS.md"   # its instructions
soul_file    = "agents/default/SOUL.md"     # its personality
policy       = "coder_memory"

[agents.zeus]
inherits     = "default"                    # copies default's settings, then overrides
display_name = "Zeus"
soul_file    = "agents/zeus/SOUL.md"
```

`agent = "zeus"` picks which agent this install runs as (it must name a table above). No `[agents]` table means anonymous runs. See [Agents](../user-guide/agents.md).

> **Removed keys.** Older configs may carry a top-level `profile = "..."` label or a
> `[tools]` table. Both were inert - `profile` never selected an agent (that is
> what `agent` does), and `[tools]` never gated tool registration (registration
> is unconditional). They were removed; old files still load, the keys are
> simply ignored. There is no replacement to configure.

## Custom providers

```toml
[custom_providers.my-llm]
base_url = "http://127.0.0.1:8015/v1"
api_mode = "openai"          # or "anthropic"
key_env  = "PANTHEON_KEY_MY_LLM"
```

Written for you by `pantheon provider add` / `pantheon model`. Only model names you typed by hand are kept here.

## Server and speech

```toml
[server]
port = 18789
host = "127.0.0.1"
```

`[stt]` / `[tts]` pick speech-to-text and text-to-speech backends. These are
provider-plane services, never model-policy entries: absent = the surface
simply has no speech capability.

```toml
[stt]
backend = "openai"          # or "command"

[stt.options]
provider    = "openai"      # catalog id or base URL
model       = "whisper-1"
api_key_env = "OPENAI_API_KEY"  # optional: env var holding the key

[tts]
backend = "command"

[tts.options]
cmd          = "espeak-ng"
args         = "--stdout -v {voice}"  # {voice}, {format} placeholders
timeout_secs = "120"
```

- `command`: a local binary. STT reads the transcript from stdout
  (`{file}`, `{language}` placeholders); TTS takes text on stdin and audio
  on stdout (`{voice}`, `{format}`). Every run is wall-clock bounded.
- `openai`: OpenAI-compatible HTTP (`/audio/transcriptions`,
  `/audio/speech`). The key resolves through the secrets broker:
  `api_key_env` names the env var explicitly, otherwise the catalog row's
  key env (e.g. `OPENAI_API_KEY`), otherwise `PANTHEON_KEY_<PROVIDER>`.
  No key anywhere is fine for keyless local endpoints.

Check status with `/voice` in the TUI. There is no microphone capture or
speaker playback in the TUI - these backends move bytes for callers that
already have audio (files, other surfaces), they don't record or play it.

## Gateway

Chat app tokens live in `<data_dir>/.env`, never in this file: `PANTHEON_DISCORD_TOKEN`, `PANTHEON_TELEGRAM_BOT_TOKEN`, plus the required `PANTHEON_GATEWAY_ALLOW` (who may talk to it). See [Channels](../user-guide/channels.md).

## Web access

Two separate systems, on purpose. `web_search` *looks things up* (facts, news, docs, prices); the `browser_*` tools *do things on a live site* (navigate, click, fill forms, extract from JS-heavy pages). The model picks by intent: "what is X" → search, "do Y on site Z" → browser.

```toml
[websearch]
enabled = true
provider = "tinyfish"        # one of: tinyfish (recommended), tavily, ollama, exa, marginalia, brave, firecrawl, searxng, perplexity
# api_key_secret = "TINYFISH_API_KEY"  # optional override; absent = the provider's default secret name
# base_url = "http://localhost:8080"  # endpoint override for self-hosted providers (SearXNG)
max_results = 8
```

The `web_search` tool only registers when the provider's auth requirement is met - keyed providers need their key resolvable (default secret names: `TINYFISH_API_KEY`, `TAVILY_API_KEY`, `OLLAMA_API_KEY`, `EXA_API_KEY`, `BRAVE_API_KEY`, `FIRECRAWL_API_KEY`, `PERPLEXITY_API_KEY`); Marginalia is keyless and SearXNG needs a self-hosted instance URL. Without that, the tool stays out of the model's tool list. Store keys with `pantheon secrets set <NAME>` (or in `<data_dir>/.env`).

```toml
[browser]
enabled = true
# binary = "/usr/local/bin/gsd-browser"  # absent = resolved from PATH
act_require_approval = true  # browser_act parks for human approval
idle_timeout_secs = 900      # stop the run's browser daemon when idle
vault_key_secret = "GSD_BROWSER_VAULT_KEY"  # auth-vault key via secrets
```

Browser automation shells out to the [gsd-browser](https://github.com/gsd-build/gsd-browser) binary (install: `curl -fsSL https://install.gsd.build/browser | bash`). One browser daemon session maps to one Pantheon run; it starts lazily on the first `browser_*` call and stops when the run ends or after `idle_timeout_secs` of disuse. `browser_act` clicks the top semantic-intent candidate with no upstream confidence threshold, so it carries the `browser.act` capability and the run parks for your approval before it runs - leave `act_require_approval = true` unless you trust autonomous clicks.

## MCP servers

MCP servers are third-party tools Pantheon launches (or connects to) and projects into the agent's tool registry as `mcp_<server>_<tool>`. Declare them in config.toml; servers imported by `pantheon migrate apply` also appear as declarations under `<data_dir>/mcp/*.json`, which fill in names the config section does not define.

```toml
[mcp]
enabled = true   # master switch; absent section = off

[mcp.servers.github]
transport = "http"                       # stdio | sse | http (streamable HTTP); default stdio
url = "http://localhost:8080/mcp"
# env refs below resolve at launch time: "env:NAME" reads the secret/vault
# first, then the process environment. Values are never logged.

[mcp.servers.github.env]
GITHUB_TOKEN = "env:GITHUB_TOKEN"

[mcp.servers.codebase-memory]            # a codebase index is just a server, not special-cased
transport = "stdio"
command = "codebase-mcp"
args = ["--repo", "/home/umar/src/pantheon"]
timeout_secs = 30                        # per-request timeout; default 30

[mcp.servers.codebase-memory.env]
INDEX_DIR = "/home/umar/.cache/codebase-index"
```

Transports: `stdio` spawns `command` with `args` (the child inherits only `PATH` plus the declared env); `sse` opens a Server-Sent Events stream and posts messages to the discovered endpoint; `http` speaks streamable HTTP (POST, with SSE fallback). Remote transports reconnect with backoff; a server that keeps failing is left alone until its backoff expires.

**Approvals.** A server never runs without your explicit approval: `pantheon mcp approve <name>` prints exactly what would launch (or where it would connect), shows the warning, and asks you to confirm by typing the server name. The approval binds the server's name, its reported version, and a content hash of the binary/script (or the endpoint URL) - any upgrade or change invalidates it and you are asked again. `pantheon mcp list` shows what's declared and whether it's approved; `pantheon mcp status` and `pantheon mcp health <name>` report live state from the launcher's `<data_dir>/mcp/live.json` snapshot; the dashboard's MCP page shows the same merged view. `PANTHEON_MCP_ENABLED=0` forces the launcher off.

## Bundled plugins

Pantheon's first-party plugins (today: `time-gap`, a hook plugin) share one enablement state with the dashboard and the mobile app: the config file. All bundled plugins are disabled by default.

```toml
[plugins.time-gap]
enabled = true   # kind and version are stamped automatically when enabled
```

For a bundled plugin this entry wins over the plugin manifest's own `enabled` flag. Third-party plugins are unaffected: their gate is the approval store (`pantheon extensions approve`), not this table, and entries for names outside the bundled catalog are inert. The agent can propose enabling a bundled plugin through its `enable_plugin` tool, which parks for your approval like any other privileged action - never silent, and only catalog plugins are eligible.

## Data directory

`$PANTHEON_DATA_DIR` or `~/.pantheon/`. Everything lives here, so backing up this folder backs up everything:

| Path | What it is |
|---|---|
| `config.toml` | This file (setup writes it, doctor checks it) |
| `.env` | Your keys (`pantheon model` writes here), readable only by you |
| `ledger.db` | The record of everything that happened |
| `memory.db` | What it remembers |
| `memory-backend.toml` | Chosen memory backend |
| `extensions/` | Installed plugins |
| `skills/` | Imported skills |
| `agents/` | Agent personality files (`AGENTS.md`, `SOUL.md`) |
| `gateway/` | Chat app state and outgoing messages |
| `logs/` | `agent.log`, `errors.log`, `gateway.log` |
| `safewrite/` | File-edit checkpoints, for undoing changes |
