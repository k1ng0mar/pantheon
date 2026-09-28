# 0001. Workspace restructure toward the target architecture

Status: **executing** (September 2026). Method required by the restructure
prompt: for every current crate/file run

```
current crate/file → KEEP | MOVE | MERGE | SPLIT | DELETE
                   → target location → dependency update → eval
```

Eval gates: `cargo test --workspace` + `python3 eval/run.py` (17 cases)
after every work unit. Nothing is deleted until its replacement is green.
Baseline before any change: both green.

---

## 1. Crate inventory and decisions

| Current crate | Action | Target crate(s) | Rationale |
|---|---|---|---|
| `pantheon-core` | **SPLIT** (crate deleted at the end) | modules distributed per §2 | Not in the target tree. It is a types grab-bag; every module has a principled home. |
| `pantheon-api` | **SPLIT** | `pantheon-api` (bottom leaf: `events`, `error`, `message`, `provenance`, `capability`, `model`, `logging`, `ident`) **+** `rpc`/`serve`/`transport`/`agui` **MOVE → `pantheon-runtime`** | The dependency diagram shows `Providers → APIs` as a *leaf*: api must be depend-able-upon by `pantheon-storage` (Event/error) without a cycle, so the JSON-RPC server cannot stay in it. Server moved to runtime ("Runtime API", ARCHITECTURE §18), user-approved choice. |
| `pantheon-runtime` | **KEEP** (+ receives dispatcher) | `pantheon-runtime` | runtime, lifecycle, turn, context, events, state + the Runtime API surface. |
| `pantheon-agent` | **KEEP** (+ receives `agent_profile`) | `pantheon-agent` | agent, profile, inheritance, instructions. |
| `pantheon-swarm` | **KEEP** | `pantheon-swarm` | swarm, delegation, coordination, task. |
| `pantheon-providers` | **KEEP** (+ receives `catalog`, `model_event`) | `pantheon-providers` | Stays **flat**: `lib/catalog/provider…/model…/request…/response…/streaming`. No per-provider directories. |
| `pantheon-capability` | **KEEP** | `pantheon-capability` | capability, registry, resolution. Policy/role maps stay here; the *types* (`Capability`, `Policy`, `Decision`) live in `pantheon-api` so the future `capability → tools → exec → memory` edges cannot cycle (see §3). |
| `pantheon-exec` | **SPLIT** | `pantheon-exec` keeps: `process`, `context`, `supervisor` (process/plugin supervision), `safewrite` (engine), `skills` (discovery/parse), `plugins`, `danger`, `acp`, `bundled_skills`, compaction. **Tools layer MOVE → new `pantheon-tools`** | "Capability ≠ Tool": the callable-operation surface (`ToolRegistry`, builtins, memory/vault/session-search tools, register helpers) is its own crate. |
| **`pantheon-tools` (NEW)** | **CREATE** | `pantheon-tools` | Deliberate change #1 from the prompt: tools must be distinct from capability. Depends on `pantheon-exec` (diagram `Tools → Exec`). |
| `pantheon-sandbox` | **KEEP** | `pantheon-sandbox` | sandbox, policy boundary, filesystem isolation. |
| `pantheon-memory` | **KEEP** | `pantheon-memory` | memory, store, recall, write, indexing, consolidation. |
| `pantheon-storage` | **KEEP** | `pantheon-storage` | database, schema, repository, transaction. Drops its `agent_profile::is_slug` use → `pantheon_api::ident::is_slug` (keeps storage free of an `→ agent` edge). |
| `pantheon-migrate` | **MOVE (rename)** | `pantheon-migration` | Target tree names it `pantheon-migration`. Directory, package name, and all references renamed. |
| `pantheon-mcp` | **KEEP** | `pantheon-mcp` | client, server, discovery (projection layer today). |
| `pantheon-extensions` | **KEEP** | `pantheon-extensions` | extension, manifest, registry, protocol. |
| `pantheon-gateway` | **KEEP** | `pantheon-gateway` | gateway, channel, message. |
| `pantheon-scheduler` | **KEEP** | `pantheon-scheduler` | scheduler, job, trigger. |
| `pantheon-secrets` | **KEEP** | `pantheon-secrets` | store, secret. |
| `pantheon-cli` | **KEEP** | `pantheon-cli` | main.rs + non-interactive verbs (composition root). |
| `pantheon-tui` | **KEEP** | `pantheon-tui` | app, state, commands, events, screens, widgets. |
| **`pantheon` (NEW)** | **CREATE** | `pantheon` (façade) | Meta crate re-exporting the public surface. |
| `pantheon-otel` | already deleted (prior work) |, | Recorded for continuity: removed before this restructure. |

