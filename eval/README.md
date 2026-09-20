# Pantheon eval harness

Regression suite derived from real Pantheon wave-1 features and
Hermes-history behaviors (WAVE2/TEAMS item: "eval/ — 20-30 Hermes-history
tasks as regression suite" — this is the seed, wave-3 lane: freebuff).

Stdlib-only (`python3 eval/run.py`); drives the real `pantheon` CLI built
from this workspace. Every case runs in a fresh sandbox
(`PANTHEON_DATA_DIR`/`PANTHEON_EXT_DIR` under a temp dir), so re-running
never inherits state from a previous run.

## Usage

```bash
cargo build -p pantheon-cli    # or point PANTHEON_BIN at an existing binary
python3 eval/run.py                # run all active cases
python3 eval/run.py --list         # show cases + why they would skip
python3 eval/run.py --only <id>    # run a single case
python3 eval/run.py --cargo-tests  # gate on `cargo test --workspace` first
```

Exit code is 0 only when every non-skipped case passes — wire it into CI
or a pre-merge hook as the feature gate.

## Current cases (10)

| Case | Origin |
|---|---|
| run-completes-and-logs | wave1 supervisor lifecycle; ledger persistence |
| explain-unknown-run-is-empty | wave1 `/explain` offline path |
| run-failure-records-code | wave1 structured errors (§20) |
| rerun-of-completed-run-keeps-history | wave1 append-only ledger |
| hook-fires-and-dedups-per-session | Hermes: once-per-session plugins |
| timegap-ages-and-injects | Hermes time-gap port |
| doctor-rejects-missing-manifest | Hermes/OpenClaw preflight complaints |
| doctor-flags-ts-entry-with-warning | doctor TS_ENTRY (OpenClaw-compat pending) |
| doctor-accepts-valid-plugin | vendor/time-gap-pantheon as fixture |
| compaction-keeps-head-and-tail | wave1 noisegate-style pantheon-exec compaction |

The compaction case mirrors `pantheon-exec::compact_output` (default
policy: head 60 / max 200 lines / 64 KiB, FNV-1a hash of the dropped
middle) in the runner, so a change to the Rust compaction behavior that
breaks the documented contract fails the eval even though no CLI command
exposes compaction yet.

## Adding a case

Append to `eval/cases.json`:

```json
{
  "id": "unique-id",
  "title": "what behavior is pinned",
  "origin": "where the behavior comes from",
  "commands": [["run", "--id", "demo", "--say", "hi"]],
  "expect": { "unique-id": ["demo"] },
  "post": [["status", "demo", "equals:completed"]]
}
```

- `commands`: argv lists; first command's stdout is checked against
  `expect["<id>"]`, command *n* against `expect["<id>__<n>"]`. The
  special probe `_compact <file>` runs the compaction mirror.
- `expect` assertions: `contains:X` (or bare string), `equals:X`,
  `ANY`, and `not:` prefix to invert.
- `post`: extra CLI checks after the main commands; last element is an
  assertion, or `exists:<path>` to check the sandbox filesystem.
- `expect_fail`: command indices expected to exit non-zero.
- `setup`: `dirs` + `files` materialized in the case sandbox;
  `#GENERATE_LINES:N` content expands to `line 0..N-1`.
- Placeholders: `<TMP>` = case sandbox dir, `<VENDOR>` = `vendor/`.
- `skip`: reason string (`needs binary`, `python3`) to skip when the
  prerequisite is missing — deliberate, so the harness degrades cleanly
  on a machine without a built binary.

Target: grow this toward the 20-30 Hermes-history tasks called for in
wave 3, one case per regression that ever bit a real session.
