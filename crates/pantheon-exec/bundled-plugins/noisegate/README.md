# noisegate

Deterministic tool-output compaction for Pantheon. Fires on the
`transform_tool_result` hook: every tool result passes through this plugin
before the model sees it. Same input → same output, always — no model
calls, no network, no clock, no randomness. Any internal error returns the
input unchanged (fail-open; the host also fails open on transform hooks,
so a crash here can never break a turn).

This is **tool-result compaction**, not session compression: it shrinks one
tool's output in place. It never touches the transcript, context limits,
or the `OnCompaction` path.

## What it does

1. **Whole-payload JSON passes through untouched** — machine data must
   still parse.
2. **Whole-payload unified diffs pass through untouched** — in a diff,
   line counts and repeated context lines carry meaning.
3. **Carriage-return artifacts folded** — `a\rb` → `b` (progress
   overwrites).
4. **Progress spam collapsed** — a run of consecutive ticker lines
   (`[====> 45%]`, `45%`, `Downloading… 45%`, `45/100`) collapses to the
   single final-state line. Lines matching error patterns
   (`traceback|error|failed|…`) are never treated as progress.
5. **Consecutive identical lines** → first kept +
   `...(N identical lines)...`. Blank-line runs collapse silently to one.
6. **Repeated identical blocks (2–16 lines)** → first kept +
   `...(N-line block, M occurrences)...`. Consecutive repeats use the
   smallest repeating period (so a 5-line chunk ×20 reports as a 5-line
   block, not ragged 16-line fragments); scattered repeats are found with
   a 64-bit rolling hash.
7. **Single lines repeated 4+ times non-consecutively** → first kept +
   `...(repeated line, N occurrences total)...`.
8. **Backstop truncation** — anything still over 256 KiB is hard-truncated
   head+tail with a `[noisegate: truncated X→Y bytes]` marker. The marker
   reserves its own space, so the final payload is exactly 256 KiB and the
   numbers are honest.

Content is never rewritten: the first occurrence of anything collapsed is
kept verbatim, and every collapse carries its count. Fenced code blocks
(``` / ~~~) are verbatim — compaction applies only to the prose between
them. The trailing newline style (`\n` vs `\r\n`) is preserved.

## Ordering vs `compact_output` — read this before tuning either

There are two compaction layers and they run in a fixed order:

1. **`compact_output`** (`crates/pantheon-exec/src/lib.rs`,
   `CompactionPolicy`, default 200 lines / 64 KiB head+tail) runs **inside
   tool execution** — the supervisor, builtin tools, vault tools, and
   browser tools each cap their raw output before returning it.
2. **noisegate** (this plugin) fires **after**, in the session layer
   (`pantheon-runtime/src/session.rs` calls `fire_transform` on the
   `registry.execute()` result).

So this plugin always sees post-`compact_output` text. Consequences:

- The 256 KiB backstop here is deliberately **larger** than
  `compact_output`'s 64 KiB default. It is a backstop for tool paths that
  bypass `compact_output` (custom registries, bare `Session` use), not a
  second truncation of the same bytes.
- The markers are deliberately different —
  `[noisegate: truncated X→Y bytes]` vs compact_output's
  `[... compacted: dropped N lines, M bytes ...]` / `[... byte-cap ...]` —
  so you can always tell which layer acted.
- A 1 MB JSON payload that bypassed `compact_output` *will* be truncated
  by the backstop (invalid JSON, but at that size the payload is a hazard
  regardless of shape). Under the cap, JSON is never touched.
- If you change `compact_output`'s policy, this plugin needs no changes:
  it adapts to whatever text arrives.

## Performance

Fires on every tool result via subprocess spawn, so it stays lean: one
file, stdlib only (`re`, `json`), no per-fire setup beyond regex
compilation (module import). Measured on this machine (CPython 3.12):

- 100 KB mixed spam (repeats + progress + unique lines): **~30 ms median**
  (target <50 ms), min 27 ms; occasional VM-contention spikes to ~70 ms.
- 1 MB unique-lines adversarial input: ~350 ms (no hard target; the
  repeat-detection phases skip via the max-count guard when nothing
  repeats).

## Files

- `plugin.yaml` — manifest (`enabled: true`; Pantheon ships noisegate on
  by default per Umar's directive).
- `__init__.py` — the implementation; `register(ctx)` wires
  `transform_tool_result`, handler returns `{"replacement": str}` or `{}`
  for no change, matching the host SHIM contract in
  `crates/pantheon-extensions/src/python_runner.rs`.
- `test_noisegate.py` — 23 standalone tests, no third-party deps:
  `python3 test_noisegate.py`. Covers repeated-line spam, progress spam,
  CR artifacts, consecutive and scattered block repeats, 1 MB truncation,
  JSON/diff/table/fence passthrough, error preservation, empty and
  binary-ish input, determinism, the manifest, and the 100 KB timing
  benchmark.
