# Memory backends: the plane and its doors

Pantheon separates the **memory plane** from the **ways users connect to
it**. The plane is the policy-gated API the runtime sees
(`recalls/writes/confirm` through `pantheon-memory`); the doors are the
backends behind it. This document is the contract those doors inherit —
what setup shows, what runs, what happens when a backend misbehaves, and
what happens to data when you switch.

## Four distinct operations

Provider **discovery**, **selection**, **initialization**, and
**migration** are separate operations, always:

1. **Discovery** — which backends exist. Sources: the built-in registry
   (`BackendRegistry::with_defaults()`), plus user manifests under
   `<data_dir>/memory-plugins/*.toml`. The setup picker lists *exactly*
   what the runtime can load (`memory_providers(data_dir)` uses the same
   `with_plugins` loader), so what setup shows is what a session can
   open. No row exists in setup that the runtime cannot construct.
2. **Selection** — which one is active: `pantheon memory backend select
   NAME [k=v ...]` writes `memory-backend.toml`. The selection file is a
   pointer, nothing else.
3. **Initialization** — constructing the backend at session start
   (`open_selected`). Fails **explicitly and loudly** on
   misconfiguration (missing URL, unconsented stdio command, unreachable
   state); see "Failure semantics".
4. **Migration** — deliberately *not* provided. Switching backends
   never moves data. Each backend owns its own store; the previous
   backend's memories stay with it and come back when it is selected
   again. The `select` verb prints this so "it lost my notes" is never a
   bug report. (Identity is orthogonal: namespaces are per agent and
   independent of the backend.)

## Transports

- **`http`** — any service exposing Pantheon's `/v1/memory` JSON
  protocol (recall GET, write POST, list_agent GET), Bearer key
  optional. Vendor products fronted through a user-run thin bridge.
- **`stdio`** — a subprocess that reads one JSON request line and
  writes one JSON response line per call (`{"op": ...}` in; `{"ok":
  true, ...}` out). The zero-HTTP path for fronting local engines.

Ops: `recall`, `write`, `list_agent`, `get`, `forget`, `confirm`.
Layer tags are the enum names (`Agent`, `Global`, `Project`,
`TaskSession`, `EphemeralTurn`). **No protocol version field** — on the
deliberate grounds that every op is individually specified and unknown
fields are ignored; a breaking change to an op's shape is a new adapter,
not a version negotiation.

## Trust boundary

- **`allow_exec = true` is the stdio consent gate.** A stdio manifest
  names a command runs with your user privileges; without the flag the
  loader rejects the manifest and the backend never registers. An `http`
  manifest runs nothing (a network endpoint), so it needs no consent.
- **Descriptive vs executable config is separated.** `name`, `label`
  are descriptive; `url`, `key`, `prefix`, `command`, `args`,
  `timeout_ms`, `allow_exec` are executable config. Nothing else in a
  manifest is read.
- **No environment injection.** A stdio backend inherits your shell's
  environment; manifests declare no env map, so a plugin cannot ask
  Pantheon to set variables for it.
- **Selection options may override manifest values**
  (`pantheon memory backend select NAME url=... key=...`) so one
  manifest can serve deployments (dev/staging pointers, rotated keys).
  Options come from the selection file — the same operator trust domain
  as the manifest itself, so this does not widen who can command exec.
- **Keys live in the manifest (or selection options), plaintext.**
  Pantheon does not accept secret-store references for bridge keys yet;
  scaffold writes manifests `0600`, and you should treat the file as you
  treat `.env`. The key rides each call as `Authorization: Bearer` to
  the bridge, and is never logged or written into config.toml.
- **Trust laundering is capped.** Records from any non-native backend
  are clamped to the Untrusted tier by default (a compromised backend
  cannot inject `user`-tier text into recall). `with_trust_ceiling`
  exists only for a backend the operator fully controls; it still cannot
  report above the configured ceiling.

## Failure semantics

| Situation | Behavior |
|---|---|
| Session start, backend fails to construct | **Loud degradation:** one stderr line naming backend, error, consequence ("running WITHOUT memory"), and the fix. No memory tools register. Never a silent SQLite fallback — memory either runs or is absent and announced. |
| Backend offline but constructor succeeds (`http`: construction needs no network) | First (and every) call fails with a structured error (`MEM_HTTP_CONN`, ...). Tools exist; calls fail loudly, per call. |
| stdio plugin times out | Child is killed at `timeout_ms`; explicit `MEM_PLUGIN_TIMEOUT`. |
| stdio plugin crashes with no output | Explicit `MEM_PLUGIN_EMPTY` (or IO/DECODE if partial output). |
| Malformed JSON either transport | Explicit decode error (`MEM_HTTP_DECODE` / `MEM_PLUGIN_DECODE`). |
| Gigabyte responses (buggy or hostile backend) | Capped at the shared response limit; explicit `MEM_HTTP_OVERSIZE` / `MEM_PLUGIN_OVERSIZE` instead of unbounded buffering. |
| Missing response fields | Defaults, never fake data: absent `hits`/`rows` → empty; absent `found` → not found; absent `removed` → **false** (a forget is only reported deleted when the backend confirms). |
| Hostile error payloads | Remote `code` fields pass only the allowlist; control characters are stripped and text length-capped (no log forging, no code/log spoofing). |
| Retries | None, on purpose: requests fail with `retryable: false` and a remediation hint. The agent surfaces them as ordinary tool errors. |

The `timeout` on http is one bounded 30 s overall agent timeout; stdio
is `timeout_ms` per call (default 5 s). `pantheon doctor` checks the
native store independently.

## Conformance tests

The behavioral suites pin this contract:

- `eval/tests/memory_plugins.rs` — manifest loading/consent, stdio
  round-trip, malformed JSON, crash, timeout kill, oversize cap,
  missing-field defaults, tier clamping, and the full
  manifest→registry→real-HTTP-service read.
- `eval/tests/memory_http.rs` — decode/conn/oversize/protocol-shape
  errors, plain-text error statuses, write round-trip with the proposal
  on the wire, tier clamping.
- `eval/tests/setup_provider_flow.rs` — the picker contract: keyless
  plugin rows, per-row key collection, no fake backends.

## When to add a new door

Don't add a transport for an integration you cannot live-verify. The
built-in vendor rows all say "via a thin bridge" for the same reason: no
vendor ships `/v1/memory`, pointing setup rows at their own REST was
removed as broken-by-design. A new door needs: a loader story, a trust
boundary, caps, structured codes, conformance tests, and this document
updated — or it doesn't ship.
