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

### Multiple bots, multiple agents

One gateway can run several bots on the same platform, each served by a different agent. Declare a `[gateway.channels.<name>]` entry per bot with its `platform`, its token, and its `profile`:

```toml
[agents.support]
display_name = "Support"
policy = "reader"
soul_file = "agents/support/SOUL.md"

[agents.coder]
display_name = "Coder"
policy = "coder"
soul_file = "agents/coder/SOUL.md"

[gateway.channels.support-bot]
platform = "telegram"
token_secret = "SUPPORT_BOT_TOKEN"   # read from <data_dir>/gateway.env (or the process env)
profile = "support"

[gateway.channels.coder-bot]
platform = "telegram"
token_secret = "CODER_BOT_TOKEN"
profile = "coder"
```

Each channel gets its own poller, its own outbound queue, and its own agent runtime: a message on the support bot is answered by the support agent with the support memory namespace, and the coder bot never sees it. Thread ids are namespaced per channel, so two bots' conversations can't collide. Leave `token_secret` out and the channel falls back to the platform's standard token (`PANTHEON_TELEGRAM_BOT_TOKEN`); leave `profile` out and it uses the default profile. Channels that don't declare a `platform` keep the old single-bot behavior exactly. An unknown `profile` stops the gateway at startup with a clear error instead of serving the wrong agent.

Deliver to a specific bot with `pantheon run --say ... --deliver support-bot`.

## Voice API (mobile)

The serve API exposes the voice edge so the mobile app can talk to
the assistant hands-free. Both routes inherit the serve token and
origin checks, and both are gated by the `[tools] voice` group plus the
`[stt]` / `[tts]` config sections (toggle the group off and the routes
400 with `voice_not_configured`).

`POST /agui/voice/transcribe` — audio to text:

```json
{ "audio": "<base64-encoded audio>", "language": "en", "prompt": "..." }
```

`language` and `prompt` are optional hints. Decoded audio is capped at
1 MiB. Response: `{"transcript": "...", "backend": "<provider>"}`.

`POST /agui/voice/speak` — text to speech:

```json
{ "text": "...", "voice": "...", "format": "wav" }
```

`voice` is optional (uses the configured default), `format` is one of
`wav` (default), `mp3`, `ogg`. Text is capped at 32 KiB. The response
body is the raw audio bytes with `audio/wav`, `audio/mpeg`, or
`audio/ogg`.

Error bodies are `{"error": {"code": "...", "message": "..."}}`:

| Status | Code | Meaning |
|---|---|---|
| 400 | `voice_bad_request` | malformed JSON, bad base64, empty audio/text, unknown format |
| 400 | `voice_not_configured` | `[tools] voice` off, or the `[stt]`/`[tts]` section missing |
| 413 | `voice_audio_too_large` / `voice_text_too_large` | over the caps above |
| 500 | `voice_backend_misconfigured` | backend name/options wrong (provider code in the message) |
| 502 | `voice_transcribe_failed` / `voice_tts_failed` | the provider call failed |
| 504 | `voice_timeout` | provider took longer than the request bound |

See also: [Providers](providers.md) for per-backend setup (`run pantheon setup` walks through each tool's providers).

## See also

- [Sessions](sessions.md): the terminal app
- [Configuration](../reference/configuration.md#gateway): tokens and the allowlist
- [Troubleshooting](../reference/troubleshooting.md): connection problems
