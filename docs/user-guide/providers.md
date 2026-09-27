# Providers

Agents don't pick models. The runtime resolves a default, a failure-only fallback chain, and per-capability auxiliaries — swap anything without touching the agent.

## Default model

```toml
[model]
provider    = "openai"
model       = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"
reasoning   = "high"   # optional: off|minimal|low|medium|high|xhigh|max (default off)
```

`pantheon model` picks from 39+ builtins and writes keys to `<data_dir>/.env` (config holds names, never values). `pantheon providers` lists the catalog. Switch mid-conversation with `/model <provider> <model>` — identity and context carry over.

## Fallbacks and helpers

```toml
[[model.fallbacks]]
provider = "anthropic"
model    = "claude-sonnet-4-5"
```

Fallbacks fire on failure only, in order, under runtime control. Helper slots (`[judge]`, `[compression]`, `[title_gen]`, `[embeddings]`, `[search_synthesis]`, `[vision]`, `[scheduled]`, `[mcp_synthesis]`) pin small models per job; unconfigured means `auto` — the run's own model — except embeddings, which default to a local embedder.

## Custom endpoints

Any OpenAI-compatible URL is a provider:

```sh
pantheon provider add --name my-llm --base-url http://127.0.0.1:8015/v1
pantheon provider models my-llm
```

Only model ids you name by hand are recorded in config. Live endpoint listings are fetched on demand, never baked in — a third party's list goes stale, and config shouldn't bless snapshots.

## See also

- [Agents](agents.md) — per-agent provider settings
- [Configuration](../reference/configuration.md) — every provider field
