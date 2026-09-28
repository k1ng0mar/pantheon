# Troubleshooting

Every error is a structured `PantheonError`: code, layer, retryable flag, cause, remediation. The message names the code so you can search, script, and automate around it.

## Error codes

`CATEGORY_DETAIL` pattern:

| Category | Meaning |
|---|---|
| `LEDGER_*` | Event ledger errors |
| `SAFE_*` | Safety gate violations |
| `PROVIDER_*` | Provider and model errors |
| `MEMORY_*` | Memory store errors |
| `CONFIG_*` | Configuration errors |
| `PLUGIN_*` | Plugin loading and execution errors |
| `CHANNEL_*` / `GATEWAY_*` | Messaging channel / gateway errors |
| `SCHEDULE_*` | Scheduler errors |
| `AGENT_*` | Agent identity and profile errors |

## Common issues

### Runtime probe times out

The installer prints `⚠ Runtime probe timed out after 10s`. The install itself succeeded, the check could not complete. Run `pantheon doctor` to diagnose.

### Provider connection fails

Verify the API key is set in the env var named by `config.toml` (e.g. `OPENAI_API_KEY`). `pantheon doctor` reports which var is missing.

### Memory not persisting

Writes need the `MemoryWrite` capability (`coder_memory` policy or an explicit grant). Anything the model proposes stays `untrusted` until you confirm it.

### Plugin not loading

Usually a manifest typo. `pantheon doctor <dir>` first, then `pantheon extensions` to see what actually loaded.

### Gateway refuses connections

`PANTHEON_GATEWAY_ALLOW` is required. Non-listed senders get a refusal and nothing reaches the runtime.

## Diagnostics

```sh
pantheon doctor              # system preflight: config, agents, model key,
                             # ledger, memory, skills, gateway, plugins
pantheon doctor <plugin_dir> # per-plugin preflight
pantheon repair --dry-run    # what repair would change, touching nothing
pantheon repair              # fix what can be fixed safely (backs up first)
```

`doctor` diagnoses and changes nothing; `repair` fixes. A failed check names its fix, `pantheon setup` for config, `export VAR=...` for keys.

## See also

- [Getting started](../getting-started.md), install and setup
- [Configuration](configuration.md), config fields and data directory
- [Terminal reference](terminal.md), `doctor`, `repair`, `reset` flags
