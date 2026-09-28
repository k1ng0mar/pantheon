# Configuration

`config.toml` in the data dir is the single source of truth for persistent configuration. Setup writes it, the runtime reads it, every verb uses it. Secrets are never in it — config names env vars, the runtime resolves them at the execution boundary.

## Model

```toml
[model]
provider    = "openai"
model       = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"   # name only, never the key
reasoning   = "high"             # optional: off|minimal|low|medium|high|xhigh|max

[[model.fallbacks]]
provider = "anthropic"
model    = "claude-sonnet-4-5"
```

Env overrides: `PANTHEON_PROVIDER`, `PANTHEON_MODEL`, `PANTHEON_REASONING`, `PANTHEON_REASONING_BUDGET`. Fallbacks are ordered, failure-only, runtime-controlled. Reasoning maps per wire mode (`reasoning_effort`, including `minimal` and `xhigh`, on OpenAI; thinking budget on Anthropic: 1k/4k/10k/20k/32k for minimal/low/medium/high/xhigh, `max` fills the window). An exact `reasoning_budget` overrides the mapping on budget wires (`0` disables); aux turns always run without it. Unknown spellings resolve to off and `doctor` flags them.

## Auxiliaries

One `[judge]`, `[compression]`, `[title_gen]`, `[embeddings]`, `[search_synthesis]`, `[vision]`, `[scheduled]`, `[mcp_synthesis]`, `[extraction]`, `[rerank]`, `[planner]` section each, same shape (`provider`, `model`, optional `api_key_env`). Absent means `auto` — the run's default model — except `[embeddings]`, which falls back to a local embedder. Env overrides: `PANTHEON_<AUX>_PROVIDER` / `PANTHEON_<AUX>_MODEL` (e.g. `PANTHEON_RERANK_MODEL`). `[extraction]`, `[rerank]`, and `[planner]` have no call sites yet — they exist so you can pin a cheap model ahead of those workloads landing.

## Reflection

```toml
[reflect]
enabled       = false   # LLM-backed reflection steps need explicit opt-in
auto_turns    = 20      # automatic pass every N completed turns (0 = off)
max_proposals = 5       # proposal cap per pass
# provider    = "openai"   # optional: pin the reflection aux model (auto = default)
# model       = "gpt-4o-mini"
# api_key_env = "OPENAI_API_KEY"
```

Reflection is Pantheon's ledger-native self-improvement loop: each pass reads structured ledger signals (repeated tool sequences, user corrections, denied approvals, repeated failures), generates proposals with provenance (memory lessons, skill proposals, persona notes), eval-gates skill/persona proposals against bounded evals, and holds them for approval. Memory lessons auto-apply at the `Memory` trust tier; everything else needs `/reflect`'s y/n card (or `pantheon reflect --approve <id>`). Every LLM call the pipeline makes resolves through the `Reflection` auxiliary slot — never the chat model — so pin a small model here to keep background self-improvement cheap.

- `/reflect` — manual one-shot pass (background); `/reflect on|off` toggles the loop (persisted here); `/reflect status` shows the toggle plus the last pass summary.
- `pantheon reflect [--dry-run] [on|off|status|log|pending] [--approve ID] [--deny ID]`
- `pantheon schedule reflect --cron '0 2 * * *'` — nightly passes via the normal scheduler.

## Policy

`policy = "reader" | "coder" | "coder_memory"`. Tools declare the capability they need; the policy decides allow/deny/approve per operation. `coder_memory` adds the `MemoryWrite` capability.

## Budgets

```toml
[budget]
max_turns          = 16      # agent turns per run
max_tool_calls     = 32      # tool calls per run
max_delegate_depth = 2       # how deep /swarm delegation may nest
max_iterations     = 3       # pipeline iterations (pantheon pipeline)
# max_tokens       = 50000   # per-run token cap — strictly optional, absent = uncapped

[goal]
max_iterations     = 10      # turns allowed per /goal before the TUI stops and asks
```

Every key is optional; a `0` is treated as unset. These are the session defaults — `/set <key> <value>` retunes them live for the current session (`max_turns`, `max_tool_calls`, `max_delegate_depth`, `max_tokens`; `0` clears the token cap), and `/tokens [n|off]` manages the token cap on its own. There is no cost cap: cost is tracked for `pantheon stats` / `/stats` only.

## Temporal awareness

