# Pantheon Architecture

> Small at the center, huge at the edges.
> The model is not the runtime. The model is one replaceable component inside it.

Composition, not cloning. Hermes-like personal agents, coding agents, swarms, and automations are different configurations of one system. Not “Hermes + 400 features.”

## System map

```
                         ┌──────────────────────────┐
                         │       USER / SYSTEM      │
                         └────────────┬─────────────┘
                                      │
                     CLI / TUI / GUI / API / Gateways
                                      │
                         ┌────────────▼─────────────┐
                         │       RUNTIME API        │
                         │ JSON-RPC + Event Stream  │
                         └────────────┬─────────────┘
                                      │
              ┌───────────────────────▼───────────────────────┐
              │                     SUPERVISOR                │
              │ runs • policies • approvals • recovery       │
              │ quotas • scheduling • lifecycle               │
              └──────────┬───────────────┬────────────────────┘
                         │               │
              ┌──────────▼──────┐   ┌────▼──────────────────┐
              │   AGENT ENGINE  │   │   EXECUTION ENGINE    │
              │ parent agents   │   │ tools                 │
              │ specialists     │   │ processes             │
              │ swarms          │   │ sandboxes             │
              │ context         │   │ filesystem/git        │
              │ delegation      │   │ browser/MCP           │
              └────────┬────────┘   └──────────┬────────────┘
                       │                       │
         ┌─────────────▼───────────────────────▼─────────────┐
         │                    CAPABILITY PLANE                │
         │ tools • skills • extensions • MCP • plugins       │
         └─────────────────────┬─────────────────────────────┘
                               │
      ┌────────────────────────▼──────────────────────────────┐
      │                  PROVIDER ABSTRACTION                 │
      │ models • search • browser • TTS • STT • vision etc.  │
      └────────────────────────┬──────────────────────────────┘
                               │
      ┌───────────────┬────────┴─────────┬───────────────────┐
      │               │                  │                   │
   Memory          Storage           Secrets            Scheduler
   5-layer         SQLite/events     OS vault/enc       durable jobs
   + runtime       snapshots         local fallback     cron/webhooks
```

Center owns: state, policy, lifecycle, execution, capabilities, recovery, events.
Everything else plugs in.

---

## 1. Runtime core

Owns agent lifecycle, execution state, sessions, task scheduling, permissions, approvals, subagents, quotas, recovery, events, persistence, extension lifecycle, config, secrets, migrations.

Model-agnostic. Adapters: Claude, Kimi, GLM, Qwen, OpenAI-compatible, local, whatever-next.

**Implemented:** `pantheon-core` (events, structured errors, capabilities, model policy), `pantheon-storage` (SQLite event-sourced ledger, `pantheon logs`, `/status`, crash recovery), `pantheon-runtime` (supervisor, run lifecycle, checkpoint/recovery), `pantheon-cli`.

## 2. Agent engine

```
receive input → load state → build context → inference → interpret
    → tool / delegate / respond → execute → observe → update state → continue
```

Every meaningful transition emits a canonical `Event` (core):

`run.started`, `model.requested/completed`, `tool.requested/started/completed`, `agent.spawned/message/completed`, `memory.proposed`, `approval.requested/granted`, `run.recovered/completed`.

Gives replay, debugging, crash recovery. Aligns with MCP 2026 (stateless protocol core, formal extensions, stronger auth, long tasks).

**Implemented:** `pantheon-agent` — pure orchestration loop behind `ModelTurn`. Capability gate at tool call (`Allow` / `Deny` / `Approval`). Denial emits structured `CAP_DENIED`; approval parks the run. Budgets: max turns, max tool calls. Swarm handoff via `AgentSpawner`.

## 3. Dynamic swarm

```
User → Primary → Researcher / Coder / Critic → Primary → User
```

Recursive spawn is allowed under runtime caps: max depth, max concurrent, CPU/mem, token/tool-call/time budgets, network, model restrictions, cost, capabilities. No agent explosions. Caps are runtime-enforced, never agent-chosen.

**Implemented:** `pantheon-swarm` — depth, concurrency, token/tool budgets, model restriction. Spawn over any cap returns a structured error.

## 4. Agent identity

Each persistent agent: identity, config, instructions, memory namespace, skills, capabilities, model config, tool/resource/lifecycle policy.

Example: Nyx / Researcher (private mem, web caps) / Coder (project mem, terminal+git caps) — same runtime infrastructure.

**Status:** durable half landed. `[agents.<name>]` (`AgentIdentity`: display name, soul/persona file ref, memory namespace defaulting to `agent:<name>`, policy) with load-time validation (slug tables, known policies, no namespace clashes). Back-compat: no `[agents]` table = anonymous runs, as before. Still open: prompt assembly reading the identity, profile multiplicity / copying across markers, nudge-interval proactive writes.

## 5. Model layer

```
ModelProvider → Model → Session → Agent
```

User/runtime chooses the model. Never the agent.

