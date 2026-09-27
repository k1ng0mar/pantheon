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
  `explain`, `stream`, `mcp list --json`. Every other verb, including
  `status`, `extensions` and `plugins list`, prints human-oriented text.
  (`status` prints a bare run state such as `running` or `complete`.)

## Session verbs

### (bare)
```
pantheon              # opens the interactive session
```
The terminal interface. Every input is a turn on the current conversation, one
ledger run, so history and approvals persist across process exits.
Auto-resumes the most recent run on open; `/new` starts fresh.

Commands: `/help`, `/new`, `/runs`, `/resume [ID|n]`, `/history`,
`/status`, `/name [TITLE]`, `/memory QUERY`, `/remember KEY TEXT`,
`/policy`, `/model P M`, `/exit`. Slash commands are session-local and
never reach the model. `/name` alone prints the current session title;
with a title it renames the conversation (durable `SessionTitled` event,
`source: manual`, overwriting any generated title).

Fresh conversations get a one-line **session title** generated from the
first prompt by the title auxiliary (config `[title_gen]`, default
`auto` = the run's default model — see docs/configuration.md). `/runs`,
`/history`, and `/status` show it; the title is a `SessionTitled` ledger
event, so `pantheon runs <run>` shows how it was produced.

### run
```
pantheon run --taskID <id> --say "text" [--deliver session|telegram|discord]
pantheon run [--id ID] [--say TEXT] [--tool NAME] [--fail CODE] [--ext] [--platform P]
```
Two modes, decided by whether `--deliver` is passed:

**With `--deliver`** it runs a real model turn and delivers the answer
somewhere other than this terminal. `--taskID` names the run to continue (a
new one is created if omitted), `--say` is the task, and the target picks
the destination:

| target | effect |
|---|---|
| `session` (default) | print the answer here, then the run id |
| `telegram` | queue it for the gateway to send |
| `discord` | queue it for the gateway to send |

`telegram`/`discord` write to a durable outbox under
`<data_dir>/gateway/outbox/`, which `pantheon gateway` drains. The queue
outlives the process, so a delivered task is not lost if the gateway is
down when it is queued.

**Without `--deliver`** it writes synthetic ledger events and never calls a
model. `--say` records a progress line, `--fail CODE` ends the run failed,
and `--ext` fires `pre_llm_call` and prints whatever context it injects. This
mode exists so recovery and ledger tooling can seed a run without spending a
model call.

### runs / logs / audit

Three different questions, three verbs. They used to be conflated.

```
pantheon runs                      # every run, with status and title
pantheon runs <run_id>             # one run's event trace, in words
pantheon runs <run_id> --metrics   # one line of counters, folded from the ledger
pantheon audit <run_id> [OUT.jsonl]
pantheon logs [name] [filters]   # the runtime's log FILES
```

**`runs`** reads the ledger. With no argument it lists every run with its
status and title, using the same status vocabulary as the in-session `/runs`
view. With a run id it replays that run's events. This verb was `explain`,
which described nothing, and then briefly `logs`, which described something
else. `audit` writes a sequence-validated JSONL trajectory (3 events minimum
for a simple run).

`--metrics` folds the run's event log into counters: runs started /
completed / failed / canceled, tool calls, model turns, approvals requested /
granted / denied, sub-agents spawned, and context trims / compressions. Failed
and canceled are counted separately from completed, because the number you are
looking for is usually the one that is *not* completed. The two context
counters are there so a run that has been quietly dropping history is visible
without scrolling a hundred events. This replaced the `pantheon-otel` crate's
`metrics_from`, which no user could reach.

Run state (`running`, `awaiting_approval`, `completed`, `failed`, `canceled`)
is *not* part of the per-run trace. It is the listing, and inside a session
`/status`. To settle a run stranded by a crash, see [repair](#repair).

**`logs`** reads log files, not runs. The runtime writes
`<data_dir>/logs/agent.log` (everything), `errors.log` (warnings and worse),
and `gateway.log`:

```
pantheon logs list                          what exists and how big
pantheon logs                               tail agent.log
pantheon logs errors                        tail errors.log
pantheon logs agent -n 200                  last 200 lines
pantheon logs -f                            follow, like tail -f
pantheon logs --level error                 only ERROR and above
pantheon logs --since 1h                    only the last hour
pantheon logs --grep PROVIDER_EXHAUSTED     only matching lines
```

The file list is a closed set, not a `*.log` glob, so the `ledger.db.<stamp>.bak`
copies `repair` writes and any editor swap file cannot show up as a log. Levels
come from `PANTHEON_LOG_LEVEL` (`debug`/`info`/`warning`/`error`, default
`info`).

The two answer different questions and neither substitutes for the other. "Why
did that turn end the way it did" is a ledger question — `runs <id>`, which has
the full event trail including the approval decisions. "What has the process
been doing" is a log question — `logs`, which also covers the failures that
happen *before* a run exists, like a provider chain exhausting on a config with
no run recorded at all.

## Approvals

Approvals are answered in the session that raised them. The terminal interface
shows a permission card (`y` allow, `n` deny).

Out of band — a run parked from a script, a gateway message, or a second
terminal:

```
pantheon run --taskID <run_id> --grant <scope>   # approve, then continue the run
pantheon run --taskID <run_id> --deny  <scope>   # refuse
pantheon run --taskID <run_id> --grant <scope> --no-resume   # record only
```
A granted call re-executes on resume. A denied call settles into the
transcript as "denied by operator" and the run continues. Both fail with
`RT_APPROVAL_RESOLVED` if the scope was already answered.

## Memory verbs

```
pantheon memory import [PATH]      # markdown -> store (through the policy gate)
pantheon memory export [PATH]      # store -> markdown
pantheon memory sync [PATH]        # bidirectional with conflict detection
pantheon memory put KEY VALUE
pantheon memory list                    # every memory in the agent namespace
pantheon memory recall QUERY
pantheon memory backend list|select NAME
pantheon memory backend select NAME [k=v ...]   # bridge options (url, key, ...)
pantheon memory backend scaffold NAME [http|stdio]  # starter bridge config
pantheon memory confirm KEY              # acknowledge a proposed write
pantheon memory vault search|read|list ...  # encrypted vault inspection
```
The default file is `MEMORY.md` in the data dir. Values containing lines
that look like `# headings` are escaped on export and unescaped on import.
Sync refuses with a conflict report when both sides changed since the last
sync.

## Plugin verbs

```
pantheon extensions                     # list loaded extensions (names)
pantheon plugins list                   # installed plugins
pantheon plugins install <name>         # from the catalog
pantheon plugins enable|disable <name>
pantheon hook <name> [--session S] [--platform P]  # fire a hook once (eval/debug)
pantheon doctor <plugin_dir>            # static preflight for one plugin
```

## Migration verbs

```
pantheon migrate detect                        # which sources are installed here
pantheon migrate show <source> [path]          # every detected item + its disposition
pantheon migrate plan <source> [path]          # dry run: import / archive / skip
pantheon migrate apply <source> [path] [--yes] # backup → import → validate
pantheon migrate validate <source> [path]      # re-check current targets, no writes
```

`<source>` is `hermes`, `openclaw`, or `omp` (aliases: `oh-my-pi`, `pi`).
`[path]` defaults to `~/.hermes`, `~/.openclaw`, `~/.omp`. Flags: `--path DIR`
to point at a different root, `--kind skill,agent,...` to restrict the plan,
`--json` for machine-readable output, `--yes` to skip the approval gate.

```
detect    read-only  lists sources, item counts, versions
show      read-only  one line per item: kind, mappable, path, and why
plan      read-only  renders the full plan; writes nothing
apply     WRITES     backs up every existing target, imports, then validates
validate  read-only  re-reads targets and proves each import landed
```

`apply` is the only verb that writes, and it needs either `--yes` or the
source name typed at the prompt. It always runs `backup → apply → validate`
and exits non-zero if any import or validation step failed.

`--kind` accepts: `skill`, `agent`, `rule`, `command`, `prompt`, `extension`,
`persona`, `memory`, `provider`, `mcp`, `schedule`, `channel`, `session`,
`credentials`.

### Where things land

| Kind | Destination |
|---|---|
| `skill` | `<data_dir>/skills/<name>/` |
| `agent`, `rule`, `command`, `prompt` | `<data_dir>/<kind>/<name>` |
| `extension` | `PANTHEON_EXT_DIR/<name>` (+ a generated `plugin.yaml`) |
| `mcp` | `<data_dir>/mcp/<source>.json` — a declaration, not the config |
| `credentials` | merged into `<data_dir>/.env` + `<data_dir>/credentials/<source>.json` |
| `session` | `<data_dir>/imported-sessions/<source>/` + `manifest.json`, then indexed into `session_search` |
| `provider` | `<data_dir>/providers/imported.toml` (sidecar), or `config.toml` with `--merge-providers` |
| `persona`, `memory` | the memory plane, as `memory://<kind>/<name>` |

### Credentials

A source `.env` is **merged into `<data_dir>/.env`**, which is already
Pantheon's own key store — the same file `pantheon model` writes and
`load_dotenv` reads, so an imported key works immediately with no wiring.

- Only provider, channel, and MCP-auth names carry. Config (`HOME`, ports,
  timeouts, allow-lists) stays behind.
- Written `0600`, comments and ordering preserved.
- **An existing key is never clobbered.** It is reported as `already present`
  and left for you to resolve.
- A value is written to `<data_dir>/.env` and nowhere else — never into the
  plan, a report, a log, or the names-only manifest.
- Other credential *files* (`auth.json`, `models.yml`, `agent.db`,
  `broker.token`, `*.pem`, `*.key`) are reported and skipped entirely, never
  imported and never archived.

Pre-images of anything overwritten go to `<data_dir>/migrate-backups/<timestamp>/`
with a JSON manifest beside them.

### Imported sessions

Transcripts are quarantined under `<data_dir>/imported-sessions/<source>/` and
then indexed into the same `session_search` store the live runtime writes, so a
migrated history is searchable straight away.

- `run_id` is namespaced `migrated:<source>:<session>`, so a migrated hit can
  never be mistaken for a real ledger event.
- `chunk_id` is derived from (source, file, line), so re-running converges
  instead of duplicating rows.
- Parsing is tolerant: a truncated final line, which is normal in a live
  transcript, is skipped rather than failing the import.
- An oversized tool result is clipped so it cannot dominate the FTS table.
- Indexing is best-effort. If it fails the transcripts are still on disk, and
  the run reports it rather than failing an otherwise complete migration.

### Fetching an endpoint's models

```
pantheon provider models <name>      # live list from the endpoint
```

Fetched on demand from `{base_url}/models`, never read from config. A starred
entry is one you have named on that endpoint; anything under "recorded but not
offered now" is a model the endpoint has retired, reported rather than silently
dropped. If the fetch fails the command falls back to the recorded names and
says so.

Measured on the reference machine, the local router offers 5 models where the
source agent's config listed 1 — a concrete case of the snapshot being wrong.

### MCP declarations

`apply` writes `<data_dir>/mcp/<source>.json` — a Pantheon-shaped declaration
carrying transport, command, args, and url, with credentials reduced to
declared `requires_env` names. `pantheon mcp list` reads those back and reports
which servers could register:

```
SOURCE     NAME                       TRANS   READY   TARGET
hermes     aws-mcp                    stdio   yes     /home/ubuntu/.hermes/bin/uvx
hermes     composio                   http    no      https://connect.composio.dev/mcp  (needs a credential (MCP_COMPOSIO_API_KEY))

4 server(s) declared, 3 ready to register
```

A `ready` server is **prepared but not attached**: Pantheon still has no MCP
server launcher (spec section 15). The report says so rather than implying the
servers are live.

### Custom providers

A source's `providers:` block becomes `[custom_providers.<id>]` sections
carrying `base_url`, `api_mode`, and `key_env`.

- **Default is a sidecar.** `<data_dir>/providers/imported.toml` is written for
  review; `config.toml` is not touched.
- `--merge-providers` also merges into `<data_dir>/config.toml`. The merge is
  **text-level**, so your hand-tuned config is left byte-identical outside the
  added sections, and a pre-image is written to `config.toml.pre-migrate`.
- An existing `[custom_providers.<id>]` is **skipped, never replaced**, and
  reported by name.
- A source that stored a **literal** API key rather than a `${VAR}` reference
  gets `key_env` omitted with a comment — no env var is invented, and the value
  is never copied. Add it with `pantheon provider add`.
- **No model list is written.** Another agent's config holds a snapshot of a
  third-party endpoint taken whenever that agent last synced — for an
  aggregator it is stale almost immediately, so baking it in would be a lie
  with a config file's authority. The sidecar notes how many ids the source
  advertised and points at `pantheon provider models`.
- `[custom_providers.*].models` therefore only ever holds models **you named**.
  `pantheon model` records one when you type an id by hand; a model picked from
  a live fetch is not recorded, because the next fetch will offer it again.

### Builtin provider keys

Nothing needs migrating: Pantheon's catalog names each provider's key env after
the same `<PROVIDER>_API_KEY` convention the sources use, so a carried key
usually lands under the name the runtime already reads. `apply` prints a
reconciliation after every run:

```
key store vs pantheon's provider catalog:
  6 key(s) already match a catalog provider and are usable as-is:
    OPENROUTER_API_KEY -> provider openrouter
    ...
```

A key whose name matches a catalog provider but not its `key_env` is reported
with the variable Pantheon expects, so it can be renamed. Keys matching nothing
(a channel token, an MCP key, a custom-provider key) are left alone — being
absent from the report is the signal, not an error.

## System verbs

```
pantheon setup [--yes] [--profile P] [--provider P] [--model M]
               [--api-key-env ENV] [--policy reader|coder|coder_memory]
               [--memory BACKEND]
               [--fallback-provider P] [--fallback-model M]
pantheon doctor              # system preflight (config, key, ledger, memory,
                             #   skills, gateway, plugins)
pantheon doctor <plugin_dir> # per-plugin preflight (as above)
pantheon reset --config | --state | --everything [--yes]
pantheon providers           # catalog listing
pantheon fallback list                    # the ordered fallback chain
pantheon fallback add <provider> <model>  # append
pantheon fallback insert <i> <p> <m>      # insert at position
pantheon fallback remove <i>             # by index (names may repeat)
pantheon provider <add|list|remove>   # custom-endpoint registry
                             # add [--name N|--provider N] [--base-url U|:port] [--api-mode M]
                             #     [--key K1,K2] [--api-key-env E] [--set V=W]...
                             # remove [NAME] [--delete-key]; no NAME opens a picker
pantheon provider models <name>       # live model list fetched from the endpoint
pantheon model [--list] [--auxiliary judge|compression|title_gen|embeddings|search_synthesis|vision|scheduled|mcp_synthesis]
                             # 39 builtins + customs. provider picker → wire mode →
                             # comma-stacked keys (→ <data_dir>/.env) → live /models
                             # fetch → model pick → auxiliary step at the bottom.
                             # :port shorthand = http://127.0.0.1:port/v1
```
Stacked keys (`PANTHEON_KEY_X=k1,k2`) rotate on 401/403/429 inside one
turn. `model --list` shows the default, auxiliaries, custom providers,
and whether each key resolves — without ever printing a value.
`reset --config` removes config files only. `reset --state` removes
ledger.db, memory.db, and gateway cursors, and refuses while a run lease is
active. Typed confirmation (`reset`) is required unless `--yes`.

### repair

`repair` finds and fixes anything wrong with this install. `doctor` is
diagnosis and changes nothing; `repair` is the fixing half, kept as a separate
verb so neither can quietly do the other's job (a `doctor` that mutated state
would be unsafe to run in a loop).

```
pantheon repair              # find and fix
pantheon repair --dry-run    # report what would change, touch nothing
pantheon repair --json       # machine-readable
```

The mechanism is one check body with two callers, not two implementations:
each entry in the registry pairs a read-only `diagnose` with a `repair`, and
`diagnose` always runs first. So `repair` on a healthy install changes
nothing, and a problem with no safe automatic fix is reported as `manual`
rather than skipped.

Seven checks, spanning the same ground as `doctor`:

| check | automatic | notes |
|---|---|---|
| `data-dir-layout` | yes | creates a missing data dir; a file where a dir belongs is never overwritten |
| `config` | partly | writes a default when missing; a config that exists is **never** overwritten |
| `ledger-integrity` | no | structural damage has no in-place fix, so it backs up and says so |
| `stranded-runs` | yes | settles runs no live session is driving |
| `search-index` | partly | recreates the FTS table; re-indexing existing runs is **not** implemented |
| `memory-index` | yes | a full rebuild — `memories_fts` is external-content, so records come back |
| `skills` | no | a rejected skill is usually a front-matter typo, so the file is kept |

Only a genuine failure exits 1. A `manual` finding means the operator has a
decision to make, which is the registry working as designed, not a failure.

Every fixer that mutates takes a backup first and names the copy:

```
$ pantheon repair
fixed  stranded-runs: run_1790460358409_7904 is still 'running' → settled 1 stranded run(s)
fixed  search-index: FTS index is missing or empty → recreated the FTS index as an
       empty table. A full re-index of existing runs is NOT implemented, so
       previously indexed runs are not searchable until they are re-indexed.
       Backup: /home/you/.pantheon/ledger.db.1790462014193.bak

2 fixed, 0 need a human, 0 failed
```

Settling a stranded run appends real events (`RunProgress` naming the reason,
then `RunFailed` with code `REPAIRED`) rather than overwriting status, so the
ledger still explains how the run ended. A run holding a **live** lease is
never touched: that one is a session still working, not a corpse. "Live" means
the lease is unexpired *and* recently heartbeated, because a lease row outlives
`kill -9` — nothing gets to release it, so a TTL-only test would report a
crashed run as busy for a full lease TTL after the crash.

Structurally damaged SQLite has no in-place fix. `repair` takes the backup and
tells you exactly what is left rather than writing to a damaged file.

## AG-UI verb

```
pantheon serve [--host H] [--port P]     # AG-UI server (web UI at /, RPC at /agui/rpc)
```

`serve` owns the whole wire surface: `GET /agui/stream` (SSE replay and
25s long-poll), `POST /agui/rpc` (JSON-RPC), `GET /agui/blob/<task>` (signed
generative-UI bytes), and `GET /agui/health`. `PANTHEON_SERVE_TOKEN` gates
every route except health.

The `stream` and `channel` verbs were removed. Both were standalone CLI
wrappers around the same `snapshot_frames` + `SseEncoder` calls the server
makes internally, and `pantheon-api` never referenced them — `serve` does
not depend on either. `channel` was, by its own doc comment, a demo that
replays frames into an in-memory surface to prove the seam; nothing
consumed its output. To watch a live run, use the SSE stream the server
already exposes, or `pantheon runs <run_id>` for the durable event list.

## Gateway verbs

```
pantheon gateway start            # install + start a supervised service
pantheon gateway restart          # restart it
pantheon gateway stop             # stop it
pantheon gateway status           # is it running?
pantheon gateway run              # foreground (what start wraps)
```
`gateway run` is the process: Discord gateway websocket and Telegram
long-poll feeding the runtime, plus a drain loop for the outbox that
`pantheon run --deliver` writes to.

`gateway start` is what you normally want. It writes a service unit
(`~/.config/systemd/user/pantheon-gateway.service` on Linux, a launchd
agent on macOS) with an **absolute** `ExecStart` — a bare `pantheon` would
resolve against the unit's own `PATH`, not your shell's — then starts and
enables it, so it comes back after a reboot. The unit sets
`PANTHEON_DATA_DIR` and nothing else: tokens stay in `<data_dir>/.env`
rather than being baked into a file you might paste somewhere.

`start` runs the same preflight as `run` before it installs anything, and
then confirms the unit actually reached `active`. `systemctl start` returns
0 for a unit that immediately crash-loops, which would otherwise leave you
with a bot that silently answers nothing.

`status` never writes a unit — asking whether it is running must not
install it.

Tokens: `PANTHEON_DISCORD_TOKEN`, `PANTHEON_TELEGRAM_BOT_TOKEN`.
Pairing is allowlist-gated and **required**: see `PANTHEON_GATEWAY_ALLOW`
below.

`pipeline` is where gate resolution lives, not here.

## Pipeline verbs

```
pantheon pipeline --spec "task" [RUN_ID]
pantheon pipeline RUN_ID --approve plan|review
pantheon pipeline RUN_ID --deny plan|review
```
Without a positional run id one is generated and printed on the park line.
`PANTHEON_PIPELINE_EVAL=1` enables the strict evaluator loop inside
implement.

## Scheduler verbs

```
pantheon schedule <task> [--every 30m | --cron "*/5 * * * *"] [--agent NAME]
                  [--model M] [--provider P]
pantheon schedule list|pause|resume|cancel|run <id>
pantheon schedule tick [--watch]
```
Schedules are durable (claim ledger) and the scheduler supports four
trigger kinds: `--every` (interval), `--cron`, one-shot, and webhook. The
CLI can create interval and cron jobs only; one-shot and webhook jobs exist
in the scheduler and fire correctly, but nothing in the CLI authors them
today. Per-job `--model`/`--provider` pins persist and validate, and live
run driving honors them at fire time.

## Swarm verbs

```
pantheon swarm N "task" [roles...] [--delivery X] [--roles a,b]
pantheon swarm status [<id>]
pantheon swarm list
```
Spawns are capped at runtime (depth, concurrency, token/tool budgets,
model restrictions) — over-cap spawns fail with `SWARM_SPAWN_DENIED`.

## Skills verbs

```
pantheon skills list [--scope ...]
pantheon skills import <name> | --url URL | --repo URL [--sub DIR]
                     | --clawhub SLUG [--owner OWNER] | --hermes [--scope S]
pantheon skills doctor
```
Discovery covers pantheon + project `.agents`/`.claude`/`.pantheon` roots,
`~/.hermes`, `~/.openclaw`, and `PANTHEON_SKILLS_DIR`. Import is the only
writer, landing verbatim `SKILL.md` files under `<data_dir>/skills`.

## MCP verbs

```
pantheon mcp list [--json]
```
Read-only report of the MCP servers a migration declared and whether
they can register. No launcher yet.

## Session verbs

`pantheon` (bare) opens the terminal interface. It needs a terminal on stdin
and stdout; with neither, it says so and exits 1 rather than falling back to a
second, line-based interface. For a non-interactive turn use `pantheon run
--taskID <id> --say "text"`.

`pantheon --resume [id]` enters the session on a specific run (unknown
ids fail loudly); without an id it resumes the most recent run.

The command surface is `/help /runs /history /resume /name /clear /exit
/quit`.

## Environment variables

| Variable | Effect |
|---|---|
| `PANTHEON_DATA_DIR` | Data directory (default `~/.pantheon`) |
| `PANTHEON_LOG_LEVEL` | Log threshold: `debug`, `info` (default), `warning`, `error` |
| `PANTHEON_EXT_DIR` | Extension directory (default `<data>/extensions`) |
| `PANTHEON_PROVIDER` / `PANTHEON_MODEL` | Default model when no config/flags |
| `PANTHEON_API_KEY` | API key fallback (config names better ones) |
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
| `PANTHEON_GATEWAY_ALLOW` | **Required** for `pantheon gateway`: allowlisted chat/user ids |
| `PANTHEON_GATEWAY_POLICY` | Gateway policy override |
| `PANTHEON_SKILLS_DIR` | Extra skill-discovery root |
| `PANTHEON_MEMORY_FILE` | Override the memory file path |
| `PANTHEON_MEMORY_HTTP_URL` / `PANTHEON_MEMORY_HTTP_KEY` | Bridge endpoint for `memory backend select` |
| `PANTHEON_VAULT_DIR` | Encrypted vault root |
| `PANTHEON_CATALOG` | Extra provider-catalog file |
| `PANTHEON_BASE_<PROVIDER>` | Base-URL override per provider |
| `PANTHEON_HTTP_TIMEOUT_MS` | Provider HTTP timeout (default 120000) |
| `PANTHEON_PLUGIN_TIMEOUT_SECS` | Plugin subprocess timeout |
| `PANTHEON_HOOK_STREAM_DELTA` | `=1` fires `on_stream_delta` per token |
| `PANTHEON_SERVE_TOKEN` | Bearer token for `pantheon serve` |
| `PANTHEON_KEY_<NAME>` | Stacked keys (`k1,k2` rotate on 401/403/429) |
| `PANTHEON_<AUX>_PROVIDER` / `PANTHEON_<AUX>_MODEL` | Aux overrides; AUX is JUDGE, COMPRESSION, TITLEGEN, EMBEDDINGS, SEARCH_SYNTHESIS, VISION, SCHEDULED, MCP_SYNTHESIS |
