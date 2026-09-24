# Channels

Three ways to talk to the runtime beyond the CLI: the local AG-UI web
server, Discord, and Telegram. All three consume the same runtime; the
agent never knows which surface sent a message.

## AG-UI (local web)

```sh
pantheon serve --host 127.0.0.1 --port 18789
```

| Route | What |
|---|---|
| `GET /` | Minimal dependency-free web client (send, follow stream, grant/deny buttons, cancel) |
| `GET /agui/stream?run=<id>&thread=<t>` | SSE frame stream |
| `POST /agui/rpc` | JSON-RPC 2.0: agui.send, agui.grant, agui.deny, agui.cancel, agui.frames, agui.sign, agui.artifact.put, agui.serve_hint, system.* |
| `GET /agui/blob/<task_id>?exp=&sig=` | Signed generative-UI artifacts (HMAC, strict task-id alphabet, TTL) |
| `GET /agui/health` | liveness |

Frames are the UI projection of ledger events: text deltas, tool
start/output, approval requests, run completion. `agui.serve_hint` returns
the actual bound addresses so clients never guess ports.

Request bodies are capped at 1 MiB (413 beyond). Artifact bodies are
capped at 8 MiB at the storage layer (`ARTIFACT_TOO_LARGE`).

Signing: `PANTHEON_GENUI_SECRET` (default is a dev-only constant; set a
real one before exposing the port). `pantheon sign <task_id>` mints a URL
without the server running.

Auth: set `PANTHEON_SERVE_TOKEN` and every `/agui` route except
`/agui/health` requires it (HTTP header `Authorization: Bearer <token>`
or `X-Pantheon-Token`; the SSE stream also accepts `?token=` for
EventSource). The served web client gets the token injected
automatically. Without the variable the server is open on localhost,
which is fine for single-user development and nothing else. Any
non-local bind, tunnel, or port-forward requires a token.

## Discord

Two inbound paths, one normalizer:

- **Gateway websocket** (`pantheon gateway`): bot token in
  `PANTHEON_DISCORD_TOKEN`. IDENTIFY/RESUME with session state,
  heartbeats with missed-ACK reconnect, op 7/9 handling. Message Creates
  and button Interactions arrive as the same `ChannelEvent` shape as the
  webhook path.
- **Webhook/bridge**: any external bridge can call
  `DiscordChannel::push_inbound` with a normalized payload.

Outbound: `DiscordRestTransport` posts channel messages; approval frames
carry Grant/Deny buttons whose `custom_id` is `grant:<scope>` /
`deny:<scope>`; clicking one resolves the parked run's approval.

Content is chunked at Discord's 2000-char limit on Unicode character
boundaries.

## Telegram

`PANTHEON_TELEGRAM_BOT_TOKEN`. The daemon long-polls `getUpdates` with a
persisted offset cursor (survives restarts, never replays the window).
Approval frames use inline keyboards with `callback_data`
`grant:<scope>` / `deny:<scope>`. Chunked at 4096 chars.

## The daemon

```sh
PANTHEON_DISCORD_TOKEN=... PANTHEON_TELEGRAM_BOT_TOKEN=... \
PANTHEON_GATEWAY_ALLOW=6123456789,223344556677889900 \
pantheon gateway
```

Access control: the gateway runs whatever a message says on this
machine, so it refuses to start without `PANTHEON_GATEWAY_ALLOW`, the
comma-separated list of platform user ids (Telegram user id, Discord
user id) allowed to talk to the bot. Everyone else gets a refusal and
nothing reaches the runtime. `PANTHEON_GATEWAY_POLICY` overrides the
session policy per deployment (`researcher` for read-only surfaces).

One thread per surface. Each tick:

1. Telegram long-poll (25s server-side wait) through the transport trait.
2. Drain webhook-fed inboxes (Discord bridge path).
3. Route events to the runtime sink: a plain message maps the
   conversation thread to a run id (one run per conversation, remembered)
   and starts/resumes chat; an approval click resolves the scope.
4. Deliver outbound replies to the channel.

Errors never kill the daemon: a failed poll backs off (1s doubling, 60s
cap) and retries. `Ctrl+C` stops cleanly.

Conversation -> run mapping is in-memory per process. Restarting the
daemon starts new run ids for the same chat threads; the old runs remain
in the ledger.

## Known limitations

- Rate limits (HTTP 429) surface as generic transport errors; the backoff
  curve exists but is not yet driven by Retry-After.
- One outbound channel is authoritative per daemon (the thread id IS the
  channel address); routing one conversation across two surfaces needs
  the ThreadRunMap plumbed through the CLI gateway command.
- The web client is a smoke-test surface, not a product UI.
