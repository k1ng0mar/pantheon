# Sessions

The terminal interface is how you work with Pantheon directly. Everything you do lands in the ledger, so conversations survive exits, crashes, and channel switches.

## Open a session

```sh
pantheon
```

Bare `pantheon` opens the interface. It requires a terminal; without one it exits and points you at `pantheon run`. Your last run resumes automatically; `/help` lists every command.

Useful commands: `/models` (browse providers and models, Enter switches), `/model [provider id]` (show or switch the live model), `/reasoning [off|minimal|low|medium|high|xhigh|max]` (effort for chat turns), `/runs` (browse conversations), `/resume <id>` (jump to one), `/history`, `/status`, `/name <title>`, `/agent [name]`, `/agents`, `/remember KEY TEXT` (store agent memory), `/skills [filter]`, `/settings`, `/gateway`, `/doctor`, `/sessions` (live sessions), `/new`, `/compress`, `/export [markdown|json]`, `/clear`, `/exit`.

Start on a specific run from the shell: `pantheon --resume [id]`.

## Approve work

When a tool call needs a human, the run parks and shows a permission card: `y` allows it, `n` denies it. Approval covers that one operation — never a blank check for future calls.

Away from the terminal? Settle it out of band:

```sh
pantheon run --taskID <id> --grant <scope>   # allow, then continue the run
pantheon run --taskID <id> --deny  <scope>   # refuse; recorded in the transcript
```

A denial doesn't kill the run — the model sees "denied by operator" and adapts.

## Run without a terminal

```sh
pantheon run --taskID <id> --say "text" --deliver session|telegram|discord
```

A real model turn, delivered where you ask: printed here, or queued for the gateway to send. The queue is durable, so a delivered task survives gateway downtime.

## Look back

```sh
pantheon runs                 # all runs, with status and title
pantheon runs <id>            # the full event trace, in words
pantheon runs <id> --metrics  # counters: turns, tools, approvals
pantheon audit <id> [out]     # sequence-checked JSONL for scripts
pantheon logs [errors] [-f]   # process logs, including pre-run failures
```

`runs` answers "why did that turn end that way" (approvals included). `logs` answers "what has the process been doing". States: `running`, `awaiting_approval`, `completed`, `failed`, `canceled`.

## See also

- [Runs](runs.md) — lifecycle, recovery, pipelines, scheduling
- [Channels](channels.md) — messaging apps and the web client
- [CLI reference](../reference/cli.md) — every flag and exit code
