# CLI reference

Every `pantheon` verb, its flags, and its exit codes. Shared rules first.

## Conventions

- Exit 0 = success. Exit 1 = operation failed (the error message names a
  structured code like `SAFE_STALE` or `LEDGER_OPEN`). Exit 2 = usage error
  (bad arguments).
- Flags are `--flag value` or `--flag=value`. Boolean flags (`--yes`) take
  no value.
- `PANTHEON_DATA_DIR` overrides the data dir (default `~/.pantheon`) for
  every verb.
- JSON on stdout means the command is for scripts: `doctor`, `audit`,
  `extensions`, `plugins list`, `status` (in some modes). Everything else is
  human-oriented text.

## Session verbs

### chat
```
pantheon chat [--id ID] [--model M] [--provider P] [--key K] [--choose] "message"
```
Runs one conversation turn with the full tool loop. `--id` resumes or names
a run; `--choose` opens an interactive model picker from the catalog.
Model/provider flags override config.toml, which overrides
`PANTHEON_MODEL`/`PANTHEON_PROVIDER`.

Special providers: `mock` requires `PANTHEON_MOCK_FILE` (a fixture JSON,
see eval/cases.json for the shape) and fails with `MOCK_PROVIDER_UNCONFIGURED`
otherwise.

### run
```
pantheon run [--id ID] [--say TEXT] [--tool NAME] [--fail CODE] [--ext] [--platform P]
```
Direct runtime entry used by evals and tests: record a run with optional
tool execution or failure. Not a user verb, but stable.

### explain / status / audit
```
pantheon explain <run_id>
pantheon status <run_id>
pantheon audit <run_id> [OUT.jsonl]
```
`explain` replays the ledger for a run in words. `status` prints the run
state (`running`, `awaiting_approval`, `completed`, `failed`, `canceled`,
or `unknown` for an id that never ran). `audit` writes a sequence-validated
JSONL trajectory (3 events minimum for a simple run).

## Approval verbs

```
pantheon grant <run_id> <scope>     # approve a parked tool call
pantheon deny <run_id> [scope]      # refuse; scope auto-detected if omitted
```
A granted call re-executes on resume. A denied call settles into the
transcript as "denied by operator" and the run continues. Both fail with
`RT_APPROVAL_RESOLVED` if the scope was already answered.

## File-edit verbs (safewrite)

```
pantheon preview <path> <file-with-new-content>
pantheon stage <path> <file-with-new-content> [--expect HASH]
pantheon apply <path> <file-with-new-content> [--expect HASH] [--run ID]
pantheon checkpoint <path>... [--run ID]
pantheon rollback (--ckpt ID | --seq N)
```
`preview` is read-only. `apply` snapshots a checkpoint first, writes
atomically (tmp+fsync+rename), and journals the apply. A stale
`--expect` hash fails with `SAFE_STALE` and changes nothing. `rollback`
restores by checkpoint id or ledger sequence.

## Memory verbs

```
pantheon memory import [PATH]      # markdown -> store (through the policy gate)
pantheon memory export [PATH]      # store -> markdown
pantheon memory sync [PATH]        # bidirectional with conflict detection
pantheon memory put KEY VALUE
pantheon memory recall QUERY
pantheon memory backend list|select NAME
```
The default file is `MEMORY.md` in the data dir. Values containing lines
that look like `# headings` are escaped on export and unescaped on import.
Sync refuses with a conflict report when both sides changed since the last
sync.

## Plugin verbs

```
pantheon extensions                     # list loaded extensions (JSON)
pantheon plugins list                   # installed plugins
pantheon plugins install <name>         # from the catalog
pantheon plugins enable|disable <name>
pantheon hook <name> [--session S]      # fire a hook once (eval/debug)
pantheon doctor <plugin_dir>            # static preflight for one plugin
```

## System verbs

```
pantheon setup [--yes] [--profile P] [--provider P] [--model M]
               [--api-key-env ENV] [--policy reader|coder|coder_memory]
               [--memory BACKEND] [--packs a,b] [--plugins a,b]
pantheon doctor              # system preflight (config, key, ledger, memory, plugins)
pantheon doctor <plugin_dir> # per-plugin preflight (as above)
pantheon reset --config | --state | --everything [--yes]
pantheon providers           # catalog listing
```
`reset --config` removes config files only. `reset --state` removes
ledger.db, memory.db, and gateway cursors, and refuses while a run lease is
active. Typed confirmation (`reset`) is required unless `--yes`.

## AG-UI verbs

```
pantheon serve [--host H] [--port P]     # AG-UI server (web UI at /, RPC at /agui/rpc)
pantheon stream <run_id> [--after N]     # SSE stream to stdout
pantheon sign <task_id> [--mime M] [--ttl MS]   # signed artifact URL
pantheon channel <run_id> [--thread T]   # replay frames through the channel seam
```

## Gateway verbs

```
pantheon gateway                  # run Discord + Telegram surfaces (env tokens)
pantheon gateway <run_id> --approve <stage> | --deny <stage>  # pipeline gates
```
`pantheon gateway` (no args) runs the channel daemon: Discord gateway
websocket and Telegram long-poll feeding the runtime. Tokens:
`PANTHEON_DISCORD_TOKEN`, `PANTHEON_TELEGRAM_BOT_TOKEN`.

## Pipeline verbs

```
pantheon pipeline --spec "task" [RUN_ID]
pantheon pipeline RUN_ID --approve plan|review
pantheon pipeline RUN_ID --deny plan|review
```
Without a positional run id one is generated and printed on the park line.
`PANTHEON_PIPELINE_EVAL=1` enables the strict evaluator loop inside
implement.

## Environment variables

| Variable | Effect |
|---|---|
| `PANTHEON_DATA_DIR` | Data directory (default `~/.pantheon`) |
| `PANTHEON_EXT_DIR` | Extension directory (default `<data>/extensions`) |
| `PANTHEON_PROVIDER` / `PANTHEON_MODEL` | Default model when no config/flags |
| `PANTHEON_API_KEY` | API key fallback (config names better ones) |
| `PANTHEON_MOCK_FILE` | Mock transport fixture (deterministic tests) |
| `PANTHEON_DISCORD_TOKEN` | Discord bot token for `pantheon gateway` |
| `PANTHEON_TELEGRAM_BOT_TOKEN` | Telegram bot token for `pantheon gateway` |
| `PANTHEON_GENUI_SECRET` | HMAC secret for signed artifact URLs |
| `PANTHEON_GENUI_BASE` | Base URL signed URLs point at |
| `PANTHEON_STALL_BUDGET_MS` / `PANTHEON_PROBE_TIMEOUT_MS` | Watchdog tuning |
| `PANTHEON_RUN_LEASE_TTL_MS` | Run lease TTL (default 30000) |
| `PANTHEON_ALLOW_MEMORY` | Legacy memory-policy toggle (config policy wins) |
| `PANTHEON_MEMORY_NAMESPACE` | Namespace for memory verbs (default `nyx`) |
| `PANTHEON_PIPELINE_EVAL` | Enable the strict pipeline evaluator |
| `PANTHEON_SECRET_*` | Env vault entries for the secrets broker |
