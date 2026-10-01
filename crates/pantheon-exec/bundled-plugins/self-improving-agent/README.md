# self-improving-agent

A disciplined learning loop for the agent, as a Pantheon hook plugin.
Adapted from pskoett's `self-improving-agent` skill (MIT-0) for Pantheon's
Python hook protocol: OpenClaw install paths, the OpenClaw CLI, and
OpenClaw-only session tools were stripped, and paths were remapped to
Pantheon (the workspace root is the home directory, where `MEMORY.md`,
`AGENTS.md`, `SOUL.md`, and `TOOLS.md` live).

Opt-in: disabled by default.

## What it does

- `on_session_start` (Context hook): ensures `.learnings/` exists, then
  injects the logging protocol (entry formats, pattern-key dedup rules,
  promotion rules) plus a pending-triage note — counts of untriaged
  learnings/errors/feature requests and the high-priority ones.
- `on_session_end` (Observer hook): ensures `.learnings/` exists and stamps
  a session-end marker so the next session's triage note can reference it.

Log files live in `<learnings_dir>/.learnings/`:

- `LEARNINGS.md` — corrections, insights, knowledge gaps, best practices
- `ERRORS.md` — command failures and integration errors
- `FEATURE_REQUESTS.md` — user-requested capabilities

Entries carry `Pattern-Key: area.symptom` — the stable dedup key. Before
logging, grep by pattern-key; on a hit, bump `Recurrence-Count` and
`Last-Seen` instead of duplicating. Broadly-applicable learnings promote
to the workspace files: behavioral patterns → `SOUL.md`, tool gotchas →
`TOOLS.md`, workflows → `AGENTS.md` (promotion rule: recurrence ≥ 3,
seen in 2+ distinct tasks, within 30 days).

## Configuration

`config.json` in the plugin dir:

```json
{"learnings_dir": ""}
```

Empty means auto-resolve: `$PANTHEON_DATA_DIR/.learnings` when that env
var is set, otherwise `~/.learnings`. Set an explicit absolute path to
keep learnings with a project instead.

## Security

Local files only — no network, no exfiltration. The injected protocol
carries the upstream warning verbatim in spirit: **never log secrets,
tokens, private keys, environment variables, or full source/config files**
unless the user explicitly asks for that level of detail; prefer short
summaries or redacted excerpts.

One honest deviation from upstream: the OpenClaw version swept the ended
session's transcript for error patterns at session end. Pantheon hook
children receive no transcript (only hook/session/run ids) and run with
a scrubbed environment, so automatic error extraction is not possible
here. Learning *content* comes from the agent following the injected
protocol; the hooks supply the reminder and the bookkeeping.
