# security-guidance

Advisory security review of tool calls. It scans what the agent writes and
runs, then makes sure the model sees a short advisory when something looks
off — without ever blocking the tool call itself.

Adapted from
[anthropics/claude-plugins-official](https://github.com/anthropics/claude-plugins-official)
`plugins/security-guidance` (Apache-2.0; see NOTICE). The static
pattern rules are ported; the upstream async LLM diff-review half is
intentionally omitted (no supported path from a hook subprocess to a
model).

## How it works

Two hook points, both advisory:

- `pre_tool_call` (gate-class, observe-only): scans the arguments of
  `write_file` (full 25-rule vulnerability pattern set, gated by file
  extension) and `shell` (secret shapes), plus credential shapes in the
  arguments of any other tool. Findings are queued in a per-session
  relay; **the hook always returns "allow"**. This is deliberate:
  `pre_tool_call` fails *closed* on plugin error or timeout, so every
  code path is guarded to return silence instead of raising. A crash
  here would deny every tool call in the session.
- `transform_tool_result` (transform-class, fail-open): pops the queued
  findings for that tool call and scans the tool *result* for leaked
  credentials (AWS keys, private keys, tokens, passwords-in-URLs — a
  Pantheon addition; matched secret text is never echoed). On any
  finding it prepends a compact banner to the result the model sees;
  otherwise it returns no change.

Cross-fire state lives in small JSON files under the OS temp dir
(`pantheon-security-guidance/findings-<session>.json`), because the
host spawns a fresh Python process per hook fire. Files are
lock-protected, expire after 10 minutes, and are removed when empty.

Stdlib only. No network, no model calls, target <100ms per fire.

## Findings are advisory, not verdicts

Every banner is prefixed `[security-guidance]` and explicitly says the
finding is *not a block*. The agent is expected to review the note and
continue. Pattern rules produce false positives; treat them as nudges.

## Files

- `patterns.py` — 25 upstream vulnerability patterns (verbatim) +
  `scan_text` / `SECRET_PATTERNS` / `scan_secrets` (Pantheon additions).
- `_findings.py` — best-effort per-session relay between hook fires.
- `__init__.py` — `register(ctx)` wiring both hook points.
- `tests/test_security_guidance.py` — unit tests (stdlib `assert`s,
  runnable with plain `python3`).
