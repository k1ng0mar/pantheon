# Pantheon

<p align="center">
  <strong>The durable runtime for AI agents.</strong><br>
  <em>The model reasons. Pantheon owns what happens next.</em>
</p>

<p align="center">
  <a href="https://github.com/k1ng0mar/pantheon/actions/workflows/ci.yml"><img src="https://github.com/k1ng0mar/pantheon/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/k1ng0mar/pantheon/releases"><img src="https://img.shields.io/github/v/release/k1ng0mar/pantheon?display_name=tag" alt="Latest release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/k1ng0mar/pantheon" alt="MIT License"></a>
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/Rust-2021-orange?logo=rust" alt="Rust 2021"></a>
</p>

Pantheon is an open-source agent runtime built around one separation: **reasoning is not authority, and a conversation is not durable state.** Models propose actions. Pantheon applies policy, records outcomes, and keeps work resumable. Agent identity and history belong to the runtime—not to a model, prompt, or interface.

**Persistent agents** · **Recoverable runs** · **Capability-governed tools** · **Provenance-aware memory** · **Replaceable models and interfaces**

## Why Pantheon

Most agent systems center the model loop. Pantheon centers the runtime that makes an agent dependable across turns and failures. Runs have durable state. Tool permissions are checked at the point of execution. Memory carries scope and provenance. Interfaces and model providers can change without replacing the agent.

The design goal is an agent whose work can be inspected, governed, and resumed—not just a system that produces the next response.

## Capabilities

| Area | What Pantheon does |
|---|---|
| **Runs and recovery** | Persists lifecycle events to a SQLite ledger. Supports leases, resumable runs, cancellation, approvals, bounded budgets, pipelines, and scheduled work. |
| **Policy and execution** | Gates tool calls through capability policy: allow, deny, or request approval. Runs tools under configured sandbox boundaries. |
| **Agents and collaboration** | Gives agents separate instructions, profiles, memory namespaces, and policies. Supports delegation with runtime caps on depth, concurrency, and budgets. |
| **Memory** | Scopes memory globally, per agent, project, and task. Writes use a common policy, provenance, and validation path. Memory provides context; it never grants authority. |
| **Providers and surfaces** | Keeps provider choice and fallback behavior in the runtime. Terminal, web, Discord, and Telegram surfaces use the same session runtime. |
| **Extensibility and migration** | Adds hooks and tools through extensions, and provides detect/plan/apply/validate flows for supported imports. |
| **Secrets** | Resolves secret values at execution time. Configuration stores secret references; values stay out of prompts and the event ledger. |

## Architecture

```text
 CLI / TUI / Web / Gateways
             │
      Runtime API + events
             │
  Supervisor ── Runs · policy · approval · recovery
      ┌──────┴──────┐
 Agent engine    Execution engine
      └──────┬──────┘
        Capability policy
             │
 Providers · Memory · Secrets · Extensions
             │
        SQLite ledger
```

The supervisor owns lifecycle and recovery. The agent loop handles model turns. The execution engine invokes tools only through the capability boundary. The ledger records canonical events used to inspect and resume work. SQLite is bundled; no database server or external vector store is required by default.

See [Architecture](docs/developer/architecture.md) for the crate map and the design decisions behind these boundaries.

## Quick start

### Install

Linux or macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/k1ng0mar/pantheon/master/install.sh | bash
```

Windows (PowerShell):

```powershell
iwr https://raw.githubusercontent.com/k1ng0mar/pantheon/master/install.ps1 -useb | iex
```

Installers fetch prebuilt release binaries; a compiler is not required.

### Configure and start

```sh
pantheon setup --yes --provider openai --model gpt-4o-mini \
  --api-key-env OPENAI_API_KEY
export OPENAI_API_KEY=sk-...
pantheon
```

Pantheon stores the environment variable name in configuration. The key value remains in the environment.

Inspect what happened and diagnose the installation:

```sh
pantheon runs <run_id>
pantheon doctor
```

For scripts and automation, use `pantheon run`; for details see [Getting started](docs/getting-started.md) and the [terminal reference](docs/reference/terminal.md).

## One runtime, multiple surfaces

| Surface | Use |
|---|---|
| Terminal/TUI | Interactive sessions, approvals, and run inspection |
| Web | Local AG-UI interface backed by the same session runtime |
| Discord / Telegram | Gateway with sender allowlisting and durable reply delivery |
| CLI | One-shot runs, automation, scheduling, migration, diagnostics |

## Guides and reference

| Guide | Topics |
|---|---|
| [Getting started](docs/getting-started.md) | Install, configure, and start a session |
| [Agents](docs/user-guide/agents.md) | Identities, profiles, collaboration |
| [Runs](docs/user-guide/runs.md) | Lifecycle, approvals, recovery, pipelines, scheduling |
| [Memory](docs/user-guide/memory.md) | Scope, provenance, trust, skills |
| [Providers](docs/user-guide/providers.md) | Models, fallbacks, custom endpoints |
| [Channels](docs/user-guide/channels.md) | Terminal, web, Discord, Telegram |
| [Extensions](docs/user-guide/extensions.md) | Hooks, tools, plugin behavior |
| [Configuration](docs/reference/configuration.md) | Configuration fields, data directory, secrets |
| [Troubleshooting](docs/reference/troubleshooting.md) | Diagnostics and common issues |
| [Contributing](docs/developer/contributing.md) | Workspace structure and development workflow |

## Evaluation and verification

The repository includes a CLI evaluation harness with isolated sandboxes and regression cases for run lifecycle, ledger behavior, extensions, and output compaction. See [eval/README.md](eval/README.md) for its scope and usage. CI runs formatting, Clippy, the Rust workspace test suite, and a release-binary smoke check.

## Build from source

```sh
git clone https://github.com/k1ng0mar/pantheon.git
cd pantheon
cargo build --release
```

Requires the Rust 2021 toolchain. SQLite is bundled.

## License

MIT. See [LICENSE](LICENSE).
