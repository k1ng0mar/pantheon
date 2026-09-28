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

Small models for specific background jobs. Each section has the same shape: `provider`, `model`, optional `api_key_env`. Available sections: `[judge]`, `[compression]`, `[title_gen]`, `[embeddings]`, `[search_synthesis]`, `[vision]`, `[scheduled]`, `[mcp_synthesis]`, `[extraction]`, `[rerank]`, `[planner]`.

Leave one out and it uses your main model. `[embeddings]` defaults to a local embedder instead. Environment overrides look like `PANTHEON_RERANK_MODEL`.

Note: `[extraction]`, `[rerank]`, and `[planner]` have no features using them yet. They exist so you can pin cheap models ahead of time.

## Self-improvement

```toml
[reflect]
enabled       = false   # opt in explicitly; off by default
auto_turns    = 20      # automatic review every N finished conversations (0 = off)
max_proposals = 5       # how many suggestions per review
# provider    = "openai"   # optional: pin a cheap model for reviews
# model       = "gpt-4o-mini"
# api_key_env = "OPENAI_API_KEY"
```

When enabled, Pantheon periodically reviews its own history, spots patterns (repeated mistakes, your corrections, denied permissions), and suggests improvements: lessons for memory, new skills, notes about its personality. Memory lessons apply automatically once confirmed; everything else waits for your yes or no (`/reflect`, or `pantheon reflect --approve <id>`). Reviews always use the pinned model, never your main chat model, so keep it cheap.

Commands: `/reflect` (run one review now), `/reflect on|off` (toggle), `/reflect status`. From the shell: `pantheon reflect [--dry-run] [on|off|status|log|pending] [--approve ID] [--deny ID]`. To run it nightly: `pantheon schedule reflect --cron '0 2 * * *'`.

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
# max_tokens       = 50000   # optional cap on tokens per run; unset = no cap

[goal]
max_iterations     = 10      # exchanges allowed per /goal before it stops and asks
```

Every key is optional; `0` counts as unset. These are the defaults for a session. `/set <key> <value>` changes them live for the current session, and `/tokens [n|off]` manages the token cap on its own. There is no spending cap: cost is tracked for statistics only.

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
profile = "default"   # just a label; does NOT pick the agent

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

`[stt]` / `[tts]` pick speech-to-text and text-to-speech backends (`command` with a `cmd`, or `openai` with a `provider`).

## Gateway

Chat app tokens live in `<data_dir>/.env`, never in this file: `PANTHEON_DISCORD_TOKEN`, `PANTHEON_TELEGRAM_BOT_TOKEN`, plus the required `PANTHEON_GATEWAY_ALLOW` (who may talk to it). See [Channels](../user-guide/channels.md).

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
