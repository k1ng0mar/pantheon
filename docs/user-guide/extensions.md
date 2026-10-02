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

## Security: approval and privileges

Pantheon does **not** sandbox plugins. Secrets (API keys, tokens) are scrubbed from a plugin's environment, but a plugin otherwise runs with **your full user privileges**: it can read and write your files, access the network, and execute commands as you. Gate hooks fail closed (a crash denies the action), but that is a fail-safe, not isolation.

Because of that, a third-party plugin never runs until you explicitly approve it:

```sh
pantheon extensions approve my-plugin
pantheon plugins approve my-plugin
```

You will see a warning describing exactly what approval means. **Approving is informed consent to the plugin running with your full privileges** - only approve plugins from sources you trust. The approval is recorded (name, version, content hash, timestamp); if the plugin's code changes afterwards, the approval lapses and you are asked again.

Plugins shipped with Pantheon (under `bundled/`) are first-party and don't need approval. Everything else is third-party.

## Bundled plugins: one enablement state

Pantheon ships a small catalog of first-party plugins (today: `time-gap`, a hook plugin that injects an implicit time-gap sense into the prompt). Like bundled MCP servers, they follow one lifecycle: **all disabled by default**, toggleable in the config file, the dashboard, and the mobile app, which all share a single enablement state.

```toml
[plugins.time-gap]
enabled = true
```

- The config file is the source of truth. The dashboard's plugin list and the mobile app read and write the same `[plugins.<name>]` entries, so the three surfaces can never disagree.
- The plugin's files are materialized automatically: on startup the runtime copies the shipped source into `<data_dir>/extensions/bundled/<name>/` (without touching anything already there). Seeding never enables - files sit inert until you flip the flag.
- For a bundled plugin, the config entry wins over the plugin manifest's own `enabled` flag. For a third-party plugin, the approval store (above) remains the gate - unapproved third-party code never loads no matter what any flag says.
- The agent can propose enabling a bundled plugin, but never silently: the proposal goes through the normal approval flow (the run parks, the request is audit-logged, and the plugin switches on only if you grant). Only catalog plugins can be proposed - there is no agent path to install arbitrary plugins.

## Plugins vs MCPs: the trust distinction

Both extend what the agent can do, and they are trusted completely differently - the approval UI surfaces this on purpose:

- A **plugin** is code Pantheon itself runs on your machine: the runtime loads its manifest and executes it (tool plugins as spawned runner processes behind the capability gate, hook plugins as per-fire child processes with a scrubbed environment). Either way it runs with your user privileges, unsandboxed. Enabling a plugin is consent to run its code.
- An **MCP server** is an out-of-process integration: Pantheon speaks the MCP protocol to it but never executes its code - the server may run on another machine entirely. Trust there is in the endpoint and its configuration, not in shipped code.

## Manage and debug

```sh
pantheon plugins list|install <name>|enable|disable|approve <name>
pantheon extensions                 # what actually loaded (+ pending approvals)
pantheon extensions approve <name>  # approve a hook extension
pantheon hook <name> [--session S]  # run a hook once and see what it adds
pantheon doctor <plugin_dir>        # preflight check: manifest, hooks, files
```

When something does not work, check in this order: `doctor` (usually a typo in the manifest), `extensions` (did it load?), then run the hook by hand (`echo '{"hook":"pre_llm_call",...}' | python3 __init__.py`).

A note on MCP servers: `pantheon mcp list` shows declared MCP servers and whether each could load, but agents cannot reach MCP servers yet. No launcher exists yet either.

## See also

- [Memory](memory.md): skills, the knowledge half of extensibility
- [Terminal reference](../reference/terminal.md#extend): plugin and hook commands
- [Architecture](../developer/architecture.md): hooks and capabilities
