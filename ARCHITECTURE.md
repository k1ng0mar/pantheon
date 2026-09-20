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

**Implemented:** `pantheon-core` (events, structured errors, capabilities, model policy), `pantheon-storage` (SQLite event-sourced ledger, `/explain`, `/status`, crash recovery), `pantheon-runtime` (supervisor, run lifecycle, checkpoint/recovery), `pantheon-cli`.

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

**Status:** not a durable subsystem yet. Capability policies exist in `pantheon-capability` / `pantheon-sandbox`; identity configs are still future work.

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
- **auxiliary models** — scoped helpers (embeddings, rerank, STT/TTS, vision, extraction, search synthesis), selected by runtime capability need

**Implemented:** `pantheon-providers` — default + failure-only fallback + auxiliaries. No live HTTP adapters yet.

## 6. Coding engine

Do not reinvent the coding-agent ecosystem. Steal aggressively.

```
CodingEngine
├── native runtime tools (fs, shell, git, LSP, DAP, AST, test, build, pkg)
└── external coding-agent adapters
    (Pi, OpenCode, Hermes, Claude Code, Aider, Goose, …)
```

Another harness can be a specialized execution backend.

**Status:** not started. Native tools today are minimal (CLI shell path). External adapters open.

## 7. Skills vs extensions

**Tier 1 — Skills:** portable knowledge. `SKILL.md` + instructions + examples + references + optional scripts. Import from `.agents/skills`, `.claude/skills`, Hermes, OpenClaw, native format.

**Tier 2 — Extensions:** runtime code. Tools, hooks, context providers, validators, result transformers, slash commands, persistent state, channels, UI, workers. OpenClaw-native plugins live here.

**Implemented:** Tier 2 extensions — hook superset (`pre_llm_call`, `pre/post_api_request`, `pre_gateway_dispatch`), `plugin.yaml` loader (both Hermes spellings), Python subprocess runner (fail-open, 10s timeout), manager with session-scoped dedup persisted across CLI processes, `skill doctor`.

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

**Implemented:** `pantheon-memory` — five layers, write path with policy + provenance checks, ephemeral never hits the store, recall returns provenance, narrowest-first. Not yet wired into the agent loop.

## 12. Storage

Default: SQLite. Runs on laptop, VPS, desktop, Pi, phone client — no DB stack.

```
StorageProvider → SQLite | PostgreSQL | future
```

Execution history is event-sourced. Derived data is mutable.

**Implemented:** `pantheon-storage` — SQLite ledger, event sourcing, `/explain`, `/status`, crash recovery (`RunRecovered`), durable occurrence claims (`ClaimStore`). Scheduler durable path uses `ClaimStore`; `Ledger::claim` also exists on a separate table (review: pick one).

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

**Implemented:** logic layer in `pantheon-providers` (default + failure-only fallback + auxiliaries, no routing). No live HTTP providers yet.

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

**Implemented:** `pantheon-gateway` — canonical shapes (conversation key includes thread), default-deny allowlist + one-shot pairing, redelivery dedup, retry backoff + reconnect outbox. Live Telegram/Discord surfaces not wired.

## 17. Interfaces

One runtime. CLI / GUI / TUI + Gateways all talk to the same Runtime API. No duplicated business logic.

**Implemented:** CLI only (`run`, `explain`, `status`, `extensions`, `hook`, `doctor`). GUI/TUI absent.

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

**Implemented:** `pantheon-api` — JSON-RPC 2.0 protocol + dispatcher + `UnixSocketTransport` (newline-delimited, one request-id → one response-id). Builtins: `system.ping`, `system.methods`. Runtime command handlers attach when the agent loop is wired into the supervisor.

## 19. Observability

`/explain run_abc123`: why spawn? why tool? why denied? why retry? why fallback? which memory influenced this?

```
Runtime events → OTel instrumentation → logs / metrics / traces
              + durable execution ledger (offline /explain)
```

**Implemented:** `pantheon-otel` — Event → span mapping, metrics fold over replay, offline explain(). Deltas excluded from spans. `/explain` remains the offline path. No live OTel exporter.

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

Supports cron, one-shot, interval, webhook, conditional, manual. Plus pause/resume, missed-run handling, locks, idempotency, delivery targets, fresh/inherited context policies.

**Implemented:** `pantheon-scheduler` — 5-field cron (UTC, Vixie dom/dow OR-rule), interval/one-shot, webhook path routing + request-id occurrence, missed-run policy, durable claim ledger on storage. Not yet driving live agent runs end-to-end.

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
pantheon migrate hermes|openclaw|opencode|claude

detect → analyze → plan → dry-run → approval → backup → apply → validate
```

Imported items retain provenance: `source`, `source_version`, `imported_at`. Unmappable items are archived, never silently dropped.

**Implemented:** `pantheon-migrate` — Hermes + OpenClaw detect/analyze/plan/render, provenance, archive-unmappable (unit-tested). Not exposed as a CLI verb yet.

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
7. OTel + ledger for observability; `/explain` stays offline-capable.
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
| `pantheon-providers` | default + fallback + auxiliaries (no routing) |
| `pantheon-extensions` | hooks, manifest, python runner, manager, doctor |
| `pantheon-memory` | 5-layer memory + write path + provenance |
| `pantheon-api` | JSON-RPC 2.0 + Unix socket transport |
| `pantheon-mcp` | MCP token → capability adapter |
| `pantheon-otel` | Event → span/metric mapping |
| `pantheon-migrate` | Hermes/OpenClaw detect → plan → provenance |

Eval: `eval/run.py` + `eval/cases.json` — regression harness driving the real CLI in fresh sandboxes.

---

## Open gaps

- Provider HTTP adapters (OpenAI-compatible + Anthropic) behind `ModelTurn`
- Agent loop wired through Runtime API command handlers + CLI
- Sandbox container/VM enforcement (profiles are policy values today)
- Durable agent identity configs
- Tier 1 skill import
- OpenClaw Soul adapter + full plugin compat
- Live gateways (Telegram/Discord) and live MCP servers
- Package ecosystem (manifest, trust levels, lifecycle)
- `pantheon migrate` CLI surface
- Grow eval toward 20–30 Hermes-history cases
- Pick one claim store (`ClaimStore` vs embedded `Ledger::claim`)
