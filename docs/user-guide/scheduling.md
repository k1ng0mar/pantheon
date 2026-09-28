# Scheduling

Scheduled tasks are not managed by the system crontab. Hermes-style, Pantheon runs its own internal scheduler inside the background gateway service: `pantheon schedule` registers jobs, the gateway's scheduler loop fires them, and `pantheon schedule tick [--watch]` remains the manual/CI primitive that fires due jobs directly.

## Background service

`pantheon init` installs and starts the user-scope gateway silently — never prompts, always safe to call. The mechanism is picked per platform:

| Platform | Mechanism |
|---|---|
| Linux + systemd | user unit in `~/.config/systemd/user`, `systemctl --user enable --now` |
| Linux without systemd | `@reboot pantheon gateway run` merged idempotently into the user crontab |
| macOS | LaunchAgent in `~/Library/LaunchAgents`, `launchctl bootstrap gui/<uid>` |
| Windows | user-scope Task Scheduler logon task via `schtasks` (no admin) |
| None available | fails open with a one-line manual `pantheon schedule tick` cron hint |

`pantheon gateway status` reports which mechanism is in use, plus the scheduler queue (active jobs, due now, next fire). The scheduler loop starts unconditionally: a scheduler-only install with no channel tokens is a working always-on service. Chat surfaces are the allowlist-guarded front door and simply stay disabled until `PANTHEON_TELEGRAM_BOT_TOKEN`/`PANTHEON_DISCORD_TOKEN` and `PANTHEON_GATEWAY_ALLOW` are set — `gateway run` prints a one-line warning and keeps ticking. `pantheon gateway restart` on a cron `@reboot` install is an honest no-op (the entry only fires at boot); run `pantheon gateway run` in the foreground to pick up changes immediately.

## Delivery

A fired job's result goes somewhere user-facing via `--deliver`:

```sh
pantheon schedule "summarize inbox" --every 2h --deliver telegram
```

Targets: `log` (default — result stays in the ledger), `telegram`, `discord`, `notify` (desktop notification: notify-send on Linux, osascript on macOS, PowerShell toast on Windows), `file:<path>` (appended). The summary is the run's final assistant message, redacted and truncated to ~2000 chars, sent through the gateway's existing channel senders. Telegram needs `PANTHEON_TELEGRAM_BOT_TOKEN` + `PANTHEON_DELIVER_TELEGRAM_TO` (chat id); Discord needs `PANTHEON_DISCORD_TOKEN` + `PANTHEON_DELIVER_DISCORD_TO` (channel id). Delivery failure never fails the job — it's a log line.

## Templates

Built-in blueprints — `pantheon schedule template list`:

`morning-briefing` · `inbox-triage` · `repo-watch` · `dep-audit` · `weekly-review` · `cost-report` · `gmail-monitor` · `cost-watch`

```sh
pantheon schedule create --template morning-briefing --var topic="AI agents" --deliver telegram
```

Each template has a default schedule, a prompt with `{{variable}}` placeholders, and the questions to fill them. Missing vars are prompted interactively on a TTY, otherwise an error. `--var model=<id>` (and `--var provider=<p>`) on any template pins that job's model instead of substituting into the prompt. Add your own in `<data_dir>/templates/*.toml` — same name as a built-in replaces it.

## Model rule

Scheduled work is background work and burns cheap tokens by default. Model resolution, in order:

1. **Explicit pin** — `--model`/`--provider` on `schedule create`, or the template's `model` var. Always wins.
2. **`[scheduled]` auxiliary** — the `[scheduled]` config section (or `PANTHEON_SCHEDULED_PROVIDER` / `PANTHEON_SCHEDULED_MODEL`). The default for unpinned jobs.
3. **Never the interactive default** — unless the `[scheduled]` slot itself resolves to it (`auto` with nothing configured).

## See also

- [Runs](runs.md) — scheduling basics, ticks, atomic claims
- [Providers](providers.md) — auxiliary model slots
