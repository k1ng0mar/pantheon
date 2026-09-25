# Pantheon

A Rust agent runtime. Small at the center, huge at the edges.

The model is not the runtime. The model is one replaceable component inside
it. The runtime owns lifecycle, state, policy, execution, capabilities,
recovery, and events. Agents never pick models.

## Documentation map

| Document | What it covers |
|---|---|
| [docs/product-overview.md](./docs/product-overview.md) | Product definition, user value, current experience, and planned system |
| [ARCHITECTURE.md](./ARCHITECTURE.md) | System design, crate map, locked decisions, status of every subsystem |
| [docs/getting-started.md](./docs/getting-started.md) | Install, setup, first chat, config reference |
| [docs/cli.md](./docs/cli.md) | Every verb, every flag, exit codes, environment variables |
| [docs/configuration.md](./docs/configuration.md) | config.toml fields, secret handling, backend selection |
| [docs/runs-and-recovery.md](./docs/runs-and-recovery.md) | Run lifecycle, approvals, leases, recovery, cancellation |
| [docs/pipelines.md](./docs/pipelines.md) | The orchestration pipeline, gates, evaluator loop |
| [docs/channels.md](./docs/channels.md) | Discord, Telegram, the AG-UI web surface, gateways |
| [docs/plugins.md](./docs/plugins.md) | Extension format, hooks, capability gating, doctor |
| [docs/memory.md](./docs/memory.md) | Memory layers, the write path, MEMORY.md sync |
| [docs/troubleshooting.md](./docs/troubleshooting.md) | Error codes and what to do about them |
| [docs/contributing.md](./docs/contributing.md) | Crate boundaries, testing rules, how to add a verb/tool/event |

## Status

Working today: chat with tool loops against any OpenAI-compatible or
Anthropic provider, crash recovery, durable operations, human approval
gates, plugin loading and hooks, memory with provenance, Discord and
Telegram surfaces, the AG-UI local web client, a setup wizard, a system
doctor, and the six-stage orchestration pipeline.

Implemented but not yet wired to a CLI surface: scheduler, secrets broker,
MCP projection, migration import, sandbox profiles, swarm caps. See
ARCHITECTURE.md sections 3, 10, 13, 15, 21, 23 for the exact state.

## Install

Linux or macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/pantheon-agent/pantheon/main/install.sh | bash
```

Windows (PowerShell):

```powershell
iwr https://raw.githubusercontent.com/pantheon-agent/pantheon/main/install.ps1 -useb | iex
```

The script installs Rust (if missing), builds from source, and links
into `~/.local/bin`. Requires `sh` and `git` at minimum on Unix;
Git and curl on Windows.

Or build from source:

```sh
git clone https://github.com/pantheon-agent/pantheon.git
cd pantheon
cargo build --release
```

Requires a Rust toolchain (edition 2021). No database server: SQLite is
bundled. No async runtime: everything is std threads.

## A ten-minute tour

```sh
pantheon setup --yes --provider openai --model gpt-4o-mini --api-key-env OPENAI_API_KEY
export OPENAI_API_KEY=sk-...
pantheon                           # interactive session (or: pantheon chat "...")
pantheon explain <run_id>          # why everything happened
pantheon doctor                    # is everything healthy
```

The default policy lets the model run shell commands in your working
directory, behind a dangerous-pattern pre-gate. Read
docs/configuration.md before pointing it at anything you care about.

## Design position (short version)

Event-sourced everything: every run is a sequence of events in SQLite, which
is why /explain, crash recovery, and audit export all read the same rows.
Capability-gated everything: tools declare the capability they need, the
policy decides allow/deny/approve. Human approval parks a run durably; a
denial becomes a transcript result, not a crashed run. Durability is
versioned CAS state machines, not hope.