`/model`: Anthropic (Opus/Sonnet), Moonshot (Kimi K3), Zhipu (GLM), Qwen, OpenAI-compat, local.

Specialists: `inherit` | `kimi-k3` | `configured-model`. Fallbacks are policy-controlled.

**Locked policy (no routing):**
- **default model** — the run’s model
- **fallback models** — ordered list, failure-only, runtime-controlled; never agent-chosen
- **auxiliary models** — scoped helper *models* (embeddings, rerank, vision, extraction, search synthesis, decision, compression, title), selected by runtime capability need. The title auxiliary names a fresh conversation from its first prompt (`[title_gen]` config, fire-and-forget beside the first turn, `SessionTitled` ledger event feeding the history lists), renamable by hand with `/name`. Auxiliary models default to `auto`: an unconfigured aux resolves to the run's default model instead of going dark. Service capabilities (STT, TTS, search, browser) are provider-plane swaps, not model-policy entries.

**Implemented:** `pantheon-providers` — default + failure-only fallback + auxiliaries. Live HTTP adapters: OpenAI-compatible + Anthropic Messages behind `ModelTurn`, both emitting normalized `ModelEvent`s (core) for complete + streaming paths; fallback chain (`chain.rs`) is the only fallback logic and sits outside the agent loop; provider/model metadata (base URL, wire mode, context limits, tools, vision, reasoning, streaming, cost) lives in the core catalog. Streaming integration tests run against the local llm-router when `PANTHEON_KEY_ROUTER` is set.

## 6. Coding engine

Do not reinvent the coding-agent ecosystem. Steal aggressively.

```
CodingEngine
├── native runtime tools (fs, shell, git, LSP, DAP, AST, test, build, pkg)
└── external coding-agent adapters
    (Pi, OpenCode, Hermes, Claude Code, Aider, Goose, …)
```

Another harness can be a specialized execution backend.

**Status:** handshake slice landed. `pantheon-exec::acp` spawns an ACP server (`omp acp`, `hermes acp`, or any command) over stdio, frames JSON-RPC 2.0 both ways (Content-Length and bare-line reads), and performs the `initialize` handshake with version negotiation (mismatch is an error, never assumed). Session/prompt methods are next; until then it is a probe, not an execution backend. Native tools today are minimal (CLI shell path).

## 7. Skills vs extensions

**Tier 1 — Skills:** portable knowledge. `SKILL.md` + instructions + examples + references + optional scripts. Import from `.agents/skills`, `.claude/skills`, Hermes, OpenClaw, native format. **Implemented:** cross-format discovery (`discover_skills_ext`) covers pantheon + project `.agents/.claude/.pantheon` + `~/.hermes`/`~/.openclaw` + `PANTHEON_SKILLS_DIR` extra roots; session registers discovered skills as capability-gated tools; `pantheon skills list|import|doctor` CLI verbs. `import` is the only writer, and only into `<data_dir>/skills`, copying the raw `SKILL.md` verbatim so it round-trips through `parse_skill`. Import modes: `--url <URL>` (single SKILL.md; GitHub repo/blob URLs rewritten to `raw.githubusercontent.com`), `--repo <URL> [--sub DIR]` (shallow-clone, walk for every `SKILL.md`, skip malformed with a warning), `--clawhub <slug> [--owner OWNER]` (public `clawhub.ai` `/api/v1/skills/{slug}?owner=…` endpoint; the registry returns the raw SKILL.md body in `skill.description`; ambiguous slugs return `409` with the owner list, surfaced as `SKILL_HUB_AMBIGUOUS`). All remote paths validate the body with `parse_skill` **before** writing — fail-closed, no partial file on malformed content. Hermes has no public hub API (the Skills Hub is a JS-rendered browser UI), so hub-pull is GitHub/ClawHub only.

**Bundled skills:** skills shipped inside the binary (`crates/pantheon-exec/bundled-skills/<name>/SKILL.md`, `include_str!` at compile time) and materialized into `<data_dir>/skills/` on first discovery. Bundling exists because a fresh install has an empty skills dir, so shipped knowledge would never reach a session. Seeding runs inside `scan_skills_ext` (before the first root scan), so the session, `skills list`, and `skills doctor` all see the same tree. Write policy: create when missing, refresh when the content stamp still matches, and never clobber a user edit. The stamp is an FNV-1a hash of the source content appended as an HTML comment, so any upstream edit produces a new stamp. Bundled skills carry `origin: bundled`; that is an origin string, not a `SkillSource`, so `skills list --scope bundled` matches it literally. A bundled skill is still plain data: `skill_read` stays gated on FilesystemRead and the skill acquires no capability. First entry: `design-references` (77 design reference sites plus a pick-a-reference workflow, so UI work is grounded in named references instead of model defaults).

**Tier 2 — Extensions:** runtime code. Tools, hooks, context providers, validators, result transformers, slash commands, persistent state, channels, UI, workers. OpenClaw-native plugins live here.

