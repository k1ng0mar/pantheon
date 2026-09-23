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

## CLI

```sh
pantheon memory put KEY VALUE          # store (goes through the gate)
pantheon memory recall QUERY           # FTS search, provenance included
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

`native` is the bundled SQLite store. The `http` backend points at a
remote memory service via `[memory.options] url = "..."`; requests are
percent-encoded, bearer-authenticated, and map errors to structured
codes. `pantheon memory backend list` shows what is available; selection
is validated (unknown backends and unknown options are rejected, not
silently ignored).
