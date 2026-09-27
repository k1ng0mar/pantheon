# Channels

One agent, every surface. Identity, memory, and history follow you from the terminal to your phone to the browser — the interface presents, the runtime remembers.

## Terminal

`pantheon` opens the interactive interface; `pantheon run` serves scripts and automation. See [Sessions](sessions.md).

## Web

```sh
pantheon serve [--port N] [--host H]
```

A local AG-UI server: web client at `/`, RPC at `/agui/rpc`, everything else gated by `PANTHEON_SERVE_TOKEN`. It runs the same Session runtime as the terminal and replays the ledger stream — a minimal reference surface, not a second product.

## Messaging

Discord and Telegram connect through the gateway:

```sh
pantheon gateway start    # supervised service (systemd/launchd), verified active
pantheon gateway restart|stop|status|run
```

Two requirements, both non-negotiable: channel tokens (`PANTHEON_DISCORD_TOKEN`, `PANTHEON_TELEGRAM_BOT_TOKEN` in `<data_dir>/.env`, never in config) and the pairing allowlist (`PANTHEON_GATEWAY_ALLOW`). Unknown senders are refused before anything reaches the runtime.

Replies route back with `pantheon run --say ... --deliver telegram|discord` — a durable outbox, so delivery survives gateway downtime.

## See also

- [Sessions](sessions.md) — the terminal interface
- [Configuration](../reference/configuration.md#gateway) — tokens and allowlist
- [Troubleshooting](../reference/troubleshooting.md) — connection issues
