# Architecture

> Small at the center, huge at the edges.
> The model is one replaceable component inside the runtime.

```
                 CLI / TUI / Web / API / Gateways
                                   │
                    RUNTIME API (JSON-RPC + events)
                                   │
                               SUPERVISOR
              runs · policies · approvals · recovery
              quotas · scheduling · lifecycle
                   ┌───────────────┴────────────────┐
              AGENT ENGINE                  EXECUTION ENGINE
         parents · specialists           tools · processes
         swarms · delegation             sandboxes · fs/git
                                             browser/MCP
                   └───────────────┬────────────────┘
                           CAPABILITY PLANE
                  tools · skills · extensions · MCP
                                       │
                           PROVIDER ABSTRACTION
                   models · search · voice · embeddings
                                       │
              Memory · Storage · Secrets · Scheduler
```

The center owns state, policy, lifecycle, execution, capabilities, recovery, and events. Everything else plugs in.

## Runtime

The supervisor owns agent lifecycle, runs, approvals, scheduling, and recovery. Every meaningful transition emits a canonical `Event`; the SQLite ledger is the source of truth for replay, debugging, and crash recovery. Run ownership is lease-based with heartbeats, so at most one supervisor drives a run.

## Agent engine

`receive → load state → build context → infer → interpret → execute → observe → update state → continue`, gated at every tool call (Allow / Deny / Approval). Denials and approvals are transcript outcomes, not crashes. Budgets bound turns and tool calls; swarms spawn under runtime caps.

## Capabilities

The security backbone: granular capabilities (`filesystem.read`, `shell.execute`, `git.push`, `memory.write`, `agent.spawn`, …) mapped through per-agent policies. Knowledge never implies authority — skills, memories, and tool descriptions inform; only policy permits.

## Execution

Tools run under policy-chosen sandbox boundaries (process isolation up through container/VM). A boundary that can't initialize fails closed — `SANDBOX_UNAVAILABLE` rather than running unconfined. File writes go through checkpointed, atomic apply with stale-check rejection and rollback.

## Memory and storage

Five memory layers (global → agent → project → task → ephemeral) behind one write path: propose → policy → provenance → validation → store. Storage is SQLite by default: event ledger, durable operation state machines, claim ledgers, and an FTS sidecar for session search. No database server, no external vector store.

## Secrets

Resolved at the execution boundary through a broker (OS keychain → encrypted file → env), never into prompts, logs, or the ledger. Config names variables; values stay outside it.

## Providers

Transport adapters only — OpenAI-compatible and Anthropic behind one `ModelTurn` interface, no SDKs above the provider crate. Default model, failure-only fallback chain, per-capability auxiliaries. No routing; agents never choose models.

## Surfaces

CLI, TUI, web (`serve`), and gateways (Discord, Telegram) all drive the same Session runtime off the same ledger. Extensions add hooks and tools through a manifest + stdio contract, fail-open by default; foreign plugin formats load through a compat adapter that maps only real semantic matches.

## Migration

`pantheon migrate hermes|openclaw|omp`: detect → plan → approve → backup → apply → validate. The apply itself is transactional (stage → validate → commit): everything detected is imported or archived with a reason — nothing silently dropped, credentials never written outside `<data_dir>/.env`.

## Locked decisions

1. Runtime owns lifecycle, state, policy, execution, capabilities, recovery, events.
2. Model policy is default + failure-only fallback + auxiliaries. No routing.
3. Memory writes: propose → policy → provenance → validation.
4. Secrets resolve at the execution boundary; values never in context or logs.
5. MCP is external interop only; internal is the native Capability API.
6. Durability is SQLite-backed state machines, not hope.
7. Skills (knowledge) and extensions (code) stay two tiers.
8. Migration retains provenance; unmappable content is archived, not dropped.

## Crate map

| Crate | Role |
|---|---|
| `pantheon-api` | Events, errors, capabilities, model policy, identifiers (bottom leaf) |
| `pantheon-storage` | Ledger, claims, leases, session search |
| `pantheon-runtime` | Supervisor, lifecycle, recovery, sessions, Runtime API (`serve`/`rpc`) |
| `pantheon-agent` | Agent loop, tool gate, budgets |
| `pantheon-exec` | Process/fs execution engine, skills, plugins, supervision |
| `pantheon-tools` | Callable tools + registry (builtins, memory/vault/session-search/skill tools) |
| `pantheon-capability` | Policy/role maps |
| `pantheon-sandbox` | Levels, profiles, enforcement |
| `pantheon-memory` | Layers, write path, backends |
| `pantheon-secrets` | Vaults, broker |
| `pantheon-providers` | Adapters, catalog, fallback chain |
| `pantheon-extensions` | Hooks, manifests, runners, compat |
| `pantheon-scheduler` | Triggers, durable claims |
| `pantheon-gateway` | Channels, allowlist, delivery |
| `pantheon-swarm` | Spawn caps |
| `pantheon-mcp` | Token → capability projection |
| `pantheon-migration` | Source importers |
| `pantheon-tui` | Terminal application: interactive session + non-interactive verbs |

## See also

- [Contributing](contributing.md) — boundaries and change mechanics
- [Decisions](decisions/) — records behind the locks above
- [Terminal reference](../reference/terminal.md) — the surface this architecture serves
