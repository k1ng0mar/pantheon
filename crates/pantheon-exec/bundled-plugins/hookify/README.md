# hookify

User-defined rules for tool calls: block dangerous actions, warn on
sensitive ones. Rules are plain markdown files - no code changes needed
to add a policy.

Adapted from
[anthropics/claude-plugins-official](https://github.com/anthropics/claude-plugins-official)
`plugins/hookify` (Apache-2.0; see NOTICE). The rule format and the
evaluation engine are ported; the upstream stop-hook example is dropped
(no blocking hook exists at session end in Pantheon) and the upstream
LLM suggester agent is replaced by deterministic heuristics.

## How it works

- `pre_tool_call` evaluates every enabled rule in `rules/*.md` against
  the tool name and arguments. `action: block` returns a denial with
  `[hookify rule '<name>']` as the reason; `action: warn` stashes the
  message. Evaluation is total: malformed rules are skipped and per-rule
  errors are swallowed, because `pre_tool_call` fails *closed* on plugin
  error - a crash here would deny every tool call in the session.
- `transform_tool_result` prepends stashed warn messages to that tool's
  result as an advisory (the tool already ran; the warning is a nudge).
- `on_session_end` runs the suggester: if the session repeatedly hit a
  dangerous shell pattern or wrote sensitive paths with no covering
  rule, a draft rule is written to `rules/suggested/*.md` with
  `enabled: false`. Drafts never take effect until you review and move
  them into `rules/`.

Upstream event names map to Pantheon tools: `bash` → `shell`, `file` →
`write_file`, `all` → every tool. You can also name a Pantheon tool
directly or override with `tool_matcher`. See `RULES.md` for the full
format.

Stdlib only. No network, no model calls. Rules live in the plugin
directory (the hook subprocess's cwd) because hook environments are
scrubbed - there is no $HOME to resolve a data dir from.

## Files

- `_rules.py` - frontmatter parser, Rule, and the evaluation engine.
- `_suggest.py` - session call log + deterministic draft-rule
  suggester.
- `__init__.py` - `register(ctx)` wiring the three hook points.
- `rules/` - three example rules (dangerous rm block, sensitive-file
  warn, curl-pipe-shell warn).
- `RULES.md` - rule authoring reference.
- `tests/test_hookify.py` - unit tests (stdlib, plain `python3`).