**Implemented:** Tier 2 extensions — 14 declared hooks of which 13 are wired to real fire sites, `plugin.yaml` loader (both Hermes spellings), Python + JavaScript subprocess runners, manager with session-scoped dedup persisted across CLI processes, `skill doctor`, and the §8 compat adapter. Hook surface: `pre_llm_call` (context injection), `pre_tool_call` (**gate** — may deny; fails **closed**), `transform_tool_result` (**transform** — may replace output; fails **open**), and the observers `pre/post_api_request`, `on_session_start/end`, `post_tool_call`, `subagent_start/stop`, `on_stream_start/end` (`on_stream_delta` is opt-in via `PANTHEON_HOOK_STREAM_DELTA=1`; it fires per token and the runner spawns a process per fire). Observer hooks are driven off the canonical `Event` stream by `event_bridge`, queued off the emit path because a hook fire spawns a subprocess; the gate and the transform fire inline at the tool-execution choke point in `session.rs` because their return values change control flow. `pre_gateway_dispatch` is declared but **unwired** — inbound messages are handled in `pantheon-gateway`, which depends only on `pantheon-core` + `pantheon-storage`; `Hook::is_wired()` is the single source of truth, so `compat::map_hook` refuses to report it as `Mapped` and `doctor` flags a manifest that declares it. No hook may be declared without a fire site.

This is a **subset** of the Hermes hook surface (41 events in its `VALID_HOOKS`), not a superset — the earlier "hook superset" claim was false and has been corrected in `hooks.rs`.

## 8. Plugin compatibility

```
OpenClaw / Hermes Plugin → Compat Adapter → Runtime Extension API
```

First targets: Soul, brief2ship, noisegate, time-gap, anti-AI-writing, QR remote, captcha-solver.

| Target | Mapping |
|---|---|
| anti-ai-writing | Extension `pre_llm_call` + Tier 1 skill + validator |
| time-gap | Extension hook + storage session state; fail-open, cache-friendly |
| noisegate | Core context primitive (exec output-compaction), not a plugin |
| brief2ship | Tier 1 skill + CLI tool; MEDIUM sandbox + network |
| qr-remote | Gateway provider; pairing/allowlist/revoke kept |
| captcha-solver | Extension, HIGH/VERY HIGH sandbox, secret broker, policy-gated |
| Soul (OpenClaw) | Lifecycle/context stress test; TS adapter + worker + memory ns |

**Implemented:** Hermes plugins load natively from `~/.hermes/plugins` (or `PANTHEON_EXT_DIR`). `anti-ai-writing` and `time-gap` proven end-to-end through the ledger. Vendor port: `vendor/time-gap-pantheon/`. Doctor flags `TS_ENTRY` for OpenClaw TypeScript plugins. Soul adapter not started.

**Compat adapter implemented** (`pantheon-extensions::compat` + `js_runner`): an OpenClaw `openclaw.plugin.json`, or an OMP `package.json` with an `omp`/`pi` field, is detected; its `api.on("<event>")` registrations are read; and each event maps to a Pantheon hook **only on a real semantic match**. The mapping is gated on `Hook::is_wired()`, so a declared-but-unwired hook (`pre_gateway_dispatch`) is reported unsupported rather than falsely promised — `Mapped` is a contract that a handler will run. The generated `plugin.yaml` lists only the mapped hooks and loads through the existing manager unchanged.

The foreign `api` surface is honoured for `on` and refused-with-a-reason for everything else (`registerProvider`, `registerTool`, `registerHttpRoute`, the media/speech/search registrations), recorded in `CompatReport::refused`, so a partial load is visible rather than implied. The JS runner uses the same one-JSON-line contract as the Python runner and is fail-open; `HookOutput` carries `dropped` / `refused` / `error` so the doctor can tell silence from failure. Credentials a manifest declares (`setup.providers[].envVars`) surface as **names only**.

Measured against the 106 real OpenClaw extensions: 2 import (`active-memory`, `openclaw-lark`), 105 archive with a specific reason. The ceiling is honest — the other 105 register providers, not lifecycle hooks.

## 9. Capability system

Security backbone. Not `coder = yes`.

Granular: `filesystem.read/write`, `shell.execute`, `git.read/write/push`, `network.outbound`, `browser`, `discord.send`, `memory.read/write`, `secrets.use`, `agent.spawn`.

Policies per agent. Coder: push = approval. Spawned researcher: read-only.

**Implemented:** `pantheon-core` Policy (Allow/Deny/Approval) + `pantheon-capability` role maps (e.g. coder push needs approval; researcher is read-only) + agent-loop gate.

## 10. Sandbox hierarchy

```
LOW        → in-process / restricted
MEDIUM     → isolated process + limits
HIGH       → container
VERY HIGH  → stronger sandbox / VM
```

Policy chooses the boundary. Sandboxing is a runtime primitive, not a Docker wrapper.

**Implemented:** `pantheon-sandbox` — levels, per-level `SandboxProfile` (drop-caps / no-new-privs / rlimits), `enforce(Policy → boundary)`, approval scope labels. Container/VM enforcement pending.

