# Getting started

Install Pantheon, tell it which AI to use, start talking.

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

Want to build it yourself? Clone the repo and run `cargo build --release`. It needs Rust, nothing else.

## Set up

```sh
pantheon setup
```

It asks a few questions: which AI provider, which model, where your API key lives, and how strict you want permissions to be. That creates `~/.pantheon/config.toml`.

If you are scripting the install (CI, a fresh machine), you can answer the questions with flags instead:

```sh
pantheon setup --yes --provider openai --model gpt-4o-mini \
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
