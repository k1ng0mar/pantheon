# Group C review (api, gateway, cli, small crates) — Nyx hand review

## pantheon-api

**[high] serve.rs:285 — unbounded Content-Length allocation**
`let mut body = vec![0u8; content_len]` with content_len straight from the
request header. A request with `Content-Length: 999999999999` allocates (or
aborts on OOM) before any size check. Local-server threat model softens it,
but the server also binds 0.0.0.0 when configured. Fix: reject
content_len > 1MiB with 413 before allocating. Two lines.

**[medium] serve.rs:255 — try_clone().unwrap() on the stream**
A failure to clone the socket panics the handler thread. Rare (fd
exhaustion), but a panic in one thread with thread-per-connection is a
process kill only if it propagates; it unwinds the thread silently,
dropping the connection. Still: return instead of unwrap.

**[medium] agui.rs — every RPC method opens a fresh Supervisor**
`sup_for(&dir)?` appears 12 times; each call opens SQLite (3 connections
now: ledger, operations, leases), runs schema migration, then drops it. Per
request. For a local single-user server this is milliseconds, but it is
also the reason serve needs no connection pooling. Acceptable for v1;
document it, and if the server ever goes multi-user, introduce a
SupervisorPool. Do not fix now.

**[low] serve.rs handle_stream — SSE loop polls ledger on a fixed interval**
No backoff on idle runs; a stream left open on a completed run exits via
status check, good, but an awaiting_approval run is polled forever. Bounded
by process lifetime; document the expectation that the web client closes
the stream.

Keep as-is: signed blob URLs with strict task-id alphabet; WEB_UI is
deliberately dependency-free; JSON-RPC dispatcher id matching.

## pantheon-gateway

**[medium] discord_gateway.rs — missed heartbeat ACKs are not fatal**
The loop tracks `acked` but never escalates when an ack never arrives; the
connection only dies on the next socket error. Discord expects a
zombied connection to be torn down after ~2 missed acks. Fix: if
`!acked` and the next heartbeat interval elapses, return Reconnect. Small
change, meaningful reliability.

**[medium] daemon.rs — outbound messages are drained and dropped**
The run loop drains `outbound` and discards (`let _ = &msg;`). The CLI
gateway thread pushes into this same vec but nothing sends it back to the
channels. The loop was built for drain-then-deliver but the deliver half is
missing. This is the most user-visible gap in group C: replies never reach
Discord/Telegram from the daemon. Fix: the daemon needs the Channel list to
be keyed by thread-to-channel routing; minimum viable: for each outbound
message, find a channel whose thread matches and call send(). Requires a
thread->channel map; implement with a simple Vec<(String, Arc<dyn
Channel>)> routing table parameter.

**[low] telegram.rs/dotdiscord.rs — 429 rate limits not handled**
ureq returns 429 as an error string; the delivery planner has backoff logic
but send paths do not consult it on rate-limit responses. Document as known
limitation in the channel docs; real fix wants response status surfacing
through ChannelError.

Keep as-is: chunk_text char-boundary safety, allowlist default-deny,
dedup window, cursor persistence, gateway resume/identity handling.

## pantheon-cli

**[medium] main.rs help() — stale verb inventory**
help() lists ~20 verbs; the binary dispatches ~25 (setup, reset, pipeline,
gateway, memory, plugins, audit, providers, stage, apply, checkpoint,
rollback, sign...). The help is the first thing a new user sees. Fix in the
doc pass: regenerate the list from the actual dispatch arms.

**[medium] flag parsing is ad hoc per verb**
Every CLI module has its own `flag()` helper with the same logic, and verbs
differ in whether flags are `--flag value` or `--flag=value` (only the
former is supported everywhere). Inconsistent UX. Fix: one shared
parse_args helper in a new cli_common module supporting both forms, migrate
mechanically.

**[low] chat --provider mock misleading**
`--provider mock` is not special-cased anywhere; mock mode is
PANTHEON_MOCK_FILE. The eval only passes because run.py sets that env. Fix:
either honor `--provider mock` by requiring PANTHEON_MOCK_FILE with a clear
error when absent, or reject the provider name with "mock is not a network
provider; set PANTHEON_MOCK_FILE". First option is friendlier.

**[low] pipeline gate output does not include the resolved run id when
auto-generated**
When --spec is given without a positional id, the run id is generated and
only appears mid-error-message. Print it as its own line so scripts can
capture it.

Keep as-is: setup wizard flag-only non-interactive mode; scoped reset with
lease guard; system doctor's fail-fast exit codes; JSON output for machine
consumption (doctor, audit).

## Small crates verdict

- pantheon-capability (81): used by agent. Keep.
- pantheon-otel (184): no consumers. It is the documented export format
  (ARCHITECTURE.md §19). Keep as library; wire into serve's metrics endpoint
  later. Document.
- pantheon-mcp (99): zero consumers; small; the token->capability projection
  is exactly right. Keep as library awaiting live MCP servers. Document.
- pantheon-scheduler (1012): zero consumers; substantial and tested; the
  durable claim ledger is real. Keep; wire a CLI verb when live scheduling
  is wanted. Document as forward work.
- pantheon-secrets (997): zero consumers; the broker is the natural upgrade
  path for api_key_env. Keep; wire when the secrets story is next touched.
- pantheon-sandbox (416): zero consumers; profiles are policy values without
  enforcement. Keep the crate; the enforce() seam is where container work
  would land. Document honestly as "policy mapping only".
- pantheon-migrate (323): zero consumers; CLI verb absent. Keep; expose
  `pantheon migrate` when import is needed.
- pantheon-swarm (294): zero consumers; session.rs denies all delegation.
  Keep Caps; wire into the spawner seam when delegation lands.

None of the eight should be deleted in this pass: each maps to a spec
section, carries tests, and deleting forward work to win a build-size
argument is the wrong trade. What they need is honest documentation of
"implemented but not wired" status, which the rewritten ARCHITECTURE.md
gives them.
