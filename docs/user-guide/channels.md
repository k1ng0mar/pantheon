# Channels

One assistant, every screen. Your identity, memory, and history follow you from the terminal to your phone to the browser. The interface changes; the assistant does not.

## Terminal

`pantheon` opens the app; `pantheon run` handles scripts and automation. See [Sessions](sessions.md).

## Web

```sh
pantheon serve [--port N] [--host H]
```

A local web page on your machine, plus an API other apps can talk to. It runs the same assistant as the terminal and shows the same conversations. Access is locked behind a token (`PANTHEON_SERVE_TOKEN`).

## Messaging

Discord and Telegram connect through the gateway:

```sh
pantheon gateway start    # start the background service
pantheon gateway restart|stop|status|run
```

Two things are required, no exceptions: your chat app tokens (put them in `<data_dir>/.env`, never in the config file) and a list of who is allowed to talk to it (`PANTHEON_GATEWAY_ALLOW`). Strangers are turned away before anything reaches the assistant.

Replies go out with `pantheon run --say ... --deliver telegram|discord`. Outgoing messages are saved first, so they get delivered even if the gateway was down when they were queued.

## See also

- [Sessions](sessions.md): the terminal app
- [Configuration](../reference/configuration.md#gateway): tokens and the allowlist
- [Troubleshooting](../reference/troubleshooting.md): connection problems
