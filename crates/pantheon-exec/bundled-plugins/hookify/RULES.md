# hookify rule format

Rules are markdown files in `rules/` inside the plugin directory. YAML
frontmatter carries the matching logic; the body is the message shown
when the rule fires.

```markdown
---
name: warn-sensitive-files
enabled: true
event: write_file          # shell | write_file | all | <tool name> | regex
tool_matcher: write_file   # optional override, takes precedence over event
pattern: \.env(\.|$)       # optional shorthand regex over the natural field
action: warn               # block | warn
conditions:                # optional, all must match
- field: path            # command | path | content | tool | args_json
    operator: regex_match  # regex_match | contains | not_contains |
                           # equals | starts_with | ends_with
    pattern: \.pem$
---

Your message to the agent (shown on block or prepended as an advisory).
```

Adapted from
[anthropics/claude-plugins-official](https://github.com/anthropics/claude-plugins-official)
`plugins/hookify` (Apache-2.0). Upstream events were Claude Code hook
events: `bash` maps here to `shell`, `file` to `write_file`, `all` to
every tool. You can also name a Pantheon tool directly (`event:
shell`) or give a regex matched against the tool name.

## Semantics

- `action: block` denies the tool call at `pre_tool_call` with
  `[hookify rule '<name>'] <message>` as the reason. The block is a
  hard denial, not a suggestion.
- `action: warn` lets the tool run and prepends the message to that
  tool's result as an advisory via `transform_tool_result`.
- `enabled: false` skips the rule.
- Files in `rules/suggested/` are drafts written by the session-end
  suggester; they are never loaded. Review a draft, then move it into
  `rules/` (or enable it in place by copying it - the loader reads
  `rules/*.md` only, one level, no recursion).
- Malformed rule files are skipped silently. A failing rule engine can
  never deny tool calls: evaluation errors are swallowed per rule.
