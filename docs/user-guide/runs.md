# Runs

Every unit of agent work — a chat turn, a tool loop, a pipeline — is a run: persisted event by event in the ledger, resumable after anything short of disk loss.

## Lifecycle

```
RunStarted → ModelRequested → [ToolStarted → ToolOutput → ToolCompleted]*
          → ModelCompleted → RunCompleted | RunFailed | RunCanceled
```

Status is derived, never stored: `running`, `awaiting_approval`, `completed`, `failed`, `canceled`. Terminal states are final — late events still append (the ledger is append-only), but a completed run stays completed.

## Approvals

Each tool declares the capability it needs; policy answers allow, deny, or approval. Approval parks the run — the process may exit — until granted or denied:

```sh
pantheon run --taskID <id> --grant <scope> [--no-resume]
pantheon run --taskID <id> --deny  <scope> [--no-resume]
```

A scope resolves exactly once. Granting resumes the turn with the real result; denying writes "denied by operator" into the transcript and the model adapts.

## Recovery

Kill the process mid-run and the ledger holds everything to the last event. Resume with the same run id: finished calls are skipped from their recorded results, interrupted calls re-execute or receive a recorded error so the transcript stays valid. Ownership is lease-based, so a second supervisor can't double-execute the same work — it waits or takes over an expired lease. Cancel is two-phase (intent, then termination with grace), and budgets (`max_turns`, `max_tool_calls`) bound every run.

If something looks stuck, `pantheon repair` settles stranded runs and rebuilds indexes, backing up first. `doctor` only diagnoses.

## Pipelines

For work you want gated at the plan and the review:

```
intake → research → plan → [GATE] → implement → review → [GATE] → commit
```

Each stage feeds the next; every stage is durable, so a crash resumes mid-pipeline and approved gates never re-ask. Denying a gate stops the pipeline by design.

```sh
pantheon pipeline --spec "build a login page" [run_id]
pantheon pipeline <run_id> --approve plan|review | --deny plan|review
```

## Scheduling

Triggers live in the scheduler; the work runs as the agent — same identity, memory, policy, tools:

```sh
pantheon schedule "nightly review" --every 24h
pantheon schedule list|pause|resume|cancel|run <id>
```

Due jobs fire on a tick: `pantheon schedule tick [--watch]` is the primitive a daemon, cron entry, or CI step calls (`--watch` keeps it running in the foreground). Cron expressions are validated at creation — an invalid or never-firing expression is rejected rather than stored as a job that would silently never run. Occurrences are claimed atomically in the ledger, so a restart never double-fires a run.

## See also

- [Sessions](sessions.md) — interacting with runs, approvals in the terminal
- [Agents](agents.md) — whose work this is
- [Terminal reference](../reference/terminal.md) — `run`, `runs`, `pipeline`, `schedule`, `swarm`, `repair`