## 2. `pantheon-core` module dissolution (the SPLIT above)

Cross-module references inside core (measured): `events → {message, model,
provenance}`, `message → provenance`, `model → error`, `model_event →
events`, everything else self-contained. That allows a clean split:

| Core module | Action | Target | Rationale |
|---|---|---|---|
| `error.rs` | MOVE | `pantheon-api::error` | `PantheonError` is needed by `pantheon-storage` (a leaf); it must sit in the bottom crate. |
| `events.rs` | MOVE | `pantheon-api::events` | Canonical `Event` needed by storage, gateway, extensions → bottom crate. Target: api = "events". |
| `message.rs` | MOVE | `pantheon-api::message` | Shared wire/conversation types → "types". |
| `provenance.rs` | MOVE | `pantheon-api::provenance` | Needed by storage + memory + gateway + extensions → bottom crate. |
| `capability.rs` | MOVE | `pantheon-api::capability` | `Capability`/`Policy`/`Decision` **types** used by sandbox, mcp, memory, exec, all below `pantheon-capability`. Keeping types in the bottom crate is what makes the target edge `capability → tools → exec → memory` cycle-free. |
| `model.rs` | MOVE | `pantheon-api::model` | Model *policy* types (`ModelPolicy`, `DefaultModel`, `FallbackChain`, `Auxiliary*`, title/judge/compress traits) consumed by agent, exec, providers, runtime, cli. Cannot go to `pantheon-providers`: `providers → agent` exists (ModelTurn), so `agent → providers` would cycle. |
| `logging.rs` | MOVE | `pantheon-api::logging` | Used by cli, providers, runtime only; must sit below all three → bottom crate. Deviation D2. |
| (new) `ident.rs` | CREATE | `pantheon-api::ident` | `is_slug` promoted here from `AgentProfile::is_slug` so `pantheon-storage` validates agent names without depending on `pantheon-agent`. |
| `catalog.rs` (+ tests) | MOVE | `pantheon-providers::catalog` | Prompt: "catalog.rs holds the provider catalog". Only cli/exec/migrate/runtime/tui/providers consume it; none of them is below providers. |
| `model_event.rs` (+ tests) | MOVE | `pantheon-providers::model_event` | Normalized streaming events (`ModelEvent::to_event → api::events::Event`); the streaming plane. Consumers: cli, providers, runtime, all already (or soon) dependent on providers. |
| `agent_profile.rs` (+ tests) | MOVE | `pantheon-agent::agent_profile` | Target: agent crate owns "profile, inheritance, instructions". Self-contained (no intra-core refs). |
| `lib.rs` | DELETE |, | Crate removed from workspace members last, after all modules moved. |

## 3. Dependency edges

**Current → target (internal edges only):**

| Edge | Status | Action |
|---|---|---|
| `tui → cli` (diagram) | reversed today (`cli → tui`) | **Deviation D1**, see §4. |
| `cli → runtime` | present | keep |
| `runtime → {agent, swarm}` | present | keep (`runtime → scheduler` stays absent: scheduler is driven by cli; adding it is not required for direction compliance) |
| `agent → capability` | present | keep |
| `swarm → capability` | absent | keep absent for now, swarm gates through the agent loop's policy today; no usage exists (D5) |
| `capability → {tools, providers, memory}` | absent | keep absent, no usage exists; the *types* move in §2 is what makes these edges acyclic when they arrive (D5) |
| `tools → exec` | new (WU-C) | created by the extraction |
| `providers → api`, `memory → storage`, `exec → sandbox` | present / new | keep; `providers → api` created by WU-F |
| `runtime → cli/tui` | absent | **must stay absent** (hard rule) |
| `capability → runtime` | absent | **must stay absent** (hard rule) |
| `runtime → extensions/secrets` | present today | Deviation D6, infrastructure boundaries are not yet trait-injected; removing these edges is a follow-up, not a file move |
| `runtime → gateway` | new (WU-E) | created by the dispatcher move (user-approved, D4) |
| `storage → {api}` | new (WU-F), replaces `→ core` | leaf keeps exactly one internal dep |

