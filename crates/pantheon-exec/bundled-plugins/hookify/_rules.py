"""Rule loading and evaluation for the hookify Pantheon plugin.

Adapted from anthropics/claude-plugins-official ``plugins/hookify``
(files ``core/config_loader.py`` and ``core/rule_engine.py``,
Apache-2.0). The upstream pair ran on Claude Code hook JSON
(``hook_event_name``/``tool_name``/``tool_input`` with matcher
libraries). This port evaluates against Pantheon ``pre_tool_call``
input (``tool`` + ``args`` JSON string).

Pantheon differences vs upstream:

- Event mapping: upstream events ``bash``/``file``/``all`` map to the
  Pantheon tools ``shell``/``write_file``/any. A rule may also name a
  Pantheon tool directly (``event: shell``) or override with
  ``tool_matcher: <tool name>``.
- Field extraction: ``command`` (shell), ``path``/``content``
  (write_file), ``tool``, and the ``args_json`` pseudo-field holding
  the raw argument JSON.
- Operators kept from upstream: ``regex_match``, ``contains``,
  ``not_contains``, ``equals``, ``starts_with``, ``ends_with``.
- The frontmatter parser is stdlib-only here: upstream needed full
  YAML; we support the flat subset used by real hookify rules
  (scalars + ``conditions:`` list of ``field``/``operator``/``pattern``
  maps). Quoting rules: no nested maps, no anchors.
- Rules live in ``rules/*.md`` inside the plugin directory (cwd for the
  hook subprocess). Suggested rules from the suggester land in
  ``rules/suggested/*.md`` with ``enabled: false`` and are NOT loaded
  until enabled.

Never raises on rule errors: a bad rule file is skipped and the
plugin as a whole never raises (a crash in ``pre_tool_call`` would
fail CLOSED and deny every tool call).
"""

import os
import re

_HERE = os.path.dirname(os.path.abspath(__file__))
_RULES_DIR = os.path.join(_HERE, "rules")

# Upstream event names -> Pantheon tool names.
_EVENT_TOOL = {
    "bash": "shell",
    "file": "write_file",
    "all": None,  # matches every tool
}

_KNOWN_TOOLS = {"shell", "write_file", "read_file", "list_dir"}


class Rule:
    def __init__(self, name, enabled, event, action, pattern, message,
                 conditions, tool_matcher, source):
        self.name = name
        self.enabled = enabled
        self.event = event
        self.action = action            # "block" or "warn"
        self.pattern = pattern          # legacy single-pattern shorthand
        self.message = message
        self.conditions = conditions    # list of (field, operator, pattern)
        self.tool_matcher = tool_matcher
        self.source = source

    def __repr__(self):
        return "Rule(%r, %s)" % (self.name, self.action)


# ---------------------------------------------------------------------------
# Frontmatter parser (stdlib; flat subset of YAML used by hookify rules)
# ---------------------------------------------------------------------------

def _parse_frontmatter(text):
    """Return (dict, body). Flat scalars + conditions list of maps."""
    lines = text.splitlines()
    if not lines or lines[0].strip() != "---":
        return {}, text
    end = None
    for i, ln in enumerate(lines[1:], 1):
        if ln.strip() == "---":
            end = i
            break
    if end is None:
        return {}, text
    fm_lines, body = lines[1:end], "\n".join(lines[end + 1:])
    data = {}
    cur_key = None
    cur_item = None
    for ln in fm_lines:
        if not ln.strip() or ln.lstrip().startswith("#"):
            continue
        # "- key: value" or "- value" list items.
        m = re.match(r"^\s+-\s+(.*)$", ln)
        if m:
            rest = m.group(1).strip()
            if cur_key == "conditions":
                kv = re.match(r"^([A-Za-z_][\w-]*)\s*:\s*(.*)$", rest)
                if kv:
                    cur_item = {kv.group(1): _parse_scalar(kv.group(2))}
                    data.setdefault(cur_key, []).append(cur_item)
                else:
                    cur_item = None
            continue
        m = re.match(r"^(\s*)([A-Za-z_][\w-]*)\s*:\s*(.*)$", ln)
        if not m:
            continue
        indent, key, val = m.group(1), m.group(2), m.group(3)
        if indent and cur_item is not None:
            # continuation line of the current condition map
            cur_item[key] = _parse_scalar(val)
        elif not indent:
            cur_key = key
            cur_item = None
            if val == "":
                data[key] = [] if key == "conditions" else ""
            else:
                data[key] = _parse_scalar(val)
    return data, body


def _parse_scalar(val):
    val = val.strip()
    if len(val) >= 2 and val[0] == val[-1] and val[0] in "\"'":
        return val[1:-1]
    low = val.lower()
    if low in ("true", "yes"):
        return True
    if low in ("false", "no"):
        return False
    try:
        return int(val)
    except ValueError:
        pass
    return val


