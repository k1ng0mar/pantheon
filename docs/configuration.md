# Configuration

Everything persistent lives in `<data-dir>/config.toml`. Setup writes it,
doctor validates it, chat reads it. Flags and environment variables always
override the config file.

## Full reference

```toml
profile = "dev"              # free-form label, informational

[model]
provider = "openai"          # catalog name or any OpenAI-compatible base URL
model = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"   # env var NAME; the value never lands here

[[model.fallbacks]]          # optional, ordered, failure-only
provider = "local"
model = "llama3.2"

policy = "coder"             # reader | coder | coder_memory  (top-level key)

[memory]
backend = "native"           # native | http (options below)
[memory.options]             # backend-specific (e.g. url for http)

[tools]
packs = ["core"]             # enabled built-in tool packs
plugins = []                 # plugin names to auto-start

[server]
port = 18789                 # AG-UI server port (0 = auto-assign)
host = "127.0.0.1"           # bind address
```

## Policy presets

| Preset | Filesystem | Shell | Git | Memory write | Notes |
|---|---|---|---|---|---|
| `reader` | read | no | read | no | Nothing executes. Safe for untrusted tasks. |
| `coder` (default) | read/write | execute | read/write, push needs approval | no | The working default. |
| `coder_memory` | same as coder | yes | same as coder | yes | Agent can persist memory. |

Policies are code-defined presets, not free-form config. This is
deliberate: free-form capability config is how agents end up with more
access than anyone intended. To change what a preset allows, change the
preset in code (pantheon-core/src/capability.rs), not the config.

## Secret handling

The config file contains env var names, never values:

```toml
api_key_env = "OPENAI_API_KEY"
```

At session start the runtime resolves `std::env::var("OPENAI_API_KEY")`.
If it is unset, chat fails with a check naming the variable. Rules:

- Only `env` sourcing exists. A `source = "raw"` style is rejected by
  validation by design.
- The key travels only in the transport Authorization header. It never
  enters prompts, events, or the ledger.
- `pantheon doctor` verifies the env var is set in the current shell.

For more than API keys (database passwords, tokens), the secrets broker in
pantheon-secrets provides encrypted-file and env vaults with
injection-at-the-execution-boundary semantics. It is implemented but not
yet wired to chat; see ARCHITECTURE.md section 13.

## Model selection order

For `pantheon chat`, the default model resolves as:

1. `--provider` / `--model` flags
2. `config.toml [model]`
3. `PANTHEON_PROVIDER` / `PANTHEON_MODEL`
4. Built-in default (`local` / `llama3.2`)

Fallbacks from `[model.fallbacks]` are used only when the default fails
with a retryable error (network, 5xx, quota). The agent never chooses
models and there is no routing: the first entry that works wins, in policy
order.

## Memory backend selection

`native` is the bundled SQLite store; it has no options. Backends are
registered in a registry (`pantheon-memory/src/backend.rs`); the http
kind is defined but no HTTP memory backend is constructed by the CLI
today, so `native` is the practical choice. `pantheon memory backend
list` shows what the running build actually registered. The selected
backend is mirrored in `memory-backend.toml` because that file is what
the memory verbs read; setup keeps the two in sync.

## Validation

`pantheon doctor` checks, in order:

1. config.toml parses (TOML error includes file, line, column)
2. required fields are non-empty
3. `api_key_env` is a plausible env var name AND is set in this shell
4. memory backend is non-empty
5. server.host is a bindable address form

Each failure reports `section`, `status: fail`, a human detail, and a fix
string. Doctor exits 1 when anything failed; warnings (like unknown
plugin hooks) do not affect the exit code.