## 4. Deviations register (target tree ≠ result, with reason)

- **D1, `cli → tui` stays.** The target diagram puts TUI above CLI, but
  the target tree also puts `main.rs` in `pantheon-cli`. The composition
  root must choose TTY→TUI vs line-REPL, which needs an edge to the TUI
  crate; a `tui → cli` edge on top of that is a package cycle (a bin
  cannot be depended on, and splitting cli into lib+bin still cycles).
  Realized instead as: `pantheon-cli` owns entry + non-interactive verbs,
  `pantheon-tui` owns the interactive surface over the same Session
  runtime. No duplicated business logic either way.
- **D2, `logging` lives in `pantheon-api`,** which the target describes as
  "commands, events, types". Only cli/providers/runtime log; every one of
  them already depends on the bottom crate, so that is its lowest legal
  home. (Alternative homes cycle: `providers → runtime` is impossible.)
- **D3, model policy types (`model.rs`) live in `pantheon-api`, not
  `pantheon-providers::model`.** `providers → agent` (ModelTurn) exists, so
  `agent → providers` would cycle; agent consumes `ModelPolicy`. The
  providers-side "model" concepts stay where they are: catalog metadata in
  `catalog.rs` and `ResolvedModel` in `http.rs`.
- **D4, the JSON-RPC server (`rpc`, `serve`, `transport`, `agui`) moves
  from `pantheon-api` into `pantheon-runtime`,** adding `runtime → gateway`
  (GenUi frames/SSE/signing). User-approved. api keeps the protocol
  *types*; the "commands" of `pantheon-api` are the command/protocol types,
  and command *dispatch* is the Runtime API. Consequence: `agui_cli` in the
  CLI calls `pantheon_runtime::{serve, ServeConfig, agui}`.
- **D5, diagram edges with no code behind them are not faked.**
  `capability → {tools, providers, memory}`, `swarm → capability`,
  `runtime → scheduler` are not added as Cargo edges this pass; nothing
  calls across them. §2/§3 make sure they *can* be added without cycles.
- **D6, `runtime → extensions/secrets` edges remain.** Removing them
  requires dependency injection (hook/broker traits defined below the
  runtime), which is a redesign, not a move; tracked as follow-up so the
  "infrastructure out of Runtime" rule lands as its own eval-gated unit.
- **D7, eval harness stays flat** (`eval/run.py` + `cases.json` +
  `README.md`) instead of `eval/{unit,integration,scenarios,regression,
  fixtures,helpers}`: unit/integration coverage is `cargo test` (120 test
  files), and splitting the stdlib-only runner would change harness paths
  for no behavioral gain. Case taxonomy documented in `eval/README.md`.
- **D8, `LICENSE`, `examples/`, `plugins/`** are in the target tree but
  absent from the repo. LICENSE text and example scope are owner decisions,
  tracked as follow-ups (examples planned on top of the new façade crate).
- **D9, agent identity is filesystem data (criterion verified, partial
  gap).** `~/.pantheon/` (or `$PANTHEON_DATA_DIR`) holds `config.toml`
  with `[agents.<name>]` tables that *reference* `agents/<name>/AGENTS.md`,
  `agents/<name>/SOUL.md`, etc.; no agent identity is compiled into the
  workspace. Deviations from the sketch: declarations live in the root
  `config.toml` rather than per-agent `agents/<name>/config.toml`, and
  `USER.md` does not exist yet, both are data-layout follow-ups with no
  workspace-code impact.

## 5. Work units (each eval-gated)

| WU | Change | Eval |
|---|---|---|
| A | rename `pantheon-migrate` → `pantheon-migration` | test + eval |
| B | `core/catalog.rs` → `pantheon-providers::catalog` | test + eval |
| C | create `pantheon-tools` (registry + concrete tools + register helpers out of exec) | test + eval |
| D | create `pantheon` façade crate | test + eval |
| E | move dispatcher `api` → `runtime` (D4) | test + eval |
| F | dissolve `pantheon-core` per §2; remove crate | test + eval |
| G | dependency-direction audit + docs (ARCHITECTURE/README/docs) | test + eval |

Deviation updates appended to §4 as work lands.
