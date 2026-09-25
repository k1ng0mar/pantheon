# Memory

Five layers, a gated write path, provenance on everything, and
bidirectional markdown sync. This is the agent's long-term state.

## Layers

```
GLOBAL      -> cross-everything facts          (narrowest first at recall)
AGENT       -> per-agent-identity facts
PROJECT     -> per-project facts
TASK/SESSION-> per-task state
EPHEMERAL   -> this turn only, never stored
```

Recall queries all layers and returns hits narrowest-first, each with
provenance (where the record came from and when it was written).

## The write path

There is exactly one path for a write, and it is the same for the model,
the CLI, and imports:

```
propose -> policy check -> provenance attach -> validation -> store
```

- Policy: memory writes need the `MemoryWrite` capability (`coder_memory`
  policy, or an explicit grant).
- Provenance: source (who/what wrote it), imported_at. Recall returns it.
- Validation: size caps and shape checks.

No silent prompt-injection writes: anything the model wants remembered
goes through the same gate as anything you write by hand.

## Trust tiers

Every record carries a trust tier from where it came from:

```
system     harness-authored, authoritative
user       you wrote it (CLI put, /remember, hand-edited MEMORY.md)
memory     user-confirmed records (memory_confirm promotion)
untrusted  model-proposed, tool- or web-derived material
```

The invariant: content never gains trust by being copied. A model
proposal lands untrusted no matter what it claims; only an explicit
user action promotes it (`pantheon memory confirm KEY`, or a human
edit in MEMORY.md). Recalled records show their tier inline, and
untrusted records are flagged `[untrusted: source]` in the context the
model sees.

## CLI

```sh
pantheon memory put KEY VALUE          # store (user trust, goes through the gate)
pantheon memory recall QUERY           # FTS search, provenance included
pantheon memory confirm KEY            # promote a record to memory tier
pantheon memory export [PATH]          # store -> markdown (default MEMORY.md)
pantheon memory import [PATH]          # markdown -> store, each section a proposal
pantheon memory sync [PATH]            # bidirectional, conflict-detecting
pantheon memory backend list|select    # switch native/http
```

## MEMORY.md format

The Agent layer syncs to markdown so a human can read and edit it:

```markdown
<!-- pantheon:agent-memory v1 -->

# Agent memory

# city

Kano

# notes

line one
\# escaped heading-looking line inside a value
```

- `# <key>` headings delimit records; the body until the next heading is
  the value.
- Value lines that start with `# ` (or are exactly `#`) are escaped with
  a leading backslash on export and unescaped on import, so a value can
  contain heading-looking text without corrupting the file.
- The sentinel comment marks v1. In v1 files a record literally keyed
  `Agent memory` round-trips; legacy files (no sentinel) treat the first
  `# Agent memory` as the document header.

## Sync semantics

`memory sync` compares three hashes: the file's current hash, the store's
current hash, and the last-synced hash (stored in `<path>.sync-hash`,
e.g. `MEMORY.md.sync-hash`).

| File changed | Store changed | Result |
|---|---|---|
| no | no | nothing to do |
| yes | no | file -> store (import) |
| no | yes | store -> file (export) |
| yes | yes | **conflict**: sync refuses, prints both sides |

Both-sides-changed requires a human decision; sync never picks a winner.

## Backends

`native` is the bundled SQLite store. External services are reached
through the same `MemoryBackend` seam, selected by name:

```sh
pantheon memory backend list                       # native, http, and plugins
# url is the service root (paths add /v1/memory/...):
pantheon memory backend select honcho url=http://127.0.0.1:8000 key=...
```

| Backend | Kind | Notes |
|---|---|---|
| `native` | SQLite + FTS5 | default; persistent `memory.db`; trust tiers, MEMORY.md sync |
| `http` | HTTP bridge | generic adapter to Pantheon's small `/v1/memory` JSON API |
| `galaxymem` | HTTP bridge | same protocol; `url=`/`key=` options or `PANTHEON_MEMORY_GALAXYMEM_URL`/`_KEY` |
| `mnemosyne` | HTTP bridge | same, `PANTHEON_MEMORY_MNEMOSYNE_URL`/`_KEY` |
| `honcho` | HTTP bridge | same, `PANTHEON_MEMORY_HONCHO_URL`/`_KEY` |
| `hindsight` | HTTP bridge | same, `PANTHEON_MEMORY_HINDSIGHT_URL`/`_KEY` |
| `openviking` | HTTP bridge | same, `PANTHEON_MEMORY_OPENVIKING_URL`/`_KEY` |

Selection is persisted at `<data_dir>/memory-backend.toml` and is honored
by the runtime session (recall before each turn, `memory_*` tools) and by
`pantheon memory recall|put|confirm`. `import`/`export`/`sync` remain
native-only because MEMORY.md is the native store's file format.

Honesty note: the plugin entries speak the Pantheon `/v1/memory` protocol.
Point them at a service that exposes it (or a thin bridge in front of
GalaxyMem/Mnemosyne/Honcho/Hindsight/OpenViking). Construction is offline
— a wrong URL fails at first call with a structured error
(`MEM_HTTP_CONN`), a missing URL fails at selection time
(`MEM_BACKEND_CONFIG`).

Guarantees that hold for every backend, including plugins:

- Policy and validation run **at the boundary** (`recall_via` /
  `write_via` / `confirm_via`) before the backend sees anything. A
  backend is never the party that decides to ignore policy.
