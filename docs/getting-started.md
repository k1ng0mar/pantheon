# Getting started

Pantheon gives you an AI agent you can shape around your own work. Install it, connect a model, start talking.

## Install

Linux or macOS:

```sh
curl -fsSL https://raw.githubusercontent.com/k1ng0mar/pantheon/master/install.sh | bash
```

Windows (PowerShell):

```powershell
iwr https://raw.githubusercontent.com/k1ng0mar/pantheon/master/install.ps1 -useb | iex
```

That downloads the program and puts it in `~/.local/bin`. Running it again later never wipes your settings.

The installer verifies the published `.sha256` file. That is an integrity check, not a signature: the checksum comes from the same place as the archive, so it catches a corrupt download, not a malicious one. Published releases also carry minisign signatures, and `pantheon update` checks them against the pinned public key before it swaps the binary. Unsigned releases are refused. See [release-signing.md](release-signing.md) for the key and the install path if verification fails. Until a release is published, use the install command above or build from source.

Want to build it yourself? Clone the repo and run `cargo build --release`. It needs Rust, nothing else.

## Sandboxing by platform

The shell and skill tools request an isolation level, and the request fails closed when it cannot be met. What "cannot be met" means depends on the platform, so it is worth knowing before you let an agent run commands:

| Platform | `High` (default) | What the fallback gives you |
|---|---|---|
| Linux x86_64 / aarch64 | bubblewrap user namespace, all capabilities dropped, no network unless the call asks, credential files and the data dir masked | `PANTHEON_SANDBOX_FALLBACK=allow` runs directly on the host: scrubbed environment, rlimits, no filesystem or network isolation |
| macOS | no boundary backend: `High` fails closed (`SANDBOX_UNAVAILABLE`) | the same direct spawn: scrubbed environment, rlimits, no isolates |
| Windows | no boundary backend: `High` fails closed | the same direct spawn: scrubbed environment, rlimits, no isolates |

`pantheon doctor` reports which case you are in (`security.exec_sandbox`), on Linux including whether the kernel refuses unprivileged user namespaces (Ubuntu's AppArmor does by default). On macOS and Windows, the isolation claim in the README does not apply: treat shell access as unsandboxed and keep approvals on.

Linux also needs `bwrap` installed; without it every `High` call is refused rather than silently downgraded.

## Set up

```sh
pantheon setup
```

It asks a few questions: which AI provider, which model, where your API key lives, and how strict you want permissions to be. That creates `~/.pantheon/config.toml`.

If you are scripting the install (CI, a fresh machine), you can answer the questions with flags instead:

```sh
pantheon setup --provider openai --model gpt-4o-mini \
 --api-key-env OPENAI_API_KEY --policy coder
```

Your API key itself is never stored in the config file. Only the *name* of the environment variable holding it. Set that variable before you run Pantheon:

```sh
export OPENAI_API_KEY=sk-...
```

## Run it

```sh
pantheon
```

Talk to it. Close it. Come back tomorrow and it remembers. Type `/help` inside to see what it can do.

Want it to do one thing and exit, for scripts or automation?

```sh
pantheon run --taskID t1 --say "summarize these logs" --deliver session
```

## Check that it works

```sh
pantheon doctor
```

It checks your config, your API key, its files, and its memory. If something is wrong, it tells you how to fix it.

## Next steps

- [Sessions](user-guide/sessions.md): the terminal app, permissions, looking back at conversations
- [Agents](user-guide/agents.md): assistants with their own name and memory
- [Runs](user-guide/runs.md): how work happens and survives crashes
- [Configuration](reference/configuration.md): every setting in `config.toml`
