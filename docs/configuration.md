# Configuration

Everything persistent lives in `<data-dir>/config.toml`. Setup writes it,
doctor validates it, chat reads it. Flags and environment variables always
override the config file.

API key *values* live in `<data-dir>/.env` (written by `pantheon model`,
loaded at every startup without overriding exported variables).
`config.toml` stores only env-var *names* (`api_key_env`), never secrets.

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

[judge]                      # optional: auxiliary judge model (see below)
provider = "local"           # any catalog provider or OpenAI-compatible base URL
model = "qwen2.5:1.5b"
api_key_env = "DECISION_API_KEY"  # env var NAME; optional

[compression]                # optional: auxiliary context-compression model
provider = "local"
model = "summarizer"
api_key_env = "COMPRESSION_API_KEY"  # env var NAME; optional

[title_gen]                  # optional: auxiliary session-title model
provider = "local"           # absent = `auto` (titles use the default model)
model = "namer"
api_key_env = "TITLEGEN_API_KEY"  # env var NAME; optional

# Five more aux slots with the identical provider/model/api_key_env shape,
# each defaulting to `auto` (the run's default model) when absent:
# [embeddings]      vector embeddings for session search
#                   (absent = deterministic local hash embedder, no network)
# [search_synthesis]  search-result synthesis
# [vision]          vision-capable reads
# [scheduled]       scheduled-run turns
# [mcp_synthesis]   MCP result synthesis
[embeddings]
provider = "local"
model = "embedder"

[custom_providers.my-llm]    # optional: user endpoints (written by `pantheon model`)
base_url = "http://127.0.0.1:8015/v1"
api_mode = "openai"          # openai | anthropic
key_env = "PANTHEON_KEY_MY_LLM"  # env var NAME; value lives in <data-dir>/.env
[custom_providers.my-llm.models]  # operator-named models only (see below)
id = "my-model"              # model id as the endpoint spells it
# context_limit / max_output_tokens optional; omitted = unknown (conservative)

[agents.nyx]                 # optional: durable identity for one persistent agent
display_name = "Nyx"         # effective name; defaults to the table name
soul_file = "soul/nyx.md"    # persona file ref (repo-relative or absolute), optional
memory_namespace = "agent:nyx"  # defaults to agent:<table>; must not collide
policy = "coder"             # reader | coder | coder_memory; unknown = doctor error

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
host = "127.0.0.1"           # bind address: one of 127.0.0.1, 0.0.0.0, localhost, ::
```

## Custom provider models

`[custom_providers.<id>.models]` holds only models the operator named:
`pantheon model` records one when an id is typed by hand, and
`pantheon provider models <name>` fetches the live list on demand,
marking which recorded names the endpoint no longer offers. A migrated
or harvested list would be a cache wearing the authority of
configuration, so no model list is ever migrated. Unknown models resolve
to conservative catalog defaults (tools on, streaming on) rather than
failing.

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

## Judge model

`[judge]` selects an auxiliary model for the decision layer: route
selection, tool-gate risk scoring, and verification. It is a plain
provider/model pair — any catalog provider, any OpenAI-compatible base
URL, local or hosted. Nothing in the runtime is tied to a specific
decision model.

```toml
[judge]
provider = "openai"
model = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"   # optional; defaults to the provider key env
```

Resolution order (field-wise):

1. `PANTHEON_JUDGE_PROVIDER` / `PANTHEON_JUDGE_MODEL`
2. `config.toml [judge]`
3. Absent both = `auto`: the run's **default model** answers decisions.
   Auxiliary models default to `auto` — an absent section narrows cost
   and latency, it never switches a capability off.

Every aux slot (`judge`, `compression`, `title_gen`, `embeddings`,
`search_synthesis`, `vision`, `scheduled`, `mcp_synthesis`) takes the
same `PANTHEON_<SLOT>_PROVIDER` / `PANTHEON_<SLOT>_MODEL` overrides.

Rules:

- The judge model is an *auxiliary*: it never replaces the chat model.
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
Same shape and resolution rules as `[judge]`:

1. `PANTHEON_COMPRESSION_PROVIDER` / `PANTHEON_COMPRESSION_MODEL`
2. `config.toml [compression]`
3. Absent both = `auto`: the run's **default model** compresses. The
   deterministic trim (oldest tool rows re-compact, oldest exchanges drop)
   remains the correctness path regardless, so choosing a compressor is
   quality-of-context, never correctness.

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

## Session titles

`[title_gen]` names a conversation from its first user prompt: when a
session starts, the title auxiliary generates the one-line label shown in
`/runs`, `/history`, and the TUI conversation picker. Like every aux
default, an absent section does **not** switch titles off — aux models
default to `auto`, so an unconfigured host titles with the run's default
model. Rename any time with `/name <title>` (manual titles are `source:
manual` events and simply overwrite the generated one — last write wins).

```toml
[title_gen]
provider = "openai"          # any catalog provider or OpenAI-compatible URL
model = "gpt-4o-mini"        # a small, cheap model is plenty
api_key_env = "OPENAI_API_KEY"   # optional; defaults to the provider key env
```

Resolution order (field-wise, same shape as `[judge]`):

1. `PANTHEON_TITLEGEN_PROVIDER` / `PANTHEON_TITLEGEN_MODEL`
2. `config.toml [title_gen]`
3. Absent both = `auto`: the run's **default model** generates titles.

Behavior:

- **Fire-and-forget.** The title call runs on a worker thread *beside*
  the first turn, so it never delays the first token. The turn joins the
  worker before reporting done, so the title is durable even for one-shot
  `pantheon chat` invocations.
- **First prompt only.** A fresh conversation titles once; resumed and
  reopened runs never retitle.
- **Bounded output.** The reply is normalized to a single line, quotes and
  `Title:` labels stripped, hard-truncated at 60 characters.
- **Deterministic fallback.** On any failure (timeout, unreachable,
  empty reply) the host derives the title from the first prompt itself
  (first line, truncated), so history is never nameless. The run's
  answer is never affected: titles are cosmetic.
- **Last title wins.** The newest `SessionTitled` ledger event is the
  run's display title, so a future manual rename simply overwrites it.
- Keys follow the normal secret rules (`api_key_env` names an env var,
  seeded as `PANTHEON_TITLEGEN_API_KEY`); the value never lands in config,
  prompts, or the ledger. Bounded: 10-second timeout, 64-token completion.

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
the memory verbs read; setup keeps the two in sync. An http bridge takes
its endpoint from `PANTHEON_MEMORY_HTTP_URL` (key from
`PANTHEON_MEMORY_HTTP_KEY`) or per-backend `PANTHEON_MEMORY_<NAME>_URL` /
`PANTHEON_MEMORY_<NAME>_KEY` gallery variables (see docs/memory.md).

## Validation

`pantheon doctor` checks, in order:

1. config.toml parses (TOML error includes file, line, column)
2. required fields are non-empty
3. `api_key_env` (model and every configured aux section) is a plausible
   env var name AND is set in this shell
4. `[agents.*]` tables: slug names, known policies, no namespace clashes
5. memory backend is non-empty; stt/tts backends carry their required options
6. server.host is one of `127.0.0.1`, `0.0.0.0`, `localhost`, `::`
7. configured agent identities are listed by effective display name

Each failure reports `section`, `status: fail`, a human detail, and a fix
string. Doctor exits 1 when anything failed; warnings (like unknown
plugin hooks) do not affect the exit code.