## 11. Memory

```
GLOBAL → AGENT → PROJECT → TASK/SESSION → EPHEMERAL TURN
+ RUNTIME STATE (operational, not “memory”)
```

Writes are capability-controlled:

```
propose → policy → provenance → validation → provider
```

No silent prompt-injection writes. Provenance: source, imported_at.

**Implemented:** `pantheon-memory` — five layers, write path with policy + provenance checks, ephemeral never hits the store, recall returns provenance, narrowest-first. Wired into the agent loop (session recall block) and the model-facing memory tools (propose/recall through the gate). Markdown sync (MEMORY.md) with sentinel-header v1 format, heading escaping, and conflict detection. Backend selection (native/http) validated at selection time.

## 12. Storage

Default: SQLite. Runs on laptop, VPS, desktop, Pi, phone client — no DB stack.

```
StorageProvider → SQLite | PostgreSQL | future
```

Execution history is event-sourced. Derived data is mutable.

**Implemented:** `pantheon-storage` — SQLite ledger, event sourcing, `pantheon logs`, `/status`, crash recovery (`RunRecovered`), durable occurrence claims (`ClaimStore`), versioned operation state machines, CAS run leases with heartbeats, and ledger-backed generative-UI artifacts. Durable tool work advances through `translate → execute → translate_result`; process groups are lease-owned and use TERM→KILL cancellation. Global `max_seq` anchor for file checkpoints. Scheduler durable path uses `ClaimStore`; `Ledger::claim` also exists on a separate table (review: pick one). **Session search** (`search.rs`): a hybrid retrieval sidecar in the ledger file, exposed to the model as the `session_search` tool (capability `FilesystemRead`). Chunks are indexed forward-only inside `Supervisor::emit` as message/tool/title events land — a hit points at the exact source event (`run_id` + `seq`). **FTS5 is primary** (exact identifiers, tool names, quoted phrases; recall-first OR query with prefix matching so `websocket` finds `websockets`). **Vector is secondary**: embeddings are computed at index time via the `Embeddings` auxiliary (`EmbedClient::from_policy`; absent entry falls back to a deterministic local hashing embedder, dim 256, char-trigram, L2-normalised — no network, no key, replaced wholesale when a real embedding model is configured). Stored as little-endian f32 blobs on the chunk row (`session_chunks.embedding`); hybrid ranking is 0.60 lexical + 0.30 semantic (cosine) + 0.10 recency, with the semantic term absent when no embedder is attached. Query-side embedding uses the same client. No external vector DB — cosine runs in-process over the chunk rows. Safe file writes live in `pantheon-exec::safewrite` (CLI: `preview` / `stage` / `apply` / `checkpoint` / `rollback`): preview is read-only, every apply snapshots a checkpoint first, multi-file batches publish atomically (tmp+fsync+rename), stale `expected_hash` fails with `SAFE_STALE`, rollback restores by checkpoint id or ledger seq, and a hash-chained journal replays torn applies at startup.

## 13. Secrets

```
Secrets API
  → macOS Keychain / Win CredMan / Linux Secret Service
  → encrypted local vault
  → env/config compat
```

Agents never get the vault:

```
agent → credential capability → secret broker → inject at execution boundary
```

Keys never in prompts or event logs.

**Implemented:** `pantheon-secrets` — `SecretVault` trait, `MemoryVault`, `EnvVault` (`PANTHEON_SECRET_*` + `env:` refs), AES-256-GCM `EncryptedFileVault` (0600 key file, atomic writes, tamper-detect), broker resolve/inject/describe. Values never in Debug/logs. OS keychain backends not implemented.

## 14. Provider plane

Abstract capabilities, not just models:

models, search, browser, vision, STT, TTS, embeddings, rerank, extraction.

Swap `SearchProvider` / `BrowserProvider` / etc. without rewriting agents.

**Implemented:** logic layer in `pantheon-providers` (default + failure-only fallback + auxiliaries, no routing) plus live OpenAI-compat and Anthropic adapters with normalized `ModelEvent` emission (single-shot + SSE streaming) and catalog-driven capability/cost metadata. STT/TTS seam: `SttProvider`/`TtsProvider` traits with `command` (bounded subprocess: text→stdout / audio→stdin) and `openai` (compatible audio endpoints) backends, registry + config `[stt]`/`[tts]` selection — service providers, deliberately outside model policy. Search/browser/vision-service swaps remain future work.

## 15. MCP

MCP is the interop boundary, not the internal architecture.

```
Internal: Native Capability API
External: MCP adapter ↔ MCP servers (tools / resources / prompts)
```

External servers become capabilities under policy. Denied tools listed as `allowed:false`.

**Implemented:** `pantheon-mcp` — policy token → core Capability projection, denied-listing, unknowns stay gated. No live MCP servers attached yet.

## 16. Gateways

Telegram, Discord, Custom first. Normalized shapes:

`InboundMessage`, `OutboundMessage`, `Conversation`, `Identity`, `Attachment`, `Reaction`, `Command`.

