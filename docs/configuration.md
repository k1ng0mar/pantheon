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

[decision]                   # optional: auxiliary decision model (see below)
provider = "local"           # any catalog provider or OpenAI-compatible base URL
model = "qwen2.5:1.5b"
api_key_env = "DECISION_API_KEY"  # env var NAME; optional

[compression]                # optional: auxiliary context-compression model
provider = "local"
model = "summarizer"
api_key_env = "COMPRESSION_API_KEY"  # env var NAME; optional

[stt]                        # optional: speech-to-text service (provider plane,
                             # NOT a model-policy entry — see below)
backend = "command"          # command | openai
[stt.options]
cmd = "whisper-cli"          # local binary; stdout is the transcript
args = "-m ggml-base.en.bin -f {file} -nt"

[tts]                        # optional: text-to-speech service
backend = "command"
[tts.options]
cmd = "piper"                # text on stdin, audio on stdout
args = "--model {voice}"
voice = "en_US-lessac-medium"

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

## Decision model

`[decision]` selects an auxiliary model for the decision layer: route
selection, tool-gate risk scoring, and verification. It is a plain
provider/model pair — any catalog provider, any OpenAI-compatible base
URL, local or hosted. Nothing in the runtime is tied to a specific
decision model.

```toml
[decision]
provider = "openai"
model = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"   # optional; defaults to the provider key env
```

Resolution order (field-wise):

1. `PANTHEON_DECISION_PROVIDER` / `PANTHEON_DECISION_MODEL`
2. `config.toml [decision]`
3. Absent both = the decision layer is off (the default).

Rules:

- The decision model is an *auxiliary*: it never replaces the chat model.
  The host consults it at fixed insertion points and validates every
  answer against live state — confidence is a signal, not permission.
- On any failure (timeout, unreachable, unparseable) the host falls back
  to its defaults; an unrecognizable tool-gate verdict fails closed to
  approval, never to allow. The call is bounded to 10 seconds.
- Keys follow the normal secret rules: `api_key_env` names an env var,
  the value never lands in config, prompts, events, or the ledger.
- Wire mode (OpenAI vs Anthropic) and the provider key env are resolved
  from the catalog exactly as for chat.

## Context compression

`[compression]` selects an auxiliary model that summarizes the oldest
exchanges when a transcript would overflow the model's context window.
Same shape and resolution rules as `[decision]`:

1. `PANTHEON_COMPRESSION_PROVIDER` / `PANTHEON_COMPRESSION_MODEL`
2. `config.toml [compression]`
3. Absent both = compression off; the runtime still trims deterministically
   (oldest tool rows re-compact, oldest exchanges drop), so configuring a
   compressor is quality-of-context, never correctness.

How it behaves on overflow, in order:

1. The oldest exchanges (never the system preamble, never the live turn)
   are rendered row-capped and summarized into one `<compressed_context>`
   note, tagged memory-tier provenance so the model treats it as data.
2. The deterministic fit runs afterward and drops whatever still does not
   fit; if the compressor is unreachable or errors, that fallback is the
   whole story (a warning prints, the run continues).
3. If even the essential rows cannot fit, the run fails with
   `CONTEXT_OVERFLOW` (see troubleshooting).

Each pass is recorded in the ledger as `ContextCompressed`, and any
trimming as `ContextTrimmed`, so `pantheon explain <run>` shows exactly
what the model saw and why. Compression is bounded: 30-second timeout,
100 KB input render cap, summary hard-capped at twice its target.

## Speech services (STT / TTS)

`[stt]` and `[tts]` select speech *services* — a local binary or an
HTTP endpoint — from the provider registry. They are deliberately NOT
auxiliary models: swapping them never touches model policy, exactly like
`[memory]`'s backend selection.

| Backend | Kind | Options |
|---|---|---|
| `command` | Subprocess | `cmd` (required), `args` template with `{file}` `{language}` (stt) or `{voice}` `{format}` (tts), `timeout_secs` (default 120) |
| `openai` | Http | `provider` (required: catalog id or base URL), `model`, `voice` (tts default), optional `api_key_env`-style keys resolve via `PANTHEON_KEY_<PROVIDER>` |

Command backends: STT prints the transcript to stdout (whisper.cpp
style); TTS receives the text on stdin and writes audio bytes to stdout
(piper, `espeak-ng --stdout`). Every run is wall-clock bounded — a hung
binary fails with `VOICE_TIMEOUT`, never a stuck session. HTTP backends
speak the OpenAI-compatible `/audio/transcriptions` (multipart) and
`/audio/speech` (JSON) shapes, resolved through the catalog.

Validation requires `cmd` for `command` and `provider` for `openai`.
Absent sections mean no speech capability — nothing is auto-enabled.
Consumers (gateway voice notes, CLI verbs) wire up in a later pass; the
seam and its backends are testable standalone.

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
3. `api_key_env` (model or decision) is a plausible env var name AND is set in this shell
4. memory backend is non-empty
5. server.host is a bindable address form

Each failure reports `section`, `status: fail`, a human detail, and a fix
string. Doctor exits 1 when anything failed; warnings (like unknown
plugin hooks) do not affect the exit code.
