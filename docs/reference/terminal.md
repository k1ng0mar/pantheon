# Terminal reference

Every `pantheon` verb, its flags, and exit codes. Anything not listed here does not exist, typos exit 2 with a suggestion.

## Conventions

- Exit 0 = success. Exit 1 = operation failed (the message names a structured code like `SAFE_STALE`). Exit 2 = usage error.
- Flags are `--flag value` or `--flag=value`. Boolean flags (`--yes`) take no value.
- `PANTHEON_DATA_DIR` overrides the data dir (default `~/.pantheon`) for every verb.
- JSON on stdout means the command is for scripts: `doctor`, `audit`, `mcp list --json`, `repair --json`. Every other verb prints human-oriented text.

## Start

```sh
pantheon                      # open the interactive session (the terminal interface)
pantheon --resume [id]        # enter the session on a run (unknown ids fail loudly)
pantheon --help | --version
```

## Talk to it

```sh
pantheon run --taskID <id> --say "text" [--deliver session|telegram|discord]
pantheon run [--id ID] [--say TEXT] [--tool NAME] [--fail CODE] [--ext] [--platform P]
pantheon run --taskID <id> --grant <scope> [--no-resume]
pantheon run --taskID <id> --deny  <scope> [--no-resume]
```

With `--deliver` (default `session`): a real model turn, printed here or queued for the gateway. Without it: synthetic ledger events only (`--say` records a line, `--fail CODE` ends the run, `--tool` names a tool, `--ext` fires hooks), for recovery/ledger testing, never a model call.

## Inspect a run

```sh
pantheon runs                          # every run, status and title
pantheon runs <run_id> [--metrics]     # event trace, or one line of counters
pantheon audit <run_id> [OUT.jsonl]    # sequence-validated JSONL trajectory
pantheon logs [agent|errors|gateway] [-n N] [-f] [--level LVL] [--since DUR] [--grep RE]
```

## Set up

```sh
pantheon setup [--yes] [--profile P] [--provider P] [--model M]
               [--api-key-env ENV] [--key SECRET]
               [--policy reader|coder|coder_memory] [--memory BACKEND]
               [--fallback-provider P] [--fallback-model M]
pantheon update [--check] [--version TAG] [--repo OWNER/REPO]
pantheon model [--list] [--auxiliary KIND]     # provider picker, keys -> .env
pantheon provider <add|list|remove>            # custom-endpoint registry
pantheon provider models <name>                # live /models fetch
pantheon providers                             # catalog listing
pantheon fallback <add|list|remove>            # ordered fallback chain
pantheon doctor [<plugin_dir>]                 # system preflight, or per-plugin
pantheon repair [--dry-run] [--json]           # fix what can be fixed safely
pantheon reset [--config|--state|--everything] [--yes]  # typed confirmation unless --yes
```

`doctor` diagnoses and changes nothing; `repair` is the fixing half. `reset --state` refuses while a run lease is live.

## Memory

```sh
pantheon memory import [FILE] | export [FILE] | sync [FILE]   # default MEMORY.md
pantheon memory list | recall QUERY | put KEY VALUE | confirm KEY
pantheon memory backend list | select NAME [k=v ...] | scaffold NAME [http|stdio]
pantheon memory vault search QUERY | read PATH | list [CATEGORY]
```

`recall --ns NAME` (or `--ns '*'`) reads another agent's namespace. `sync` refuses on conflict.

## Extend

```sh
pantheon skills list|import <name>|doctor
pantheon plugins list|install <name>|enable <name>|disable <name>
pantheon extensions                       # what actually loaded
pantheon hook <name> [--session S] [--platform P]
pantheon mcp list [--json]                # read-only: migration-declared servers
pantheon migrate <detect|show|plan|apply|validate> <hermes|openclaw|omp> [path]
               [--kind K] [--json] [--yes] [--merge-providers]
```

`apply` is the only writer (`backup → apply → validate`, needs `--yes` or typed source name).

## Run unattended

```sh
pantheon schedule <task> [--every 30m | --cron "*/5 * * * *"] [--agent N] [--model M] [--provider P]
               [--timeout 10m] [--overlap skip|replace|queue]
pantheon schedule list|pause|resume|cancel|run <id>
pantheon schedule tick [--watch]      # advance the scheduler manually; --watch loops
pantheon swarm status [<id>] | list   # recorded swarms only; the swarm verb doesn't spawn
# (sub-agent work happens through in-session delegation, the engine's
# Delegate arm, not through the swarm verb; spawn caps still apply)
pantheon gateway start|restart|stop|status|run [discord|telegram]
pantheon serve [--port N] [--host H]      # AG-UI server
pantheon pipeline --spec "task" [RUN_ID]
pantheon pipeline RUN_ID --approve plan|review | --deny plan|review
```

## Environment variables

| Variable | Effect |
|---|---|
| `PANTHEON_DATA_DIR` | Data directory (default `~/.pantheon`) |
| `PANTHEON_REPO` | Release repo for installer/`update` |
| `PANTHEON_VERSION` | Pin installer version |
| `PANTHEON_LOG_LEVEL` | `debug`/`info` (default)/`warning`/`error` |
| `PANTHEON_PROVIDER` / `PANTHEON_MODEL` | Default model when no config/flags |
| `PANTHEON_REASONING` | Reasoning effort: `off` (default), `minimal`, `low`, `medium`, `high`, `xhigh`, `max` |
| `PANTHEON_API_KEY` | API key fallback |
| `PANTHEON_DISCORD_TOKEN` / `PANTHEON_TELEGRAM_BOT_TOKEN` | Gateway tokens |
| `PANTHEON_GATEWAY_ALLOW` | **Required** for gateway: allowlisted chat/user ids |
| `PANTHEON_SERVE_TOKEN` | Bearer token for `serve` |
| `PANTHEON_MEMORY_NAMESPACE` | Namespace for memory verbs (default `nyx`) |
| `PANTHEON_EXT_DIR` / `PANTHEON_SKILLS_DIR` | Extension dir / extra skill root |
| `PANTHEON_HTTP_TIMEOUT_MS` | Provider HTTP timeout (default 120000) |
| `PANTHEON_STALL_BUDGET_MS` | Watchdog stall budget (default 30000) |
| `PANTHEON_RUN_LEASE_TTL_MS` | Run lease TTL (default 30000) |
| `PANTHEON_PIPELINE_EVAL` | `=1` enables the strict pipeline evaluator |
| `PANTHEON_KEY_<NAME>` | Stacked keys (`k1,k2` rotate on 401/403/429) |
| `PANTHEON_<AUX>_PROVIDER` / `PANTHEON_<AUX>_MODEL` | Aux overrides (JUDGE, COMPRESSION, TITLEGEN, EMBEDDINGS, SEARCH_SYNTHESIS, VISION, SCHEDULED, MCP_SYNTHESIS) |