```toml
[temporal]
enabled            = true    # master switch (default on — zero tokens, pure string injection)
min_gap_secs       = 7200    # idle seconds before an elapsed-gap hint fires (0 = off)
notify_date_change = true    # hint when the local date rolled over, even on a short gap
# timezone         = "Africa/Lagos"  # IANA name; absent = system local timezone
```

Tacit temporal awareness: the model notices when a conversation has meaningfully aged, without timestamping every message. Before a turn's first model call the pipeline reads the last assistant turn's timestamp from the durable ledger (restart-safe) and, when the gap matters, appends one coarse hint to the outgoing user message — for the API call only, never written to the ledger or transcript, and never on the system prompt (prompt caching unaffected). Wording is coarse and gets coarser with the gap: `about 40 minutes`, `about 5 hours`, `about a day`, `about 3 days`. A date rollover across a short gap yields `[temporal: the previous exchange was yesterday]`; multi-day gaps already imply the date change, so wordings never stack. The conversation's standing system preamble tells the model to factor such hints in and never quote them.

`policy = "reader" | "coder" | "coder_memory"`. Tools declare the capability they need; the policy decides allow/deny/approve per operation. `coder_memory` adds the `MemoryWrite` capability.

## Memory

```toml
[memory]
backend = "native"
```

Backend selection is mirrored in `memory-backend.toml`. See [Memory](../user-guide/memory.md).

## Secrets

```toml
[secrets]
env_allowlist = ["MY_API_KEY", "PANTHEON_*"]
plugin_env_allowlist = ["MY_PLUGIN_TOKEN"]
```

The run's secrets-boundary policy. Both lists are empty by default (fail closed):

- `env_allowlist`: env vars readable through the `env:` secret-name form. Entries are exact names or `PREFIX_*` wildcards; `"*"` alone allows all (explicit opt-out). Without an entry, `env:` lookups resolve nothing — secrets must come from `PANTHEON_SECRET_*` or a durable vault, so a name like `env:AWS_SECRET_ACCESS_KEY` can never exfiltrate an arbitrary host variable.
- `plugin_env_allowlist`: manifest-declared env vars the plugin supervisor may copy from the host into plugin subprocesses (same entry syntax). A project-controlled manifest can declare any name it likes, so a declared name alone never crosses the boundary — only an entry here lets a host var reach plugin code. Extension (Python/JS) hook subprocesses always run with a cleared environment (PATH only) regardless of this list.

## Agents

```toml
profile = "default"   # informational label only; does NOT select an agent

[agents.default]
display_name = "Default"
agents_file  = "agents/default/AGENTS.md"
soul_file    = "agents/default/SOUL.md"
policy       = "coder_memory"

[agents.zeus]
inherits     = "default"
display_name = "Zeus"
soul_file    = "agents/zeus/SOUL.md"
```

`agent = "zeus"` selects the profile this install runs as (must name a declared table). No `[agents]` table means anonymous runs. See [Agents](../user-guide/agents.md).

## Custom providers

```toml
[custom_providers.my-llm]
base_url = "http://127.0.0.1:8015/v1"
api_mode = "openai"          # or "anthropic"
key_env  = "PANTHEON_KEY_MY_LLM"
```

Written by `pantheon provider add` / `pantheon model`. `models` sub-rows hold only model ids you named by hand.

## Server and speech

```toml
[server]
port = 18789
host = "127.0.0.1"
```

`[stt]` / `[tts]` select speech backends (`command` with `cmd`, or `openai` with `provider`).

## Gateway

Tokens live in `<data_dir>/.env`, never in config: `PANTHEON_DISCORD_TOKEN`, `PANTHEON_TELEGRAM_BOT_TOKEN`, plus required `PANTHEON_GATEWAY_ALLOW`. See [Channels](../user-guide/channels.md).

## Data directory

`$PANTHEON_DATA_DIR` or `~/.pantheon/`:

| Path | What it is |
|---|---|
| `config.toml` | This file (setup writes it, doctor validates it) |
| `.env` | Key store (`pantheon model` writes, imports merge here), `0600` |
| `ledger.db` | Event ledger, operations, leases, artifacts |
| `memory.db` | Five-layer memory store |
| `memory-backend.toml` | Selected memory backend |
| `extensions/` | Loaded plugins |
| `skills/` | Imported skills |
| `agents/` | Agent identity files (`AGENTS.md`, `SOUL.md`) |
| `gateway/` | Channel cursors and outbox |
| `logs/` | `agent.log`, `errors.log`, `gateway.log` |
| `safewrite/` | File-edit checkpoints and write journal |
