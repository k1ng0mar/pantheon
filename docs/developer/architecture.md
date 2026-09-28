# Architecture

> Small at the center, huge at the edges.
> The model is one replaceable part inside the assistant.

```
                 CLI / TUI / Web / API / Gateways
                                   |
                    RUNTIME API (JSON-RPC + events)
                                   |
                               SUPERVISOR
              runs · permissions · approvals · recovery
              quotas · scheduling
                   +---------------+----------------+
              AGENT ENGINE                  EXECUTION ENGINE
         parents · specialists           tools · processes
         swarms · delegation             sandboxes · files/git
                                             browser/MCP
                   +---------------+----------------+
                           PERMISSION PLANE
                  tools · skills · extensions · MCP
                                       |
                           PROVIDER ABSTRACTION
                   models · search · voice · embeddings
                                       |
              Memory · Storage · Secrets · Scheduler
```

The center owns the state, the permissions, and the recovery. Everything else plugs in.

## Runtime

The supervisor owns conversations, approvals, scheduling, and crash recovery. Every meaningful change is recorded as an `Event` in a SQLite database; that record is the source of truth for replays, debugging, and recovery. Only one supervisor can drive a run at a time, enforced with leases and heartbeats.

## Agent engine

`receive → load state → build context → infer → interpret → execute → observe → update state → continue`, with a permission check at every tool call (Allow / Deny / Ask). Denials and approvals are written into the conversation, not crashes. Limits bound turns and tool calls; agent delegation runs under runtime caps.

## Permissions

The security backbone: fine-grained capabilities (`filesystem.read`, `shell.execute`, `git.push`, `memory.write`, `agent.spawn`, ...) mapped through per-agent policies. Knowing something never means being allowed to do something: skills, memories, and tool descriptions inform; only the policy permits.

## Execution

Tools run inside sandbox boundaries chosen by the policy (process isolation up to containers/VMs). A boundary that cannot start fails closed: the tool reports `SANDBOX_UNAVAILABLE` instead of running unconfined. File writes go through atomic apply with checkpoints and rollback.

## Memory and storage

Five memory layers (global → agent → project → task → ephemeral) behind one write path: suggest → permission → label the source → validate → store. Storage is SQLite by default: the event record, durable operation state, and a search index for sessions, all in one file.

## Secrets

Secrets are resolved where they are used, through a broker (OS keychain → encrypted file → environment). Values never appear in prompts, logs, or the record. Config names variables; the values stay outside it.

## Providers

Transport adapters only: OpenAI-compatible and Anthropic behind one `ModelTurn` interface, no SDKs above the provider crate. Default model, failure-only backup chain, per-job helper models. No routing; agents never pick models.

## Surfaces

Terminal app, terminal commands, web (`serve`), and chat gateways (Discord, Telegram) all drive the same assistant off the same record. Extensions add hooks and tools through a manifest + stdio contract, fail-open by default; foreign plugin formats load through a compat adapter that only maps real matches.

## Migration

`pantheon migrate hermes|openclaw|omp`: detect → plan → approve → backup → apply → validate. The apply is transactional (stage → validate → commit): everything found is imported or archived with a reason, nothing silently dropped, credentials never written outside `<data_dir>/.env`.

## Locked decisions

1. The runtime owns conversations, state, permissions, execution, recovery, events.
2. Model policy is default + failure-only backups + helper models. No routing.
3. Memory writes go: suggest → permission → label the source → validate.
4. Secrets resolve where used; values never in context or logs.
5. MCP is for talking to outside systems; inside, the native Capability API.
6. Durability comes from SQLite-backed state machines, not hope.
7. Skills (knowledge) and extensions (code) stay two separate tiers.
8. Migration keeps provenance; anything unmappable is archived, not dropped.

## Crate map

| Crate | Role |
|---|---|
| `pantheon-api` | Events, errors, capabilities, model policy, identifiers (bottom leaf) |
| `pantheon-storage` | The record, claims, leases, session search |
| `pantheon-runtime` | Supervisor, recovery, sessions, Runtime API (`serve`/`rpc`) |
| `pantheon-agent` | Agent loop, permission gate, budgets |
| `pantheon-exec` | Process/file execution engine, skills, plugins, supervision |
| `pantheon-tools` | Callable tools + registry (builtins, memory/vault/session-search/skill tools) |
| `pantheon-capability` | Policy/role maps |
| `pantheon-sandbox` | Levels, profiles, enforcement |
| `pantheon-memory` | Layers, write path, backends |
| `pantheon-secrets` | Vaults, broker |
| `pantheon-providers` | Adapters, catalog, backup chain |
| `pantheon-extensions` | Hooks, manifests, runners, compat |
| `pantheon-scheduler` | Triggers, durable claims |
| `pantheon-gateway` | Channels, allowlist, delivery |
| `pantheon-swarm` | Spawn caps |
| `pantheon-mcp` | Token → capability projection |
| `pantheon-migration` | Source importers |
| `pantheon-tui` | Terminal application: interactive session + non-interactive commands |

## See also

- [Contributing](contributing.md): boundaries and change mechanics
- [Decisions](decisions/): the records behind the locks above
- [Terminal reference](../reference/terminal.md): the surface this architecture serves