- Model-authored writes reach plugins already clamped to `untrusted`.
- Operations a backend cannot represent (`forget`, `confirm` on
  non-native) return structured `MEM_BACKEND_UNSUPPORTED` instead of
  silently pretending.
- A misconfigured selection degrades to native with a warning line; a
  session never runs memory-less because of a typo in TOML.

Protocol (what a bridge must implement):

```
GET  /v1/memory/recall?query=...&limit=N&layers=...
   -> [{"key":..,"value":..,"rank":..,"provenance":{...}}]
POST /v1/memory/write        {"layer","namespace","key","value","provenance","max_bytes"}
   -> {"key":..,"value":..,"provenance":{...}}
GET  /v1/memory/list_agent?namespace=...
   -> [["key","value"], ...]
```

Errors: 4xx/5xx with `{"error":"CODE","cause":"..."}` map back onto
structured Pantheon errors.

Plugin backends can also be registered from Rust (a plugin crate calls
`BackendRegistry::register_with`, factory receives the persisted
selection), so a backend can hold its own config beyond `url`/`key`
without Pantheon owning that config.

## Custom memory plugins

Any backend, not just the pre-named ones, is installable by dropping a
manifest file — no Rust changes:

```sh
pantheon memory backend scaffold mybrain http    # writes the template
$EDITOR ~/.pantheon/memory-plugins/mybrain.toml
pantheon memory backend select mybrain           # now the active backend
```

Manifests live at `<data_dir>/memory-plugins/<name>.toml`; the **file
name is the backend name**, and selection options override the manifest
(`select mybrain url=... key=...`), so one manifest can serve several
deployments.

### `kind = "http"`

```toml
name = "mybrain"
label = "MyBrain memory service"
kind = "http"
url = "http://127.0.0.1:9000"   # service root
key = "token"                   # optional bearer token
prefix = "/v1/memory"           # protocol mount point (default)
```

Use this for any service that exposes (or is bridged to) the
`/v1/memory` protocol above. `prefix` lets a bridge mount the protocol
anywhere, e.g. `/api/memory`.

### `kind = "stdio"`

```toml
name = "mybrain"
kind = "stdio"
command = "python3"
args = ["/opt/mybrain/bridge.py"]
timeout_ms = 5000
```

A subprocess bridge: the runtime spawns the command per call, writes one
JSON request line to stdin, reads one JSON response line from stdout
(stderr is ignored). No long-lived process to supervise, and a hung
plugin is **killed on timeout** (`MEM_PLUGIN_TIMEOUT`) instead of
wedging the turn.

Requests the bridge must handle:

```json
{"op":"recall","query":"...","limit":8,"layers":["Agent","Project","Global"]}
{"op":"write","layer":"Agent","namespace":"nyx","key":"k","value":"v","provenance":{...},"max_bytes":4096}
{"op":"list_agent","namespace":"nyx"}
{"op":"get","namespace":"nyx","key":"k"}
{"op":"forget","layer":"Agent","namespace":"nyx","key":"k"}
{"op":"confirm","namespace":"nyx","key":"k"}
```

Responses are `{"ok":true, ...}` with `hits` / `record` / `rows` /
`found` / `removed` as appropriate, or
`{"ok":false,"code":"MYBRAIN_DOWN","cause":"..."}` — the code surfaces
to the caller unchanged, so plugin failures stay debuggable.

Broken manifests are skipped with a loud line at load time (one bad file
never bricks the registry); `pantheon memory backend list` shows what
loaded, and the CLI validates names on `select`. Because user manifests
are plain files under the data dir, they are versionable, shareable, and
removable without touching the runtime.

## Vault archive (Obsidian library)

Long material that should not live in the working memory store — design
docs, research reports, course notes, project write-ups — gets archived
into the Obsidian vault instead: a human-readable, linked markdown
library that survives outside the agent's SQLite state.

Vault root resolution: `PANTHEON_VAULT_DIR`, else `$HOME/vault`.

The agent gets four tools (registered in every session):

| Tool | Capability | What it does |
|---|---|---|
| `vault_archive` | FilesystemWrite | Write a markdown doc under a vault category (`notes/`, `projects/`, `reference/`, `people/`) with YAML frontmatter (title, date, tags) |
| `vault_read` | FilesystemRead | Read one note; output goes through deterministic compaction like any other tool |
| `vault_search` | FilesystemRead | Keyword search across vault `.md` files; returns `[[wikilink]]` hits with a snippet |
| `vault_list` | FilesystemRead | List notes in a category or across the vault |

The same surface is available from the CLI:

```sh
pantheon memory vault search continuity [category]
pantheon memory vault read notes/ideation/2026-09-15-webhookwatch.md
pantheon memory vault list projects
```

Guarantees and limits:

- Path traversal is rejected (`..` in any argument -> `VAULT_PATH_TRAVERSAL`).
- Traversal runs under a wall-clock budget (2.5s) because the vault may
  sit on a FUSE network mount; partial results are labeled instead of
  stalling the turn. Narrow with `category` when you see the note.
- Markdown files over 512 KiB are skipped during search (read them by
  path with `vault_read`, which compacts).
- Vault writes are plain `FilesystemWrite` — no memory trust tiers. The
  vault is user-owned space; anything the model archives there is just a
  file you can delete or edit in Obsidian.

Division of labor: the native store holds small keyed facts with trust
tiers and provenance; the vault holds big documents. When a record grows
past what belongs in `MEMORY.md`, archive it and keep only a pointer.
