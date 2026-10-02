"""hookify: user-defined rules for tool calls, ported to Pantheon.

Adapted from anthropics/claude-plugins-official ``plugins/hookify``
(Apache-2.0). Upstream let users write natural-language-ish rules in
markdown files and evaluated them in Claude Code's PreToolUse hook,
with a stop-hook example and an LLM "suggester" agent.

Pantheon port notes (read before changing the hook wiring):

- Upstream events are Claude Code hook events; here ``event: bash``
  maps to the Pantheon ``shell`` tool, ``event: file`` to
  ``write_file``, ``event: all`` to any tool. Rules may also name a
  Pantheon tool directly (``event: shell``) or override with
  ``tool_matcher: <tool>``. The stop-hook example is dropped: Pantheon
  has no blocking hook at session end.
- Rules live in ``rules/*.md`` inside the plugin directory (the hook
  subprocess's cwd). Suggested rules land in ``rules/suggested/``.
- ``action: block`` returns ``{"deny": true, "reason": ...}`` to
  ``pre_tool_call``. ``action: warn`` stashes the message and
  ``transform_tool_result`` prepends it to that tool's result as an
  advisory.
- Every code path here is guarded: ``pre_tool_call`` is Gate-class and
  fails CLOSED on plugin error/timeout, so a crash in this plugin
  would deny every tool call. Never raise.
- The upstream LLM suggester is replaced by deterministic heuristics
  (``_suggest.py``): the plugin cannot reach a model from a hook
  subprocess.

Env is scrubbed for hook subprocesses (PATH only), so rules are
bundled/edited in the plugin dir, not looked up from $HOME.
"""

import json
import os
import sys
import tempfile
import fcntl
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import _rules  # noqa: E402
import _suggest  # noqa: E402

#: Pantheon spawns a fresh process per hook fire, so the warn stash must
#: survive across processes: small JSON files in the temp dir, TTL'd.
_WARN_TTL = 600


def _stash_path(session_id):
    safe = "".join(c if (c.isalnum() or c in "_-.") else "_"
                   for c in (session_id or "unknown"))[:128] or "unknown"
    return os.path.join(tempfile.gettempdir(), "pantheon-hookify",
                        "warn-%s.json" % safe)


def _stash_warn(session_id, message):
    if not session_id or not message:
        return
    path = _stash_path(session_id)
    try:
        os.makedirs(os.path.dirname(path), exist_ok=True)
        try:
            fh = open(path, "r+", encoding="utf-8")
        except OSError:
            fh = open(path, "w", encoding="utf-8")
        fcntl.flock(fh, fcntl.LOCK_EX)
        try:
            try:
                entries = json.load(fh)
            except (ValueError, OSError):
                entries = []
            now = time.time()
            entries = [e for e in entries if isinstance(e, dict)
                       and now - e.get("ts", 0) <= _WARN_TTL]
            entries.append({"ts": now, "message": message[:2000]})
            entries = entries[-20:]
            fh.seek(0)
            fh.truncate()
            json.dump(entries, fh)
        finally:
            fcntl.flock(fh, fcntl.LOCK_UN)
            fh.close()
    except Exception:
        pass


def _pop_warns(session_id):
    path = _stash_path(session_id)
    try:
        with open(path, encoding="utf-8") as fh:
            entries = json.load(fh)
    except (OSError, ValueError):
        return []
    try:
        os.unlink(path)
    except OSError:
        pass
    now = time.time()
    return [e["message"] for e in entries if isinstance(e, dict)
            and now - e.get("ts", 0) <= _WARN_TTL and e.get("message")]


def register(ctx):
    ctx.register_hook("pre_tool_call", _pre_tool_call)
    ctx.register_hook("transform_tool_result", _transform_tool_result)
    ctx.register_hook("on_session_end", _on_session_end)


def _load_args(args):
    try:
        data = json.loads(args) if isinstance(args, str) else {}
    except (ValueError, TypeError):
        data = {}
    return data if isinstance(data, dict) else {}


def _pre_tool_call(**kwargs):
    """Evaluate rules. Block -> deny; warn -> stash; else allow."""
    try:
        tool = kwargs.get("tool", "") or ""
        args_json = kwargs.get("args", "") or ""
        session_id = kwargs.get("session_id", "") or ""
        args = _load_args(args_json)
        # Log for the session-end suggester (best-effort).
        _suggest.log_call(session_id, tool, args_json)
        rules = _rules.load_rules()
        engine = _rules.RuleEngine()
        blocking, warnings = engine.evaluate(rules, tool, args, args_json)
        if blocking:
            rule = blocking[0]
            reason = "[hookify rule '%s'] %s" % (
                rule.name, rule.message or "blocked by policy")
            return {"deny": True, "reason": reason[:2000]}
        for rule in warnings:
            msg = "[hookify rule '%s'] %s" % (
                rule.name, rule.message or "warning")
            _stash_warn(session_id, msg)
    except Exception:
        pass
    return {}


def _transform_tool_result(**kwargs):
    """Prepend stashed warn messages as an advisory."""
    try:
        session_id = kwargs.get("session_id", "") or ""
        result = kwargs.get("result", "") or ""
        warns = _pop_warns(session_id)
        if not warns:
            return {}
        banner = ("[hookify] Policy warnings (advisory - the tool already "
                  "ran):\n" + "\n".join(warns))
        if len(banner) > 4000:
            banner = banner[:4000] + "\n(...truncated...)"
        return {"replacement": banner + "\n\n" + result}
    except Exception:
        return {}


def _on_session_end(**kwargs):
    """Run the deterministic suggester; drafts land disabled."""
    try:
        session_id = kwargs.get("session_id", "") or ""
        rules = _rules.load_rules()
        _suggest.suggest(session_id, rules)
    except Exception:
        pass
    return {}
