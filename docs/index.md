# Pantheon docs

> The model is replaceable. The agent should not be.

Pantheon is a durable agent runtime. The model reasons and generates language; Pantheon provides everything around it — lifecycle, state, policy, execution, memory, recovery, events — so an agent persists across model changes, restarts, and interruptions.

## Start here

- [Getting started](getting-started.md) — install, set up, run your first session.

## User guide

| Page | Contents |
|---|---|
| [Sessions](user-guide/sessions.md) | Terminal interface, one-shot runs, reading back what happened |
| [Agents](user-guide/agents.md) | Identities, profiles, collaboration |
| [Memory](user-guide/memory.md) | Layers, trust, learning into skills |
| [Runs](user-guide/runs.md) | Lifecycle, approvals, recovery, pipelines, scheduling |
| [Channels](user-guide/channels.md) | Terminal, web, Discord, Telegram |
| [Providers](user-guide/providers.md) | Models, fallbacks, custom endpoints |
| [Extensions](user-guide/extensions.md) | Plugins, hooks, capability gating |

## Reference

| Page | Contents |
|---|---|
| [Terminal](reference/terminal.md) | Every verb, flag, exit code, environment variable |
| [Configuration](reference/configuration.md) | `config.toml` fields, secrets, data directory |
| [Troubleshooting](reference/troubleshooting.md) | Error codes, common issues, diagnostics |

## Developer

| Page | Contents |
|---|---|
| [Architecture](developer/architecture.md) | System design, crate map, locked decisions |
| [Contributing](developer/contributing.md) | Boundaries, tests, adding a verb or tool |
| [TUI states](developer/tui.md) | Interface state matrix (internal spec) |
| [Decisions](developer/decisions/) | Architecture decision records (internal) |

## Principles

Persistent agents. Runtime authority over model authority. Experience becomes knowledge. Knowledge is not authority. Everything important is recoverable. One environment, many agents. Interfaces are replaceable.
