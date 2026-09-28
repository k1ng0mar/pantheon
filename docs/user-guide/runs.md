# Runs

Every piece of work Pantheon does, a chat reply, a tool call, a scheduled job, is a run. Everything it does is written down event by event, so it can pick up after anything short of losing the disk.

## What happens during a run

Roughly: the run starts, the model is asked, it may use tools, it finishes, the run is marked done, failed, or canceled. A run's status is always derived from what actually happened, never guessed.

## Permissions

Every tool says what it needs; your policy answers allow, deny, or "ask me". Asking parks the run (the program can even exit) until you decide:

```sh
pantheon run --taskID <id> --grant <scope> [--no-resume]
pantheon run --taskID <id> --deny  <scope> [--no-resume]
```

Each permission is decided exactly once. Allowing continues the work with the real result; denying writes "denied by operator" into the conversation and the model adapts.

## Surviving crashes

Kill the process mid-run and everything up to the last recorded event is safe. Start it again with the same run id: finished steps are skipped using their recorded results, interrupted ones run again or get a recorded error, so the conversation stays coherent. Only one supervisor can drive a run at a time, so a second one cannot accidentally do the same work twice.

If something looks stuck, `pantheon repair` tidies up stranded runs and rebuilds indexes, backing up first. `pantheon doctor` only diagnoses.

## Pipelines

For work you want checked at the plan and at the review, like a small assembly line:

```
intake → research → plan → [CHECK] → build → review → [CHECK] → done
```

Each stage feeds the next. Every stage is saved, so a crash resumes mid-pipeline and approved checks are never asked twice. Denying a check stops the pipeline on purpose.

```sh
pantheon pipeline --spec "build a login page" [run_id]
pantheon pipeline <run_id> --approve plan|review | --deny plan|review
```

## Scheduling

Jobs can run on their own, on a timer:

```sh
pantheon schedule "nightly review" --every 24h
pantheon schedule list|pause|resume|cancel|run <id>
```

Each firing runs as your agent, with its memory, tools, and permissions. The full details, background service, where results go, templates, live in [Scheduling](scheduling.md).

## See also

- [Sessions](sessions.md): talking to runs, answering permissions in the terminal
- [Agents](agents.md): whose work this is
- [Terminal reference](../reference/terminal.md): `run`, `runs`, `pipeline`, `schedule`, `swarm`, `repair`