Agent sees a canonical event. Gateway handles auth, allowlist, pairing, delivery, dedup, attachments, reconnects.

**Implemented:** `pantheon-gateway` — canonical shapes (conversation key includes thread), default-deny allowlist + one-shot pairing, redelivery dedup, retry backoff + reconnect outbox. Live surfaces: Telegram long-poll daemon with persisted update cursor; Discord gateway websocket (tungstenite, blocking, IDENTIFY/RESUME, heartbeat with missed-ACK reconnect) plus webhook-bridge inbound; approval buttons (Grant/Deny custom_ids) resolve parked runs; `pantheon gateway run` runs both surfaces against the runtime with a thread->run map; `gateway start|stop|restart|status` wraps it in a service (systemd user unit, launchd on macOS) with an absolute ExecStart and a post-start liveness check, and drains the durable outbox that `pantheon run --deliver` writes. Rate-limit (429): the platform's own wait is now honored — `Retry-After` header (Discord) and Telegram's `parameters.retry_after` body field are carried on `ChannelError::retry_after_secs`, `delivery::retry_delay_ms` prefers the hint over the exponential guess (capped), and the daemon's 429 path sleeps the longer of the two. Hermes's `_rate_limits.json`/`human_delay_mode` proactive pacing is still unbuilt.

## 17. Interfaces

One runtime. CLI / GUI / TUI + Gateways all talk to the same Runtime API. No duplicated business logic.

**Implemented:** CLI (34 verbs: chat, run, explain, status, audit, grant, deny, memory, plugins, extensions, hook, doctor, preview, stage, apply, checkpoint, rollback, serve, stream, sign, channel, gateway, setup, reset, pipeline, providers, skills, migrate, schedule, swarm, model, provider, session, mcp) plus the AG-UI local web client (`pantheon serve` at `/`). The web surface executes the canonical Session runtime and replays its typed ledger turn/item stream; it remains intentionally minimal rather than a full product UI. The TUI (`pantheon` on a TTY) shares the same Session runtime and exposes the session surface below.

**Session surface (shared by CLI REPL and TUI):** every conversation is a durable ledger run. `pantheon` (bare) opens the TUI on a TTY, or the line-buffered REPL otherwise. `/history` opens an interactive searchable list — type to filter, Up/Down to scroll, Enter to resume — in both surfaces (the REPL mirrors the existing `pick_model` filter-then-select loop; the TUI renders a centered ratatui overlay). `/resume [id]` resumes a run by id (validates first; missing id → most recent run). `pantheon --resume [id]` enters the session on a specific run from the shell, failing loudly on an unknown id. A resumed run rebuilds its transcript from the ledger via `rebuild_messages` + `replay` before the first turn. TUI slash commands: `/help`, `/runs [N]`, `/history`, `/resume [id]`, `/status <ID>`, `/name [TITLE]`, `/cost`, `/clear`, `/exit`, `/quit`. The REPL (non-TTY `pantheon`) has the wider set documented in docs/cli.md.

## 18. Runtime API

Canonical protocol: **JSON-RPC + event stream**.

Commands:

```
agent.create/run/pause/resume/stop
task.create/cancel
swarm.spawn/inspect
memory.search/propose
tool.list/execute
model.list/select
schedule.create
package.install/update/rollback
```

Events stream as the core `Event` enum (no second event type). Transports: HTTP / WebSocket / Unix socket.

**Implemented:** `pantheon-api` — JSON-RPC 2.0 dispatcher + `UnixSocketTransport` + stdlib HTTP server (`pantheon serve`) with SSE streams and signed blob routes. `agui.send` now admits a host-assigned `turn_id` and executes through the same `pantheon-runtime::Session` used by CLI/gateway paths; completion, parking, and failure are durable `TurnCompleted` / `TurnParked` / `TurnFailed` ledger events. Other methods: `system.ping`, `system.methods`, `agui.grant/deny/cancel/frames/sign/artifact.put/serve_hint`. Request bodies capped at 1 MiB (413). The aspirational command list below is the long-term protocol, not the current surface.

## 19. Observability

``pantheon logs` run_abc123`: why spawn? why tool? why denied? why retry? why fallback? which memory influenced this?

```
Runtime events → OTel instrumentation → logs / metrics / traces
              + durable execution ledger (offline `pantheon logs`)
```

**Implemented:** `pantheon-otel` — Event → span mapping, metrics fold over replay, offline explain(). Deltas excluded from spans. `pantheon logs` remains the offline path. Span mapping exists but no live OTel exporter (no OTLP/gRPC/HTTP push target); instrumentation is fold-only today.

## 20. Recovery

Structured errors:

```json
{
  "code": "TOOL_TIMEOUT",
  "layer": "execution",
  "retryable": true,
  "cause": "...",
  "remediation": "...",
  "evidence": "..."
}
```

Classes: retry / fallback / degrade / pause / resume / fail.

Runtime checkpoints and recovers runs after crashes. Not “hope the model remembers.”

