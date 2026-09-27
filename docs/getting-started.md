# Getting started

Install Pantheon, point it at a model, start working.

## Install

Linux or macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/k1ng0mar/pantheon/master/install.sh | bash
```

Windows (PowerShell):

```powershell
iwr https://raw.githubusercontent.com/k1ng0mar/pantheon/master/install.ps1 -useb | iex
```

A prebuilt binary from GitHub Releases, linked into `~/.local/bin`. Needs only `curl` and `tar`. Re-running never overwrites your config. Pin a release with `PANTHEON_VERSION=v0.1.0`.

Prefer source? `git clone` + `cargo build --release` (Rust edition 2021). SQLite is bundled; nothing else to run.

## Set up

```sh
pantheon setup
```

Answer five questions — profile, provider, model, API key location, policy — and you get `~/.pantheon/config.toml`. Scripted setups pass flags instead of answering:

```sh
pantheon setup --yes --provider openai --model gpt-4o-mini \
  --api-key-env OPENAI_API_KEY --policy coder
```

Only the key's *name* goes in the config; the value stays in your environment.

## Run it

```sh
export OPENAI_API_KEY=sk-...
pantheon
```

Talk to it. Leave. Come back — the work remains, with its history. `/help` lists session commands.

No terminal? No session. For scripts and CI:

```sh
pantheon run --taskID t1 --say "summarize these logs" --deliver session
```

## Check health

```sh
pantheon doctor
```

Exit 0 means config, key, ledger, memory, and plugins are healthy. Anything else names its fix.

## Next steps

- [Sessions](user-guide/sessions.md) — the interface, approvals, inspecting runs
- [Agents](user-guide/agents.md) — identities that persist
- [Runs](user-guide/runs.md) — lifecycle, recovery, pipelines, scheduling
- [Configuration](reference/configuration.md) — every `config.toml` field
