# Pantheon

<p align="center">
  <strong>The durable runtime for AI agents.</strong><br>
  <em>The model proposes. Pantheon disposes.</em>
</p>

<p align="center">
  <a href="https://github.com/k1ng0mar/pantheon/actions/workflows/ci.yml"><img src="https://github.com/k1ng0mar/pantheon/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/k1ng0mar/pantheon/releases"><img src="https://img.shields.io/github/v/release/k1ng0mar/pantheon?display_name=tag" alt="Latest release"></a>
  <img src="https://img.shields.io/badge/License-MIT-blue.svg" alt="MIT License">
  <img src="https://img.shields.io/badge/Rust-2021-orange?logo=rust" alt="Rust 2021">
</p>

## The problem

Models are stateless oracles. You hand one a conversation and it returns the next token — fluent, confident, and amnesiac. Kill the process and the agent dies with it: no record of what it decided, no record of what it touched, no way to resume except replaying the transcript and hoping.

A conversation is not durable state. A prompt is not a policy. And a model that can run shell commands with your UID because it *said so in a tool call* is not an agent you own — it's a very eloquent guest with root.

Every agent harness eventually rediscovers this. The ones that don't are demos.

## The thesis

**Runtime authority over model authority.** The model reasons; the runtime decides what happens. Pantheon sits between the model and the world and enforces three separations:

- **Policy is enforced at execution, not at prompt time.** The model never sees a policy it can argue with. Tool calls pass a capability gate in the runtime: allow, deny, or pause for human approval. A jailbroken system prompt changes nothing — the gate isn't in the prompt.
- **The SQLite event ledger is the source of truth.** Every turn, tool call, approval, and failure is appended to a local, WAL-mode SQLite database. Sessions survive crashes, restarts, and provider outages because the agent's state is the ledger, not the context window. Rewind in the UI never rewrites history — the ledger is append-only.
- **Memory is provenance-aware, not a suggestion box.** Every memory carries its trust tier and origin. External tiers are clamped to Untrusted; writes are scanned for secret patterns before they land. Memory provides context; it never grants authority.

Pantheon is written in Rust, runs as a single binary, and keeps its state in your home directory. No database server. No sidecar. No cloud account required.

## How it works

### 30 seconds

```sh
# Install (Linux/macOS; Windows has an equivalent .ps1)
curl -fsSL https://raw.githubusercontent.com/k1ng0mar/pantheon/master/install.sh | bash

# Configure: names the env var, never the key
pantheon setup --yes --provider openai --model gpt-4o-mini --api-key-env OPENAI_API_KEY
export OPENAI_API_KEY=<your-key>

# Open the terminal UI
pantheon
```

Scripts and automation use `pantheon run "task"`; inspect anything with `pantheon runs <run_id>`; diagnose with `pantheon doctor`. Build from source with `cargo build --release` (Rust 2021, SQLite bundled).

### The turn loop

```text
┌─────────┐   ┌──────────┐   ┌──────────────┐   ┌───────────┐
│   TUI   │──▶│ Runtime  │──▶│ Agent engine │──▶│ Providers │
│ tabs ·  │   │(session  │   │ (turn loop,  │   │ OpenAI /  │
│ status  │   │ runs ·   │   │  unique call │   │ Anthropic │
│ bar ·   │   │ approvals│   │  IDs, run    │   │ + custom  │
│ timeline│   │ recovery)│   │  budgets)    │   │ endpoints │
└─────────┘   └────┬─────┘   └──────────────┘   └───────────┘
                   │                                  │
                   │        ┌──────────────┐           │
                   ├───────▶│ Capability   │◀──────────┘
                   │        │ gate         │  allow · deny ·
                   │        └──────┬───────┘  approve
                   │               │
                   │        ┌──────▼───────┐
                   │        │ Tools (shell,│
                   │        │ fs, memory,  │
                   │        │ MCP client…)│
                   │        └──────┬───────┘
                   │               │
                   │        ┌──────▼───────┐
                   └───────▶│ Sandbox      │
                            │ (fail-closed │
                            │ isolation)   │
                            └──────┬───────┘
                                   │
                            ┌──────▼───────┐
                            │ SQLite event │
                            │ ledger (WAL) │
                            └──────────────┘
```

Surfaces (TUI, web, Discord, Telegram gateways, CLI) all speak to the same session runtime. The model proposes tool calls; the capability gate, sandbox, and ledger decide what they become.

## Mechanisms — and what each one buys you

**The ledger.** Every lifecycle event — turn started, tool called, approval requested, cost stamped — is appended to a SQLite database in WAL mode with a busy timeout. Ledger writes are redacted at append time: log lines are scrubbed, and blocked-command errors carry a digest and rule name, never raw text. *Trust:* a crashed process loses nothing. `pantheon runs <id>` shows exactly what happened, and resuming a run is replay, not reconstruction.