**Implemented:** structured `PantheonError` in core; supervisor `RunRecovered`; scheduler/webhook occurrence keys; storage claims durable across restart.

## 21. Scheduler

First-class subsystem, not a side cron.

```
schedule → durable run request → agent/session/context policy
        → execution → delivery
```

Supports cron, one-shot, interval, webhook, conditional, manual. Plus pause/resume, missed-run handling, locks, idempotency, delivery targets, fresh/inherited context policies, and per-job model/provider pins (`--model`/`--provider`; Hermes parity with its per-job model + provider snapshot; blank pins rejected, `None` inherits the runtime default via `effective_model`). Pins are honored at fire time: `schedule run` resolves the job's own model and provider and applies its auxiliaries, overriding the runtime default.

**Implemented:** `pantheon-scheduler` — 5-field cron (UTC, Vixie dom/dow OR-rule), interval/one-shot, webhook path routing + request-id occurrence, missed-run policy, durable claim ledger on storage. `schedule run` drives a real model-backed agent turn, and records the resulting run id and last-run time on the job.

## 22. Package ecosystem

A package is more than a skill:

```
my-research-agent/
├── agent.yaml
├── instructions.md
├── skills/  tools/  hooks/  policies/
├── tests/   examples/
└── manifest.json
```

Trust levels gate capabilities. Lifecycle: install → verify → resolve deps → sandbox → test → approve → activate → rollback. Channels: stable / beta / nightly + pinning.

**Status:** not started. `packages/` format and lifecycle open.

## 23. Migration

Killer feature.

```
pantheon migrate hermes|openclaw|omp

detect → analyze → plan → dry-run → approval → backup → apply → validate
```

Imported items retain provenance: `source`, `source_version`, `imported_at`. Unmappable items are archived, never silently dropped.

**Implemented:** `pantheon-migrate` — the full pipeline for **Hermes, OpenClaw, and OMP**, exposed as a CLI verb. `migrate detect|show|plan|apply|validate <source> [path]`, with `--kind K` filtering, `--json` for machine output, and `--yes` to skip the approval gate. `detect`/`show`/`plan`/`validate` are read-only; `apply` is the single writer, refuses to run without approval, and runs `backup → apply → validate` in that order.

Five item classes are **bridged** rather than copied, because their source representation cannot be dropped into the data dir: `mcp`, `credentials`, `session`, and the memory-plane `persona`/`memory`. A bridge parses the source and writes a Pantheon-shaped artefact; the file copier never sees these targets, so a source file is never laid over its own destination.

- **skills / agents / rules / commands / prompts** — copied verbatim into `<data_dir>/<kind>/<name>`.
- **extensions** — OpenClaw and OMP plugins go through the section 8 compat adapter, which generates a `plugin.yaml` carrying only the hooks that map.
- **mcp** — the source's `mcp_servers` block (or an `mcp.json`) becomes `<data_dir>/mcp/<source>.json`. Transport, command, args, and url carry over. Header tokens and env values do not: a `${VAR}` indirection becomes a declared `requires_env` name, and a literal token is flagged `needs_credentials` with its name *not* invented.
- **credentials** — a source `.env` is **merged into `<data_dir>/.env`**, which is already Pantheon's own key store (the file `pantheon model` writes and `load_dotenv` reads), so an imported key works with no wiring. Only names that classify as provider/channel/mcp-auth carry; `HOME`, ports, timeouts and allow-lists stay behind. Written 0600, comments and order preserved, and **an existing key is never clobbered** — it is reported as `already_present` for the operator to decide. A names-only manifest lands at `<data_dir>/credentials/<source>.json`.
- **sessions** — transcripts are copied to a quarantine path under `<data_dir>/imported-sessions/<source>/` with a manifest, then parsed and indexed into the same `session_search` store the live runtime writes. A migrated chunk's `run_id` is namespaced `migrated:<source>:<session>`, so a hit can never be mistaken for a real ledger event, and `chunk_id` is derived from (source, file, line) so re-indexing converges. Same-named files in different subdirectories are disambiguated by their relative path; symlinks are never followed; an oversized tool result is clipped rather than allowed to dominate the FTS table.
- **providers** — a source's `providers:` block becomes `[custom_providers.<id>]` sections (`base_url`, `api_mode`, `key_env`). The default destination is a reviewable sidecar at `<data_dir>/providers/imported.toml`; `--merge-providers` additionally merges into `<data_dir>/config.toml`, which is the only file migration edits that the runtime reads on every startup. That merge is **text-level** — the rest of the user's config is left byte-identical rather than round-tripped through a TOML parser — a pre-image goes to `config.toml.pre-migrate`, and an existing `[custom_providers.<id>]` is skipped, never replaced. A source that stored a *literal* key gets `key_env` omitted rather than an invented variable. **No model list is migrated.** Another agent's config holds a snapshot of a third-party endpoint from whenever that agent last synced; for an aggregator it is stale immediately (the reference machine's local router offers 5 models where the source config listed 1). So `[custom_providers.*].models` holds only models the operator **named**: `pantheon model` records one when an id is typed by hand, and `pantheon provider models <name>` fetches the live list on demand, marking which recorded names the endpoint no longer offers. A harvested list would be a cache wearing the authority of configuration. Not offered for OMP: its `models.yml` is a credential file and is classified as a secret.

