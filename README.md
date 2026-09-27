# Pantheon

A durable agent runtime. The model reasons; Pantheon owns lifecycle, state, policy, execution, recovery, and events — so agents survive model changes, restarts, and interruptions.

## Install

Linux or macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/k1ng0mar/pantheon/master/install.sh | bash
```

Windows (PowerShell):

```powershell
iwr https://raw.githubusercontent.com/k1ng0mar/pantheon/master/install.ps1 -useb | iex
```

Prebuilt binary from GitHub Releases, needs only `curl` and `tar`. No toolchain required.

## Quickstart

```sh
pantheon setup --yes --provider openai --model gpt-4o-mini --api-key-env OPENAI_API_KEY
export OPENAI_API_KEY=sk-...
pantheon              # interactive session (the terminal interface)
pantheon runs <run_id>  # why everything happened
pantheon doctor         # is everything healthy
```

## Docs

- [Getting started](docs/getting-started.md) — install, setup, first session
- [Docs index](docs/index.md) — agents, memory, runs, channels, providers
- [CLI reference](docs/reference/cli.md) — every verb, flag, exit code
- [Configuration](docs/reference/configuration.md) — `config.toml` fields
- [Architecture](docs/developer/architecture.md) — system design, crate map
- [Contributing](docs/developer/contributing.md) — boundaries, tests, how to add a verb/tool

## Commands

```sh
pantheon          # start Pantheon
pantheon doctor   # diagnose your installation
pantheon update   # update Pantheon
pantheon --help   # show available commands
```

## Build from source

```sh
git clone https://github.com/k1ng0mar/pantheon.git
cd pantheon
cargo build --release
```

Requires a Rust toolchain (edition 2021). No database server: SQLite is bundled.

## License

MIT. See [LICENSE](LICENSE).