**Approvals as first-class control.** The capability decision isn't binary: allow, deny, or *ask a human*. Approvals pause the run and resume it — settling the whole batch, not just one call — from the TUI, the web UI, or the messaging gateways. *Trust:* you can hand an agent real capabilities (shell, network, memory writes) and keep a human veto on the dangerous ones, without babysitting every step.

**Fail-closed sandbox.** Tool execution requests an isolation level. If the sandbox wrapper can't establish it, the command does not run — there is no fallback to raw host execution. *Trust:* "the sandbox was unavailable" can never silently become "the agent ran it on your machine anyway."

**The danger gate.** Before shell text reaches execution, a pre-gate blocks `sh -c`/`eval` wrappers, `${IFS}` tricks, command substitution, backticks, `find -delete`/`-exec`, and obfuscated `rm`. Blocked errors name the rule, not the command — so a denied command containing a token can't leak it into the ledger. *Trust:* the most common prompt-injection-to-shell paths are dead before they reach your filesystem.

**Capability-gated tools.** Every tool declares the capability it needs; the runtime policy (`reader`, `coder`, `coder_memory`) decides per operation. File tools are confined to the workspace root with `O_NOFOLLOW`, and plugin names can't squat builtins. *Trust:* a compromised or confused model can't reach `~/.ssh` because the path boundary isn't in the prompt — it's in the syscall-adjacent layer.

**Durable scheduler claims.** Cron (with a fixed Vixie DOM/DOW interpretation), one-shot, and interval jobs register with validation and claim their ticks through a unified claims table — surviving restarts without double-firing. Per-job timeouts (default 600s), overlap policies (skip/replace/queue), and HMAC-SHA256-verified webhooks. *Trust:* scheduled work is a runtime guarantee, not a process that has to stay alive.

**Cost ceilings that trip.** Usage is summed per turn — including cached tokens — and stamped onto the outcome. Run-level budgets (`max_tokens`, `max_cost_cents`, seeded from the ledger) actually halt the run when hit. *Trust:* an agent can't quietly burn $40 while you sleep. The ceiling is enforced by the code that counts, not the model that spends.

**Trust-tiered memory.** Five memory layers plus runtime state, each with byte budgets and trust-aware eviction. External tiers (remote servers, imported files) are clamped to Untrusted regardless of what they claim; writes are scanned for secret patterns before landing. *Trust:* memory retrieved from outside can't launder itself into a trusted instruction, and your API keys don't end up in the memory store.

**Transactional migration.** Importing state (detect → plan → approve → apply → validate) runs as stage → validate → atomic commit, with automatic rollback and byte/file budgets. *Trust:* migrating from another harness is a transaction, not a hope — a failed import leaves your existing state untouched.

## Honest comparison

| | Pantheon | Hermes Agent | OpenClaw | Claude Code |
|---|---|---|---|---|
| Language | Rust, single binary | Python | TypeScript/Node | Proprietary CLI |
| Core thesis | Runtime authority over model authority | Self-improving loop (agent authors its own skills) | Personal assistant on every channel | Best-in-class coding agent |
| Durable state | SQLite event ledger; runs resume after crashes | SQLite `state.db` + markdown memories with char caps | Local files / gateway state | Session resume within the CLI |
| Policy enforcement | Capability gate at execution; approvals pause/resume | Deny globs + hardline blocklist (vendor: "not a security boundary") | Allowlist-based tool config | Per-tool permission prompts |
| Sandbox | Fail-closed isolation | Documented as not containment | Process-level | Sandboxed tool execution |
| Cost control | Run budgets enforced by the runtime | Usage summaries per model | Varies | Usage dashboard |
| Memory | Trust-tiered, provenance-aware, secret-scanned | Self-authored skills + curated memory | Persistent memory files | Project memory files |
| Messaging | Discord + Telegram gateways (real REST outbound, dead-letter) | Telegram gateway | WhatsApp, Telegram, Discord, Slack, Signal, … | None (terminal only) |
| Scheduler | Durable cron/one-shot/interval with claims | Cron jobs | Cron | None built in |
| MCP | Client (stdio, capability-gated `tools/call`) — **server: planned** | MCP support via skills | MCP support | MCP client |
| Browser control | **Planned** | Via skills | First-class browser tool | None |
| Model choice | OpenAI, Anthropic, custom endpoints + fallbacks | Configurable providers | Local + cloud providers | Anthropic models |

Hermes/OpenClaw cells summarize their public docs and positioning as of September 2026; details move fast, verify before deciding. The "planned" rows are deliberate: Pantheon claims nothing it hasn't built.

