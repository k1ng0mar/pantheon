# Troubleshooting

Every error is a structured `PantheonError`: code, layer, retryable flag,
cause, remediation. This page maps the codes you will actually meet to
what to do.

## Model / provider

| Code | Meaning | Fix |
|---|---|---|
| `PROVIDER_EXHAUSTED` | Default and all fallbacks failed | Check the first failure's cause above it in the log; usually key, base URL, or network |
| `MOCK_PROVIDER_UNCONFIGURED` | `--provider mock` without `PANTHEON_MOCK_FILE` | Point `PANTHEON_MOCK_FILE` at a fixture JSON (shape in eval/cases.json) |
| `MOCK_MISMATCH` | Fixture has no response matching the turn | Add a `{"match": "...", "content": "..."}` entry |
| `MODEL_HTTP_*` | Transport error from the provider | The cause names the HTTP status; 401/403 = key, 429 = quota, 5xx = provider side |

## Run lifecycle

| Code | Meaning | Fix |
|---|---|---|
| `RUN_PARKED` | Run is awaiting an approval | `pantheon grant <run> <scope>` or deny |
| `RUN_TERMINAL` | Run already completed/failed/canceled | Start a new run id |
| `RT_LEASE_BUSY` | Another supervisor owns the run | Wait for it, or let the lease expire (TTL 30s) |
| `LOST_LEASE` | This supervisor lost ownership mid-work | Stop tool work; reacquire; the run is recoverable |
| `BUDGET_EXHAUSTED` | max_turns or max_tool_calls hit | Raise the budget in code or simplify the task |
| `WATCHDOG_KILL` | Stall probe failed after the silence budget | Check the provider transport; the run stays recoverable |
| `SWARM_SPAWN_DENIED` | Delegation not configured in this session | Expected in v1; spawn caps exist but the spawner is not wired |

## Approvals

| Code | Meaning | Fix |
|---|---|---|
| `RT_APPROVAL_UNKNOWN` | Scope was never requested | Copy the scope from the ApprovalRequested event |
| `RT_APPROVAL_RESOLVED` | Scope already granted/denied | Nothing to do |
| `RT_NOT_PARKED` | Run is not in awaiting_approval | Check `pantheon status` first |

## Tools and plugins

| Code | Meaning | Fix |
|---|---|---|
| `CAP_DENIED` | Policy denies the capability | Change the policy preset or the tool's capability |
| `TOOL_PANIC` | Tool implementation panicked | Bug in the tool; the run continues with the error as result |
| `PLUGIN_TIMEOUT` | Plugin missed the per-call deadline | The group was killed; fix the plugin's latency or raise timeout_ms |
| `PLUGIN_DEAD` / `PLUGIN_EOF` | Plugin process died or closed stdout | Run it manually to see the crash |
| `PLUGIN_PROTOCOL` | Invalid JSON or wrong call_id | Plugin's stdio contract is broken; one JSON line in/out |
| `TOOL_ARGS_TOO_LARGE` | (planned) oversized arguments | Reserved by the args-size cap decision |

## Files and storage

| Code | Meaning | Fix |
|---|---|---|
| `SAFE_STALE` | File changed since your expected hash | Re-preview; the change was not applied |
| `LEDGER_OPEN` / `LEDGER_*` | SQLite errors | Disk space, permissions, or a corrupted file (restore from backup) |
| `ARTIFACT_TOO_LARGE` | Blob over 8MiB | Store big files on disk, put a reference in the ledger |
| `ARTIFACT_ID` / `ARTIFACT_MIME` | Unsafe task id or header-hostile mime | Use `[A-Za-z0-9_-]` ids |

## Config and setup

| Code | Meaning | Fix |
|---|---|---|
| `CONFIG_OPEN` | No config file | `pantheon setup` |
| `CONFIG_PARSE` | Malformed TOML | The error names line and column; or rerun setup |
| `CONFIG_WRITE` | Could not persist config | Check directory permissions |

## Pipelines

| Code | Meaning | Fix |
|---|---|---|
| `PIPELINE_GATE` | Parked on plan or review | `pantheon pipeline <run> --approve <stage>` |
| `PIPELINE_DENIED` | A gate was denied | Pipeline stops by design; start a new run |
| `PIPELINE_EVAL_LOOP` | implement did not converge | Raise max_iterations, fix the task, or disable the strict evaluator |
| `PIPELINE_STATE` | Stage operation has no output | Inspect the operation row; likely an executor bug |
| `OPERATION_CONFLICT` | Version CAS failed | Another worker moved the operation; re-read and retry |
| `OPERATION_IDENTITY` | Operation id reused for a different request | Use a fresh operation id |

## Channels

| Code | Meaning | Fix |
|---|---|---|
| `TELEGRAM_HTTP` / `DISCORD_HTTP` | Platform API error | Token validity, network; the daemon backs off and retries |
| `DISCORD_EVENT` | Malformed gateway payload | Bridge bug; check the normalizer's expectations |

## Generic debugging order

1. `pantheon doctor` — most "it doesn't work" is config or env.
2. `pantheon explain <run_id>` — the answer to "why did it do that" is in
   the event replay.
3. `cargo test --workspace && python3 eval/run.py` — if these are green,
   the bug is in your config or environment, not the build.
