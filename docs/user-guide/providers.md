# Providers

Your agent never picks its own model. You set a default, a backup list for when the first one fails, and optionally small specialist models for specific jobs. Swap any of them without touching the agent.

## Default model

```toml
[model]
provider    = "openai"
model       = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"
reasoning   = "high"   # optional: off|minimal|low|medium|high|xhigh|max
```

`pantheon model` shows the built-in catalog (39+ models) and saves keys into `<data_dir>/.env`. The config file holds names, never the keys themselves. `pantheon providers` lists everything available. Switch mid-conversation with `/model <provider> <model>`; your conversation and memory carry over.

## Backups and helpers

```toml
[[model.fallbacks]]
provider = "anthropic"
model    = "claude-sonnet-4-5"
```

Backups kick in only when the main model fails, in order. You can also pin small models for specific jobs (`[judge]`, `[compression]`, `[title_gen]`, `[embeddings]`, `[search_synthesis]`, `[vision]`, `[scheduled]`, `[mcp_synthesis]`). Leave one unset and it just uses the main model, except embeddings, which default to a local one.

## Custom endpoints

Any OpenAI-compatible URL works as a provider:

```sh
pantheon provider add --name my-llm --base-url http://127.0.0.1:8015/v1
pantheon provider models my-llm
```

Only model names you type by hand are saved in the config. Live model lists are fetched fresh each time, never baked in.

## Reliability

If a provider says "slow down" (429), Pantheon waits as long as asked (up to a minute) and retries. Errors stay structured, so scripts can tell a rate limit from a bad key. Asking for structured JSON output works on both the OpenAI and Anthropic connections.

## See also

- [Agents](agents.md): per-agent model settings
- [Configuration](../reference/configuration.md): every provider field