**Builtin provider keys need no migration, only verification.** Pantheon's catalog names each provider's key env after the same `<PROVIDER>_API_KEY` convention the sources use, so a carried key usually already lands under the name the runtime reads. Measured on the reference machine, 6 of the 31 credential-shaped variables in `~/.hermes/.env` are exact catalog `key_env` matches (`openrouter`, `groq`, `nebius`, `nvidia-nim`, `qwen`, `cloudflare`) and need nothing. `apply` prints a reconciliation after every run so a key that matches nothing is distinguishable from one that does, and a near-miss is reported with the variable Pantheon expects.

Invariants, each unit-tested:

- **Never silently dropped.** Every detected item appears in the plan as an import, an archive with a reason, or a credential skip. Nothing vanishes.
- **A credential file is never an import target.** `.env` is read and merged; `auth.json`, `models.yml`, `config.yaml`, `agent.db`, `broker.token`, `*.pem` and `*.key` classify as `ItemKind::Secret` and are reported — neither imported *nor archived*, because archiving a credential would make this verb an exfil path. A value is only ever written to `<data_dir>/.env`, never to a report, a plan, or a log.
- **No name collisions.** Two sources can offer the same skill name (Hermes mirrors its marketing family into `marketingskills/`); the first wins and the loser is archived naming the winner, never silently overwritten.
- **Nothing broken gets imported.** A `SKILL.md` is validated with the same frontmatter rules `parse_skill` applies before it is a candidate, so an artifact the runtime would reject is archived with the reason instead.
- **A bridge target is never reached by the copier.** The file-copy path filters bridged kinds explicitly, so a merge cannot be undone by a verbatim copy.

Backups land under `<data_dir>/migrate-backups/<ts>/` with a JSON manifest mirroring absolute target paths, and `ApplyReport::rollback` restores them. `copy_tree` refuses to follow symlinks and records every skip rather than copying a partial tree. Replace is remove-then-copy so a re-import cannot leave stale files behind.

Item kinds: `skill`, `agent`, `rule`, `command`, `prompt`, `extension` (Tier 2), `persona`, `memory`, `provider`, `mcp`, `schedule`, `channel`, `opaque`, `secret`. Skills land in `<data_dir>/skills/<name>/`, extensions in `PANTHEON_EXT_DIR`, and persona/memory route to the memory plane as `memory://<kind>/<name>` for `pantheon memory put`.

**Deliberately out of scope: Codex CLI and Claude Code.** Both are installed on the reference machine and neither is a `SourceKind`, because neither is an agentic harness. They are single-model CLIs: no plugin lifecycle, no extension/hook surface, no agent identity, no memory plane, and no runtime worth importing into. A migration from either would copy files that Pantheon has no subsystem to load them into. The lesson worth taking from them is the adapter shape (section 6), not their contents.

**Coverage measured on the reference machine** (Hermes 0.21.5+2144, OMP 18.2.4, 106 OpenClaw extensions): 194 skills, 7 extensions, 4 MCP servers bridged, 7 custom providers with 246 model rows, 30 credential keys merged, transcripts quarantined and indexed (10 chunks), 0 credentials leaked outside `<data_dir>/.env`, 0 existing keys or providers clobbered. Of the 107 OpenClaw extensions, 2 import — `active-memory` and `openclaw-lark` — because the other 105 register providers (`registerProvider`, `registerHttpRoute`, media/speech/search) rather than lifecycle hooks. That is the honest ceiling: the adapter maps hooks, and a provider registry is a different subsystem.

## 24. What we steal

| Source | Take |
|---|---|
| Hermes | long-run behavior, personal-agent ergonomics, memory, skills, messaging, providers, sandboxing, self-host |
| OpenClaw | plugin lifecycle, gateways, Soul, channels, extension shape |
| Pi / OMP | minimal loop, composability, coding workflow |
| OpenCode | sessions, provider abstraction, client/server |
| Claude Code / Codex / Gemini / Aider / Goose / SWE | adapter, don’t clone |
| MCP | external interop |
| OpenTelemetry | observability |

---

## Decisions (locked)

1. Runtime owns lifecycle, state, policy, execution, capabilities, recovery, events.
2. Model policy: default + failure-only fallback (runtime-controlled) + auxiliaries. **No routing. Agents never pick models.**
3. Memory writes: propose → policy → provenance → validation → provider.
4. Secrets via broker at the execution boundary; values never in context/logs.
5. MCP = external interop only. Internal = native Capability API.
6. Durable checkpoint + recovery (D). SQLite default storage.
7. OTel + ledger for observability; `pantheon logs` stays offline-capable.
8. Skills (portable) vs extensions (runtime code) stay two tiers.
9. Sandboxing is a runtime primitive with policy-chosen levels.
10. Migration retains provenance; unmappable content is archived, not dropped.

