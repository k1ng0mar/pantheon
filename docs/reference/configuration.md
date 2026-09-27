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

One `[judge]`, `[compression]`, `[title_gen]`, `[embeddings]`, `[search_synthesis]`, `[vision]`, `[scheduled]`, `[mcp_synthesis]` section each, same shape (`provider`, `model`, optional `api_key_env`). Absent means `auto` — the run's default model — except `[embeddings]`, which falls back to a local embedder. Env overrides: `PANTHEON_<AUX>_PROVIDER` / `PANTHEON_<AUX>_MODEL`.

## Policy

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
