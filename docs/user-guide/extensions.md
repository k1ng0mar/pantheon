# Extensions

Two ways to teach Pantheon new tricks. **Skills** are portable know-how: written instructions for how to do something (see [Memory](memory.md)). **Extensions** are actual code: plugins that run when something happens, or new tools. This page is about extensions.

## Write one

```
my-plugin/
├── plugin.yaml      # describes the plugin (required)
└── __init__.py      # the code (Python)
```

```yaml
name: my-plugin
version: "1.0.0"
provides_hooks:
  - pre_llm_call
```

Pantheon sends your code one JSON line, your code answers with one JSON line. Answer `{"context": "..."}` to add background the model should see, or `{}` to stay quiet. Hooks have 10 seconds to answer, so do slow work elsewhere.

## How they behave

- **Most hooks fail open**: if your plugin crashes, times out, or returns garbage, the hook is skipped and the conversation continues. A broken plugin never breaks Pantheon.
- **The permission hook fails closed**: it can deny tool calls, so its failures count as denials.
- Three crashes in a row disables the plugin for the session, loudly. A new session gives it another chance.
- Nothing bypasses your permissions. A plugin's tools need capabilities like any built-in tool.

## Manage and debug

```sh
pantheon plugins list|install <name>|enable|disable <name>
pantheon extensions                 # what actually loaded
pantheon hook <name> [--session S]  # run a hook once and see what it adds
pantheon doctor <plugin_dir>        # preflight check: manifest, hooks, files
```

When something does not work, check in this order: `doctor` (usually a typo in the manifest), `extensions` (did it load?), then run the hook by hand (`echo '{"hook":"pre_llm_call",...}' | python3 __init__.py`).

A note on MCP servers: `pantheon mcp list` shows declared MCP servers and whether each could load, but agents cannot reach MCP servers yet. No launcher exists yet either.

## See also

- [Memory](memory.md): skills, the knowledge half of extensibility
- [Terminal reference](../reference/terminal.md#extend): plugin and hook commands
- [Architecture](../developer/architecture.md): hooks and capabilities
