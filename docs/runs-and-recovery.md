# Runs, approvals, and recovery

How work is represented, parked, resumed, and survived.

## Run lifecycle

A run is any unit of agent work: one chat turn, a tool loop, a pipeline.
Every transition is an event in the ledger (SQLite, `ledger.db`):

```
RunStarted -> ModelRequested -> [ToolStarted -> ToolOutput -> ToolCompleted]*
          -> ModelCompleted -> RunCompleted
                        \-> RunFailed | RunCanceled
```

The run status (shown by `pantheon logs <id>`, or `/status` in-session) is derived state: `running`,
`awaiting_approval`, `completed`, `failed`, `canceled`, or `unknown`.
Terminal statuses are immutable: a `RunFailed` after `RunCompleted` does
not overwrite the completed status. The events still append (the ledger is
append-only), only the derived status is protected.

## Approvals

Tools declare the capability they need. The policy decides:

- **Allow**: executes immediately.
- **Approval**: the run parks. `ApprovalRequested { scope }` is recorded,
  status becomes `awaiting_approval`, and the process can exit safely.
- **Deny** (policy): the tool never runs; the model sees a structured
  denial and can adapt.

Resolving a parked run:

```sh
pantheon run --id run_abc --grant call_1_0   # approve, then continue the run
pantheon run --id run_abc --deny call_1_0    # refuse; the call settles as denied
```

`grant` continues the run it unblocked, so the granted tool call actually
executes and the model gets its real result. Pass `--no-resume` to only
record the permission and leave the run parked.

A call parked on approval emits no `ToolStarted` event, so it is invisible
to crash recovery, which only sees started-but-unfinished calls. Grant-resume
therefore treats granted-but-unexecuted calls as pending too. Without that,
the model would see a tool call with no result and answer from imagination.

The `git.push` capability is reached through the `shell` tool, not a
dedicated push tool: a shell call whose command is a git push picks up
`GitPush` and parks like any other approval.

Denial semantics (important): a denied call does not fail the run. On
resume, the transcript receives a tool result reading "denied by operator:
this tool call was rejected and was not executed", and the turn continues.
The model can pick a different approach or finish. This is why deny exists
as its own verb rather than being a synonym for kill.

A scope can only be resolved once (`RT_APPROVAL_RESOLVED` on repeat).
Granting a scope that was never requested fails (`RT_APPROVAL_UNKNOWN`).

## Crash recovery

Kill the process mid-run (crash, OOM, Ctrl-C on the host) and the ledger
holds everything up to the last event. On the next `chat` with the same
run id:

- Tool calls that completed are skipped (their results are in the ledger).
- Tool calls that were granted but crashed before executing re-execute
  from their persisted name+arguments.
- Tool calls with no persisted record get a fabricated error result so
  the transcript stays provider-valid.
- The run status moves through `RunRecovered` and continues.

There is no "hope the model remembers": the transcript is rebuilt from
persisted message rows.

## Run leases

A lease is a CAS row (`run_leases` table: run_id, lease_id,
lease_until_ms, heartbeat_ms) proving one supervisor owns a run.

- Acquire: `BEGIN IMMEDIATE` transaction; a second supervisor loses with
  `RT_LEASE_BUSY`.
- Renew: conditional UPDATE on `lease_id AND lease_until_ms > now`; a
  heartbeat thread renews at TTL/3.
- Takeover: an expired lease can be acquired by anyone. This is the
  recovery path for a dead supervisor.
- Loss: `LOST_LEASE` errors stop tool work. Doing work under a lost lease
  risks double-execution against the new owner.

Process groups (plugin processes) are registered against the lease. A
supervisor that lost its lease refuses to killpg the group: the PGID may
have been reused by the replacement. Kill authority follows the lease,
never the PID number.

## Cancellation

```sh
# via RPC: agui.cancel {run_id}
# or programmatically: supervisor.cancel_run(run_id, reason)
```

Two phases, deliberately split:

1. **Intent** (fast, synchronous): `RunCanceled` event + every linked
   operation moves to `canceling`. The run stops taking new work.
2. **Termination** (slow, off-thread): process groups owned by the lease
   get TERM, a 5s grace, then KILL; operations settle to `canceled`.

The heartbeat thread in the lease holder also watches for cancellation
and performs phase 2 itself, so a cancel request from another supervisor
is honored without that supervisor ever signaling a process group.

## Watchdog

The turn watchdog is activity-based, not duration-based. Silence past the
stall budget (`PANTHEON_STALL_BUDGET_MS`, default 30s) triggers a
liveness probe (a ledger status read); only a failed probe kills, and a
human pause (awaiting approval) never eats the clock. What it catches:
wedged runtimes, dead processes. What it does not catch: a slow provider
response (the HTTP transport's own timeout covers that, default 120s).

## Budgets

Two caps, runtime-owned, never agent-chosen:

- `max_turns` (default 16): model turns per run.
- `max_tool_calls` (default 32): tool executions across the WHOLE run,
  not per turn.

Hitting either returns `BUDGET_EXHAUSTED` (turns) or ends the loop with
`BudgetExhausted { cap }` (tool calls). A batch of calls that would
overshoot is rejected before any of it executes.
