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

Tool plugins and hook plugins do not have the same isolation, so Pantheon does not make one blanket sandbox claim for both.

- **Tool plugins** (spawned runner processes in `pantheon-exec`) run sandboxed at the manifest level (`high` by default, `very_high` opt in) through immutable content addressed staging plus `build_sandboxed_checked`. Filesystem is read only except the per plugin work directory under the data dir plugin work folder, network is denied unless the manifest declares it, and the child fails closed with `PLUGIN_SANDBOX_UNAVAILABLE` when the wrapper cannot initialize. Same uid ptrace outside the sandbox is not prevented, and non Linux hosts have no namespace backend yet, so the spawn refuses there unless the manifest explicitly opts into direct fallback.
- **Hook plugins** (per-fire child processes in `pantheon-extensions`) run behind the capability policy when one is configured, which is the production path. `PluginEnable` maps to the VeryHigh sandbox level, the runner builds the child through `build_sandboxed_checked`, and a boundary whose wrapper cannot initialize refuses the spawn instead of running un-isolated, unless the direct fallback is explicitly opted in. Their environment is scrubbed to `PATH` plus `PANTHEON_DATA_DIR` when configured. When no policy is configured (standalone and test use only), the hook runner spawns directly, as before.

Because of that, a third-party plugin never runs until you explicitly approve it:

```sh
pantheon extensions approve my-plugin
pantheon plugins approve my-plugin
```

You will see a warning describing exactly what approval means. **Approving is informed consent to the plugin running with your full privileges** - only approve plugins from sources you trust. The approval is recorded (name, version, content hash, timestamp); if the plugin's code changes afterwards, the approval lapses and you are asked again.

Plugins shipped with Pantheon (under `bundled/`) are first-party and don't need approval. Everything else is third-party.

## Bundled plugins: one enablement state

Pantheon ships a set of first-party plugins through the embedded registry (cf-ops, noisegate, hookify, doc-pack, and friends - tool and hook kinds). Like bundled MCP servers, they follow one lifecycle: **disabled by default unless the plugin ships on**, toggleable in the config file, the dashboard, and the mobile app, which all share a single enablement state. (The separate vendored-extension catalog that once carried the `time-gap` hook is empty - time-gap went native via `[temporal]`, which is on by default with nothing to enable.)

```toml
[plugins.<name>]
enabled = true
```

- The config file is the source of truth. The dashboard's plugin list and the mobile app read and write the same `[plugins.<name>]` entries, so the three surfaces can never disagree.
- A catalog plugin's files are materialized automatically: on startup the runtime copies the shipped source into `<data_dir>/extensions/bundled/<name>/` (without touching anything already there). Seeding never enables - files sit inert until you flip the flag.
- For a bundled plugin, the config entry wins over the plugin manifest's own `enabled` flag. For a third-party plugin, the approval store (above) remains the gate - unapproved third-party code never loads no matter what any flag says.
- The agent can propose enabling a bundled plugin (embedded registry or vendored catalog), but never silently: the proposal goes through the normal approval flow (the run parks, the request is audit-logged, and the plugin switches on only if you grant). Only bundled names can be proposed - there is no agent path to install arbitrary plugins.

## Plugins vs MCPs: the trust distinction

Both extend what the agent can do, and they are trusted completely differently - the approval UI surfaces this on purpose:

- A **plugin** is code Pantheon itself runs on your machine: the runtime loads its manifest and executes it. Tool plugins run as spawned runner processes behind the capability gate with full user privileges and no namespace sandbox. Hook plugins run as per-fire child processes with a scrubbed environment, sandboxed at the VeryHigh level whenever a capability policy is configured and spawned directly only when no policy is configured. See the security section above for which kind gets which isolation.
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
