# Extensions

Two tiers: **skills** are portable knowledge (they explain how, see [Memory](memory.md)); **extensions** are runtime code, hooks that inject context, tools that run in child processes. This page covers extensions.

## Write one

```
my-plugin/
├── plugin.yaml      # manifest (required)
└── __init__.py      # Python entry
```

```yaml
name: my-plugin
version: "1.0.0"
provides_hooks:
  - pre_llm_call
```

One JSON line in on stdin, one JSON line out on stdout. Return `{"context": "..."}` to inject a system block, `{}` to stay silent. Hook timeout is global (10s), answer fast, work asynchronously.

## Semantics

- **Fail-open hooks** (context injection, result transforms): a crash, timeout, or bad output skips the hook; the turn continues. A broken plugin never breaks a run.
- **The gate hook** (`pre_tool_call`) fails **closed**: it may deny, so its failures deny.
- Three consecutive failures disable the plugin for the session, loudly. A new session retries.
- Nothing bypasses capability policy. A plugin that executes registers tools carrying capabilities like any built-in.

## Manage and debug

```sh
pantheon plugins list|install <name>|enable|disable <name>
pantheon extensions                 # what actually loaded
pantheon hook <name> [--session S]  # fire once, see the injection (or silence)
pantheon doctor <plugin_dir>        # static preflight: manifest, hooks, entry files
```

Debug order: `doctor` (usually a manifest typo), `extensions` (did it load?), fire the hook by hand (`echo '{"hook":"pre_llm_call",...}' | python3 __init__.py`). Hermes plugins load natively; OpenClaw TypeScript entries are flagged, Python is the supported runtime.

`pantheon mcp list` (read-only) shows migration-declared MCP servers and whether each could register. A stdio JSON-RPC client exists in the `pantheon-mcp` crate (connect, list tools, call tools), but no CLI verb or tool surface wires it up yet, agents can't reach MCP servers today. No launcher yet.

## See also

- [Memory](memory.md), skills, the knowledge half
- [Terminal reference](../reference/terminal.md#extend), plugin and hook verbs
- [Architecture](../developer/architecture.md), hooks, compat adapter, capabilities