## Crate map

| Crate | One line |
|---|---|
| `pantheon` | Façade: re-exports every library crate under `pantheon::…` |
| `pantheon-tui` | The terminal product — TUI, CLI verbs, session tabs, status bar |
| `pantheon-runtime` | Supervisor: run lifecycle, quotas, recovery, checkpointing |
| `pantheon-agent` | Agent engine: the model turn loop with run budgets |
| `pantheon-api` | Bottom protocol leaf: commands, events, shared types |
| `pantheon-exec` | Execution engine: process/fs/git surface, danger gate, confinement |
| `pantheon-sandbox` | Sandbox hierarchy; isolation levels, fail-closed |
| `pantheon-capability` | Capability plane: enforces policy decisions at execution |
| `pantheon-tools` | Named, schema'd, capability-gated tools (shell, fs, memory, …) |
| `pantheon-mcp` | MCP client (stdio): `initialize` / `tools/list` / `tools/call` |
| `pantheon-memory` | Memory plane: five layers + runtime state, trust tiers |
| `pantheon-secrets` | Secrets broker: values resolved at execution, never in prompts |
| `pantheon-storage` | SQLite event-sourced execution ledger |
| `pantheon-scheduler` | Durable jobs: cron, one-shot, interval, webhooks |
| `pantheon-gateway` | Discord/Telegram gateways: routing, queues, dead-letter |
| `pantheon-swarm` | Delegation with runtime-owned caps (depth, concurrency, budget) |
| `pantheon-extensions` | Hooks and plugins: spec, `plugin.yaml` loader, Python/JS runners |
| `pantheon-migration` | Import flows: detect → plan → approve → apply → validate |
| `pantheon-providers` | Provider adapters: OpenAI, Anthropic, custom endpoints, fallbacks |

## Configuration

`config.toml` in the data dir (`$PANTHEON_DATA_DIR` or `~/.pantheon/`) is the single source of truth. Setup writes it; every verb reads it. Secrets are never in it — config names env vars, the runtime resolves them at the execution boundary.

```toml
[model]
provider    = "openai"
model       = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"   # name only, never the key
reasoning   = "high"             # off|minimal|low|medium|high|xhigh|max

[[model.fallbacks]]              # ordered, failure-only, runtime-controlled
provider = "anthropic"
model    = "claude-sonnet-4-5"

policy = "coder"                 # reader | coder | coder_memory

[memory]
backend = "native"

[secrets]
env_allowlist = ["MY_API_KEY", "PANTHEON_*"]   # empty by default: fail closed

[server]
port = 18789
host = "127.0.0.1"

[agents.default]
display_name = "Default"
policy       = "coder_memory"
```

Env overrides: `PANTHEON_PROVIDER`, `PANTHEON_MODEL`, `PANTHEON_REASONING`, `PANTHEON_DATA_DIR`, plus `PANTHEON_<AUX>_PROVIDER`/`_MODEL` for auxiliaries (judge, compression, embeddings, …). Gateway tokens live in `<data_dir>/.env` (`PANTHEON_DISCORD_TOKEN`, `PANTHEON_TELEGRAM_BOT_TOKEN`, `PANTHEON_GATEWAY_ALLOW`), never in config. Full reference: [docs/reference/configuration.md](docs/reference/configuration.md).

## Guides

| | |
|---|---|
| [Getting started](docs/getting-started.md) | Install, configure, first session |
| [Agents](docs/user-guide/agents.md) | Identities, profiles, collaboration |
| [Runs](docs/user-guide/runs.md) | Lifecycle, approvals, recovery, pipelines, scheduling |
| [Memory](docs/user-guide/memory.md) | Scope, provenance, trust, skills |
| [Providers](docs/user-guide/providers.md) | Models, fallbacks, custom endpoints |
| [Channels](docs/user-guide/channels.md) | Terminal, web, Discord, Telegram |
| [Extensions](docs/user-guide/extensions.md) | Hooks, tools, plugin behavior |
| [Configuration](docs/reference/configuration.md) | Every field, the data dir, secrets |
| [Troubleshooting](docs/reference/troubleshooting.md) | Diagnostics and common issues |

The repo also ships a CLI evaluation harness with isolated sandboxes and regression cases ([eval/README.md](eval/README.md)). CI runs `cargo fmt`, Clippy, the workspace test suite, and a release-binary smoke check.

## Contributing

See [docs/developer/contributing.md](docs/developer/contributing.md) for the workspace layout and development workflow. The design decisions behind the crate boundaries live in [docs/developer/architecture.md](docs/developer/architecture.md). Pull requests run the full CI gate: formatting, Clippy with warnings denied, and tests.

## License

MIT (declared in the workspace `Cargo.toml`).
