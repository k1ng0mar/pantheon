# Getting started

## Prerequisites

- A Rust toolchain (rustup, edition 2021). Any recent stable works.
- That is all. SQLite is bundled; there is no async runtime, no container
  requirement, no external services for the core path.

## Build

```sh
git clone <repo> pantheon && cd pantheon
cargo build --release          # or cargo build for a debug binary
```

The binary is `target/release/pantheon` (or `target/debug/pantheon`).

## First setup

```sh
pantheon setup
```

Five questions: profile name, provider, model, API key env var name, and
execution policy. Answer them and you get `~/.pantheon/config.toml`.

Non-interactive (scripts, CI):

```sh
pantheon setup --yes \
  --profile dev \
  --provider openai \
  --model gpt-4o-mini \
  --api-key-env OPENAI_API_KEY \
  --policy coder \
  --memory native \
  --packs core
```

Every question has a flag. `--yes` accepts defaults for anything you did
not flag. Secrets are never written to the config file: setup stores the
env var NAME and resolves it at runtime.

## First chat

```sh
export OPENAI_API_KEY=sk-...
pantheon chat "what files are in this directory"
```

The model can call tools (shell, file read/write, git) according to the
policy. Each run gets an id like `run_1690000000000_ab12`; everything it did
is recorded in the ledger.

```sh
pantheon status run_1690000000000_ab12   # current state
pantheon explain run_1690000000000_ab12  # full event replay in words
```

## Where data lives

`$PANTHEON_DATA_DIR` or `~/.pantheon/`:

| File | What it is |
|---|---|
| `config.toml` | Your configuration (setup writes it, doctor validates it) |
| `ledger.db` | Event ledger + durable operations + run leases + artifacts |
| `memory.db` | Five-layer memory store |
| `memory-backend.toml` | Which memory backend is selected |
| `extensions/` | Loaded plugins |
| `safewrite/` | File-edit checkpoints and the write journal |
| `gateway/` | Channel cursors (Telegram update offsets, Discord) |

## Next steps

- Point it at a real model: docs/configuration.md
- Understand what the agent is allowed to do: docs/configuration.md
  (policies) and docs/plugins.md (capability gating)
- Long-running or resumable work: docs/runs-and-recovery.md
- Multi-step tasks with human checkpoints: docs/pipelines.md
- Talk to it from Discord/Telegram: docs/channels.md

## Verify your install

```sh
pantheon doctor
```

Exit 0 means config, model key, ledger, memory store, and plugins are all
healthy. Each failed check names its fix.
