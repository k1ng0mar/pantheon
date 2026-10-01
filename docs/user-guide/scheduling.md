# Scheduling

Pantheon can do things on its own, on a timer. Jobs are registered with `pantheon schedule`; the background service fires them; each firing runs as your agent, with its memory, tools, and permissions.

## Background service

`pantheon init` installs and starts the background service for your user account. It picks the right mechanism for your machine:

| Platform | How it stays running |
|---|---|
| Linux with systemd | a user service, started automatically |
| Linux without systemd | a `@reboot` entry in your crontab |
| macOS | a LaunchAgent |
| Windows | a Task Scheduler logon task (no admin needed) |

It never asks questions and is always safe to run again. `pantheon gateway status` tells you which mechanism is in use and what jobs are queued. The scheduler keeps ticking even with no chat apps connected; messaging just stays off until you add tokens and the allowlist.

You can also fire due jobs by hand, which is what CI and manual setups use:

```sh
pantheon schedule tick [--watch]   # --watch keeps it running in the foreground
pantheon schedule run <job-id>     # run one job now, outside its schedule
```

A successful `schedule run` removes a one-shot job — it has fired its single time. If the run fails, the one-shot stays so you can retry it; recurring jobs are never removed this way, they just record the run.

## Where results go

A finished job sends its result somewhere you will see it, with `--deliver`:

```sh
pantheon schedule "summarize inbox" --every 2h --deliver telegram
```

Targets: `log` (default, the result just stays in the conversation history), `telegram`, `discord`, `notify` (a desktop notification), `file:<path>` (appended to a file). The summary is the job's final message, trimmed to about 2000 characters. Telegram needs a bot token and a chat id; Discord needs a bot token and a channel id. If delivery fails, the job still counts as done; the failure is logged.

## Templates

Ready-made job blueprints, `pantheon schedule template list`:

`morning-briefing` · `inbox-triage` · `repo-watch` · `dep-audit` · `weekly-review` · `cost-report` · `gmail-monitor` · `cost-watch`

```sh
pantheon schedule create --template morning-briefing --var topic="AI agents" --deliver telegram
```

Each template has a default schedule and a prompt with `{{variable}}` placeholders. Missing values are asked for interactively, or error out without a terminal. Manage your own with `pantheon schedule template save --name my-watch --every 1h --prompt "Check {{thing}} and report back." --var thing:"What should I watch?":"the build"`; they live in `<data_dir>/templates.json` and are shared by every client. Same name as a built-in overrides it — and `pantheon schedule template delete <name>` on an overridden built-in removes your copy and reveals the built-in again. Deleting a built-in you never overrode is an error.

A job's template can be swapped later: the dashboard's `PUT /api/schedule/jobs/<id>` accepts `template` (a name, or `null` to clear it) plus `template_vars` (`vars` works too). Reassigning validates like creation — the template must exist, missing vars are rejected, defaults fill gaps, and the task text is re-rendered from the new template. An explicit `task` in the same request still wins over the rendered text.

## Which model runs jobs

Background work uses cheap models by default. The order Pantheon checks:

1. **Explicit pin**: `--model`/`--provider` on `schedule create`, or the template's `model` var. Always wins.
2. **`[scheduled]` setting**: the `[scheduled]` section in your config, or the `PANTHEON_SCHEDULED_PROVIDER` / `PANTHEON_SCHEDULED_MODEL` env vars. The default for unpinned jobs.
3. **Never your main chat model**, unless you set it explicitly in step 1 or 2.

## See also

- [Runs](runs.md): scheduling basics
- [Providers](providers.md): model settings
