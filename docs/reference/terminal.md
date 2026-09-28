# Terminal reference

Every `pantheon` command, its flags, and its exit codes. If it is not listed here, it does not exist; a typo exits with code 2 and a suggestion.

## Conventions

- Exit 0 means it worked. Exit 1 means something failed (the message names an error code like `SAFE_STALE`). Exit 2 means you used it wrong.
- Flags are `--flag value` or `--flag=value`. Boolean flags (`--yes`) take no value.
- `PANTHEON_DATA_DIR` changes the data folder (default `~/.pantheon`) for every command.
- Commands that print JSON are for scripts: `doctor`, `audit`, `mcp list --json`, `repair --json`. Everything else prints for humans.

## Start

```sh
pantheon                      # open the app (needs a real terminal)
pantheon --resume [id]        # open the app on a past conversation
pantheon --help | --version
```

## Talk to it

```sh
pantheon run --taskID <id> --say "text" [--deliver session|telegram|discord]
pantheon run [--id ID] [--say TEXT] [--tool NAME] [--fail CODE] [--ext] [--platform P]
pantheon run --taskID <id> --grant <scope> [--no-resume]
pantheon run --taskID <id> --deny  <scope> [--no-resume]
```

With `--deliver` (default `session`): a real reply from the model, printed here or sent to your phone. Without it: no model call, just a recorded note (`--say` writes a line, `--fail CODE` ends the run), used for testing and recovery.

## Look at past work

```sh
pantheon runs                          # every conversation, status and title
pantheon runs <run_id> [--metrics]     # the full record, or a line of counts
pantheon audit <run_id> [OUT.jsonl]    # machine-readable record for scripts
pantheon logs [agent|errors|gateway] [-n N] [-f] [--level LVL] [--since DUR] [--grep RE]
```

## Set up

```sh
pantheon setup [--yes] [--profile P] [--provider P] [--model M]
               [--api-key-env ENV] [--key SECRET]
               [--policy reader|coder|coder_memory] [--memory BACKEND]
               [--fallback-provider P] [--fallback-model M]
pantheon update [--check] [--version TAG] [--repo OWNER/REPO]
pantheon model [--list] [--auxiliary KIND]     # pick a provider, keys go to .env
pantheon provider <add|list|remove>            # your custom AI endpoints
pantheon provider models <name>                # fetch that endpoint's model list
pantheon providers                             # everything in the catalog
pantheon fallback <add|list|remove>            # backup models, in order
pantheon doctor [<plugin_dir>]                 # health check (changes nothing)
pantheon repair [--dry-run] [--json]           # fix what can be fixed safely
pantheon reset [--config|--state|--everything] [--yes]  # typed confirmation unless --yes
```

`doctor` only diagnoses. `repair` actually fixes things. `reset --state` refuses while a run is in progress.

## Memory

```sh
pantheon memory import [FILE] | export [FILE] | sync [FILE]   # default MEMORY.md
pantheon memory list | recall QUERY | put KEY VALUE | confirm KEY
pantheon memory backend list | select NAME [k=v ...] | scaffold NAME [http|stdio]
pantheon memory vault search QUERY | read PATH | list [CATEGORY]
```

`recall --ns NAME` (or `--ns '*'`) reads another agent's memories. `sync` refuses if there is a conflict.

## Extend

```sh
pantheon skills list|import <name>|doctor
pantheon plugins list|install <name>|enable <name>|disable <name>
pantheon extensions                       # what actually loaded
pantheon hook <name> [--session S] [--platform P]
pantheon mcp list [--json]                # declared MCP servers (read-only for now)
pantheon migrate <detect|show|plan|apply|validate> <hermes|openclaw|omp> [path]
               [--kind K] [--json] [--yes] [--merge-providers]
```

`migrate apply` is the only one that writes (`backup, apply, check`, needs `--yes` or the source name typed out).

## Unattended work

```sh
pantheon schedule <task> [--every 30m | --cron "*/5 * * * *"] [--agent N] [--model M] [--provider P]
               [--timeout 10m] [--overlap skip|replace|queue]
pantheon schedule list|pause|resume|cancel|run <id>
pantheon schedule tick [--watch]      # fire due jobs now; --watch keeps looping
pantheon swarm status [<id>] | list   # past agent collaborations; never starts new work
pantheon gateway start|restart|stop|status|run [discord|telegram]
pantheon serve [--port N] [--host H]      # the local web page + API
pantheon pipeline --spec "task" [RUN_ID]
pantheon pipeline RUN_ID --approve plan|review | --deny plan|review
```

## Environment variables

| Variable | What it does |
|---|---|
| `PANTHEON_DATA_DIR` | Data folder (default `~/.pantheon`) |
| `PANTHEON_REPO` | Release repo for the installer and `update` |
| `PANTHEON_VERSION` | Pin an installer version |
| `PANTHEON_LOG_LEVEL` | `debug` / `info` (default) / `warning` / `error` |
| `PANTHEON_PROVIDER` / `PANTHEON_MODEL` | Default model when nothing else is set |
| `PANTHEON_REASONING` | How hard it thinks: `off` (default), `minimal`, `low`, `medium`, `high`, `xhigh`, `max` |
| `PANTHEON_API_KEY` | API key fallback |
| `PANTHEON_DISCORD_TOKEN` / `PANTHEON_TELEGRAM_BOT_TOKEN` | Chat app tokens |
| `PANTHEON_GATEWAY_ALLOW` | **Required** for the gateway: who may talk to it |
| `PANTHEON_SERVE_TOKEN` | Password for `serve` |
| `PANTHEON_MEMORY_NAMESPACE` | Namespace for memory commands (default `nyx`) |
| `PANTHEON_EXT_DIR` / `PANTHEON_SKILLS_DIR` | Where plugins live / extra skill folder |
| `PANTHEON_HTTP_TIMEOUT_MS` | How long to wait for the AI (default 120000) |
| `PANTHEON_STALL_BUDGET_MS` | Watchdog patience (default 30000) |
| `PANTHEON_RUN_LEASE_TTL_MS` | Run ownership timeout (default 30000) |
| `PANTHEON_PIPELINE_EVAL` | `=1` turns on the strict pipeline checker |
| `PANTHEON_KEY_<NAME>` | Backup keys (`k1,k2` rotate on 401/403/429) |
| `PANTHEON_<AUX>_PROVIDER` / `PANTHEON_<AUX>_MODEL` | Helper model overrides (JUDGE, COMPRESSION, TITLEGEN, EMBEDDINGS, SEARCH_SYNTHESIS, VISION, SCHEDULED, MCP_SYNTHESIS) |
