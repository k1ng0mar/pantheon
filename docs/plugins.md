# Plugins and extensions

Extensions are runtime code loaded from disk: hooks that inject context,
and (via the plugin supervisor) tools that run in a child process. This
document is for writing and debugging them.

## Layout

```
my-plugin/
├── plugin.yaml      # manifest (required)
└── __init__.py      # Python entry (required for Python plugins)
```

`plugin.yaml`:

```yaml
name: my-plugin
version: "1.0.0"
provides_hooks:            # which hooks this plugin implements
  - pre_llm_call
# optional:
timeout_ms: 10000          # hook timeout (default 10000)
once_per_session: true     # fire once per session id, not every turn
```

Both Hermes manifest spellings load. Canonical is `provides_hooks`.

## Hooks

| Hook | When | Input.extra | Return |
|---|---|---|---|
| `pre_llm_call` | Before each model call, per fresh run | `message` = the user message text | `{"context": "..."}` injects a system block |
| `pre_api_request` / `post_api_request` | Provider plane | - | reserved |
| `pre_gateway_dispatch` | Before gateway delivery | - | reserved |

Contract: one JSON line in on stdin, one JSON line out on stdout.

```json
// in
{"hook": "pre_llm_call", "session_id": "s1", "platform": "cli", "extra": {"message": "hi"}}
// out
{"context": "tone guidance: be terse"}
```

- Return `{}` or nothing: silent, no injection.
- Crash, timeout, or bad output: **fail-open**. The turn continues without
  your context. A broken hook never breaks a run.
- A plugin that fails **3 times in a row** is skipped for the rest of the
  session with a loud stderr warning. One failure does not disable it;
  a wedge does. A new session retries the plugin.

`once_per_session` is tracked by (plugin, hook, session) and persists
across CLI processes via `hook_seen.json`.

## Capability gating

Plugins do not bypass the gate. Anything a plugin does goes through the
same capability policy as built-in tools. Hook context injection is a
system-prompt addition, which is why it is safe by default; a plugin that
wanted execution would need to register tools, and those tools carry
capabilities like any other.

## Tool-providing plugins (stdio protocol)

A plugin can also provide tools. The runtime spawns it, speaks
newline-delimited JSON over stdio, enforces per-call timeouts, and owns
the process group:

```json
// request (one line)
{"call_id": "c1", "tool": "weather", "args": {"city": "Kano"}}
// response (one line)
{"call_id": "c1", "result": "31C, dusty"}
```

Rules the runtime enforces:

- **Timeout**: no response within the timeout kills the process GROUP
  (TERM, then KILL), marks the plugin dead, and returns `PLUGIN_TIMEOUT`.
- **Protocol desync**: a wrong `call_id` marks the plugin dead immediately
  (a stale line would poison every later call).
- **Kill safety**: the process group is registered against the run lease.
  Killing follows the lease; a supervisor that lost its lease never
  signals a possibly-reused PGID.

Output larger than the compaction policy is head+tail compacted before it
enters the transcript.

## Doctor

```sh
pantheon doctor <plugin_dir>
```

Static checks, no execution: manifest present and parseable, declared
hooks are known hook names, entry file exists, TypeScript entries flagged
(`TS_ENTRY` warning: OpenClaw-style TS plugins need an adapter, Python is
the supported runtime), missing bins or env flagged as errors. Exit 1 on
any error finding.

## Installing

```sh
pantheon plugins list
pantheon plugins install <name>    # from the catalog
pantheon plugins enable <name>
pantheon plugins disable <name>
```

Plugins load from `$PANTHEON_EXT_DIR` (default `<data>/extensions`).
Hermes plugins load natively from `~/.hermes/plugins` when present.
Writes are atomic (tmp+rename); install and enable-state changes survive
crashes.

## Debugging checklist

1. `pantheon doctor <dir>` first. Most failures are manifest typos.
2. `pantheon extensions` shows what actually loaded (JSON).
3. `pantheon hook pre_llm_call --session test` fires your hook once and
   shows the injected context (or `(silent)`).
4. Run the plugin by hand: `echo '{"hook":"pre_llm_call",...}' | python3 __init__.py`
   and read the JSON it prints.
5. Streak-disabled? The stderr line says `skipped for the session after 3
   consecutive failures`. Restart the session after fixing the plugin.