def load_rule_file(path):
    """Parse one ``*.md`` rule file; return a Rule or None on any error."""
    try:
        with open(path, encoding="utf-8") as fh:
            text = fh.read()
    except OSError:
        return None
    try:
        fm, body = _parse_frontmatter(text)
        name = str(fm.get("name", os.path.basename(path)))
        enabled = fm.get("enabled", True)
        if isinstance(enabled, str):
            enabled = enabled.lower() in ("true", "yes", "1")
        event = str(fm.get("event", "all")).strip()
        action = str(fm.get("action", "warn")).strip().lower()
        if action not in ("block", "warn"):
            action = "warn"
        pattern = fm.get("pattern")
        conditions = []
        raw = fm.get("conditions") or []
        for c in raw:
            if isinstance(c, dict) and c.get("field"):
                conditions.append((
                    str(c["field"]),
                    str(c.get("operator", "regex_match")),
                    str(c.get("pattern", "")),
                ))
        tool_matcher = fm.get("tool_matcher")
        if tool_matcher is not None:
            tool_matcher = str(tool_matcher).strip()
        return Rule(name=name, enabled=bool(enabled), event=event,
                    action=action,
                    pattern=str(pattern) if pattern is not None else None,
                    message=body.strip(), conditions=conditions,
                    tool_matcher=tool_matcher, source=path)
    except Exception:
        return None


def load_rules(rules_dir=None):
    """Load all enabled ``*.md`` rules from ``rules_dir`` (not suggested/)."""
    d = rules_dir or _RULES_DIR
    rules = []
    try:
        names = sorted(os.listdir(d))
    except OSError:
        return []
    for name in names:
        if not name.endswith(".md") or name.startswith("."):
            continue
        rule = load_rule_file(os.path.join(d, name))
        if rule is not None and rule.enabled:
            rules.append(rule)
    return rules


# ---------------------------------------------------------------------------
# Rule engine
# ---------------------------------------------------------------------------

class _RegexCache:
    def __init__(self, limit=128):
        self._cache = {}
        self._limit = limit

    def compile(self, pattern):
        rx = self._cache.get(pattern)
        if rx is None:
            rx = re.compile(pattern, re.IGNORECASE)
            if len(self._cache) >= self._limit:
                self._cache.pop(next(iter(self._cache)))
            self._cache[pattern] = rx
        return rx


class RuleEngine:
    """Evaluate rules against a Pantheon pre_tool_call input dict."""

    def __init__(self):
        self._rx = _RegexCache()

    # -- matching ------------------------------------------------------

    def _event_matches(self, rule, tool):
        wanted = (rule.tool_matcher or "").strip()
        if wanted:
            return wanted == tool
        ev = (rule.event or "all").strip()
        if ev == "all":
            return True
        if ev in _EVENT_TOOL:
            mapped = _EVENT_TOOL[ev]
            return mapped is None or mapped == tool
        # Direct Pantheon tool name, or a regex over the tool name.
        if ev in _KNOWN_TOOLS:
            return ev == tool
        try:
            return self._rx.compile(ev).search(tool) is not None
        except re.error:
            return False

    def _field_value(self, field, tool, args, args_json):
        if field == "tool":
            return tool
        if field == "args_json":
            return args_json
        if field == "command" and tool == "shell":
            return args.get("command", "")
        if tool == "write_file":
            if field in ("path", "file_path"):
                return args.get("path", "")
            if field == "content":
                return args.get("content", "")
        v = args.get(field, "")
        return v if isinstance(v, str) else ""

    def _condition_matches(self, cond, tool, args, args_json):
        field, operator, pattern = cond
        value = self._field_value(field, tool, args, args_json)
        if not isinstance(value, str):
            return False
        try:
            if operator == "regex_match":
                return self._rx.compile(pattern).search(value) is not None
            if operator == "contains":
                return pattern.lower() in value.lower()
            if operator == "not_contains":
                return pattern.lower() not in value.lower()
            if operator == "equals":
                return value.lower() == pattern.lower()
            if operator == "starts_with":
                return value.lower().startswith(pattern.lower())
            if operator == "ends_with":
                return value.lower().endswith(pattern.lower())
        except (re.error, TypeError):
            return False
        return False  # unknown operator never matches

    def _rule_matches(self, rule, tool, args, args_json):
        if not self._event_matches(rule, tool):
            return False
        # Legacy shorthand: pattern applies to the natural field
        # (command for shell, content for write_file, args_json else).
        if rule.pattern:
            if tool == "shell":
                field = "command"
            elif tool == "write_file":
                field = "content"
            else:
                field = "args_json"
            if not self._condition_matches(
                    (field, "regex_match", rule.pattern),
                    tool, args, args_json):
                return False
        for cond in rule.conditions:
            if not self._condition_matches(cond, tool, args, args_json):
                return False
        return True

    # -- evaluation -----------------------------------------------------

    def evaluate(self, rules, tool, args, args_json):
        """Return (blocking, warnings): lists of matched Rules."""
        blocking, warnings = [], []
        for rule in rules:
            try:
                matched = self._rule_matches(rule, tool, args, args_json)
            except Exception:
                continue
            if matched:
                (blocking if rule.action == "block" else warnings).append(rule)
        return blocking, warnings