---

## Crate map

| Crate | Role |
|---|---|
| `pantheon-core` | events, structured errors, capabilities, model policy |
| `pantheon-storage` | SQLite ledger, event sourcing, claims, explain |
| `pantheon-runtime` | supervisor, run lifecycle, recovery |
| `pantheon-cli` | run / explain / status / extensions / hook / doctor |
| `pantheon-agent` | agent engine loop, tool gate, budgets |
| `pantheon-swarm` | spawn caps (depth, concurrency, budget, model) |
| `pantheon-exec` | output compaction (head+tail, FNV-1a middle hash) |
| `pantheon-capability` | policy/role → Allow/Deny/Approval maps |
| `pantheon-sandbox` | levels, profiles, enforce(Policy → boundary) |
| `pantheon-secrets` | vaults + broker (inject at boundary) |
| `pantheon-scheduler` | cron/interval/webhook + durable claims |
| `pantheon-gateway` | canonical messages, allowlist, dedup, delivery |
| `pantheon-providers` | default + fallback + auxiliaries (no routing); OpenAI-compat + Anthropic adapters, streaming, `ModelEvent`s |
| `pantheon-extensions` | hooks, manifest, python runner, manager, doctor |
| `pantheon-memory` | 5-layer memory + write path + provenance |
| `pantheon-api` | JSON-RPC 2.0 + Unix socket transport |
| `pantheon-mcp` | MCP token → capability adapter |
| `pantheon-otel` | Event → span/metric mapping |
| `pantheon-migrate` | Hermes/OpenClaw/OMP: detect → analyze → plan → approval → backup → apply → validate |

Eval: `eval/run.py` + `eval/cases.json` — regression harness driving the real CLI in fresh sandboxes.

---

## Open gaps

- **Context window management is not in the production loop.** `pantheon-exec::context` implements `fit_to_window` / `compress_oldest`, and the `ContextTrimmed` / `ContextCompressed` / `CONTEXT_OVERFLOW` types exist, but `Session::drive` calls none of them: `session.rs` contains zero references to any of those symbols, and `drive` bounds turns, not tokens. A long session grows the transcript until the provider rejects it. Highest-value single fix in the repository; the module is already written and tested.
- **Sub-agent delegation is not wired.** `AgentSpawner` exists but `session.rs:879` hard-codes `spawner: None` and the Tools arm returns `SWARM_SPAWN_DENIED` (`session.rs:1517`), so the spawn path is unreachable from a chat turn. `pantheon swarm` writes ledger rows and a manifest; it does not execute the agents.
- `pre_gateway_dispatch` has no fire site: `pantheon-gateway` depends only on `pantheon-core` + `pantheon-storage` and cannot reach the extension manager. Wiring it needs a deliberate dependency edge (or a callback trait in core) — it is declared, reported unsupported by the compat adapter, and flagged by `doctor` rather than silently mapped
- `pantheon-otel` has no consumer and no OpenTelemetry dependency. It is a pure event→span/metrics transformation, so the crate name oversells it; there is no OTLP exporter, no live trace, and no live metrics
- **The sandbox cannot initialize on most EC2/container hosts.** `bwrap` is present but fails with `setting up uid map: Permission denied` where user namespaces are blocked, and the runner falls back to a direct spawn. `shell` now prefixes `[sandbox unavailable on this host: ran WITHOUT namespace isolation]` to the tool result whenever this happens, so the downgrade is visible in the tool output and the ledger. It does not fail the call: the capability gate already ran, so this is degraded isolation rather than a bypassed policy. A `fail_closed` profile option would let an operator refuse instead
- Sub-agent delegation is not wired. `AgentSpawner` exists but `session.rs:879` hard-codes `spawner: None` and the Tools arm returns `SWARM_SPAWN_DENIED` (session.rs:1517), so the spawn path is unreachable from a chat turn. `pantheon swarm` writes ledger rows and a manifest; it does not execute the agents
- MCP is projection only: `pantheon mcp` maps declared tokens to capabilities but launches no client, so no MCP tool can be discovered or called
- Scheduler has no daemon and no missed-run catch-up after a long outage (jobs fire on the next `schedule run`, not retroactively). `DurableClaimLedger` and `runs_for_missed` are called only from their own tests
- Durable agent identity configs (identity/config/memory-namespace/skills/capability/mode per persistent agent)
- Package ecosystem (`packages/` format, install/verify/resolve/sandbox/test/approve/activate/rollback, channels + pinning)
- No web/search, image, browser, patch, or grep/glob tool; no image or file input on any surface
- Approval has no "always allow" and no rule persistence
- Config and secrets are env-file only; the secrets broker is not on the tool execution path

## Audit trail

Full per-crate audit (September 2026): docs/audit/group-A.md,
group-B.md, group-C.md, comparative-nyx.md. Fix pass applied the high/
medium findings; deliberate non-changes are documented there too.
