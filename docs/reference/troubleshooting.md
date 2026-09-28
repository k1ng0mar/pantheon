# Troubleshooting

Every error has a structured code, so you can search for it, script around it, and get a useful message instead of a shrug. The message always names the code.

## Error codes

Codes look like `CATEGORY_DETAIL`:

| Category | Meaning |
|---|---|
| `LEDGER_*` | The conversation record had a problem |
| `SAFE_*` | A safety rule was broken |
| `PROVIDER_*` | The AI provider or model failed |
| `MEMORY_*` | The memory store had a problem |
| `CONFIG_*` | Something is wrong in the config |
| `PLUGIN_*` | A plugin failed to load or run |
| `CHANNEL_*` / `GATEWAY_*` | A chat app or the gateway had a problem |
| `SCHEDULE_*` | A scheduled job had a problem |
| `AGENT_*` | An agent name or profile had a problem |

## Common problems

### Install check times out

The installer prints a timeout warning. The install itself worked; only the self-check could not finish. Run `pantheon doctor` to see what is going on.

### Cannot reach the AI provider

Make sure the API key is set in the environment variable named in your config (for example `OPENAI_API_KEY`). `pantheon doctor` tells you exactly which one is missing.

### Memory is not saving

Writing memories needs permission (the `coder_memory` policy, or an explicit grant). Anything the model suggests stays `untrusted` until you confirm it.

### A plugin will not load

Usually a typo in its manifest file. Run `pantheon doctor <dir>` first, then `pantheon extensions` to see what actually loaded.

### The gateway refuses everyone

`PANTHEON_GATEWAY_ALLOW` is required, and it lists who may talk to it. Anyone not on the list is turned away before the assistant ever sees the message.

## Diagnostics

```sh
pantheon doctor              # health check: config, agents, model key,
                             # history, memory, skills, gateway, plugins
pantheon doctor <plugin_dir> # check one plugin
pantheon repair --dry-run    # show what repair would change, touching nothing
pantheon repair              # fix what can be fixed safely (backs up first)
```

`doctor` diagnoses and changes nothing; `repair` does the fixing. A failed check names its fix: run `pantheon setup` for config problems, `export VAR=...` for missing keys.

## See also

- [Getting started](../getting-started.md): install and setup
- [Configuration](configuration.md): settings and the data directory
- [Terminal reference](terminal.md): `doctor`, `repair`, `reset` flags
