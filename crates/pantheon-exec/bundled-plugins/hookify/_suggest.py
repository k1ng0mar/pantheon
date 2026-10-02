"""Session-end rule suggester for the hookify Pantheon plugin.

Upstream hookify's suggester is an LLM agent that reads the session
transcript and proposes new YAML rules. A Pantheon hook subprocess
cannot reach a model, so this port uses deterministic heuristics
instead: it looks at the session's recorded tool calls and drafts
rules for patterns the user repeats, *unless* a rule already covers
them.

Data source: the same relay approach as security-guidance - a
per-session JSON log of tool calls written at ``pre_tool_call`` time
and consumed here at ``on_session_end``. The ``__init__`` module owns
the logging; this module only drafts and writes.

Draft rules are written to ``rules/suggested/*.md`` with
``enabled: false``. They never take effect until the operator reviews
and enables them. Max 5 drafts per session; a run is a no-op if no
heuristic fires.

Never raises: suggestion is best-effort and the session must not be
affected by it (``on_session_end`` is Observer-class; its return is
discarded anyway).
"""

import fcntl
import json
import os
import re
import tempfile
import time

import _rules

_HERE = os.path.dirname(os.path.abspath(__file__))
_SUGGESTED_DIR = os.path.join(_HERE, "rules", "suggested")

#: Shell command shapes that suggest a block rule.
_DANGEROUS_SHELL = [
    ("block-dangerous-shell-rm-rf", r"rm\s+-[a-z]*r[a-z]*f?\s+/(?:\s|$)",
     "rm -rf against the filesystem root",
     "rm\\s+-[a-z]*r[a-z]*f?\\s+/(\\s|$)"),
    ("block-dangerous-shell-mkfs", r"\bmkfs\b", "mkfs (filesystem creation)",
     "\\bmkfs\\b"),
    ("block-dangerous-shell-dd-dev", r"\bdd\b.*of=/dev/",
     "dd writing directly to a device", "\\bdd\\b.*of=/dev/"),
    ("block-dangerous-shell-forkbomb", r":\(\)\s*\{\s*:\|\:&\s*\}",
     "fork bomb", ":\\(\\)\\s*\\{\\s*:\\|\\:&\\s*\\}"),
    ("block-dangerous-shell-curl-pipe", r"(curl|wget)[^\n|]*\|\s*(ba)?sh",
     "piping a download straight into a shell",
     "(curl|wget)[^\\n|]*\\|\\s*(ba)?sh"),
]

#: Write-path shapes that suggest a warn rule.
_SENSITIVE_PATHS = [
    ("warn-sensitive-env", r"\.env(\.|$)", ".env files"),
    ("warn-sensitive-pem", r"\.pem$", ".pem private-key files"),
    ("warn-sensitive-ssh", r"\.ssh/|id_rsa|id_ed25519", "SSH key material"),
    ("warn-sensitive-creds", r"credential|secret", "credential/secret files"),
]

_TTL = 24 * 3600
_SESSION_RE = re.compile(r"[^A-Za-z0-9_.-]")


def _log_path(session_id):
    safe = _SESSION_RE.sub("_", session_id or "unknown")[:128] or "unknown"
    return os.path.join(tempfile.gettempdir(), "pantheon-hookify",
                        "calls-%s.json" % safe)


def log_call(session_id, tool, args_json):
    """Append one tool call to the per-session log. Never raises."""
    if not session_id:
        return
    path = _log_path(session_id)
    try:
        os.makedirs(os.path.dirname(path), exist_ok=True)
        fh = open(path, "a+", encoding="utf-8")
    except OSError:
        return
    try:
        fcntl.flock(fh, fcntl.LOCK_EX)
        try:
            fh.seek(0)
            entries = json.load(fh)
            if not isinstance(entries, list):
                entries = []
        except (ValueError, OSError):
            entries = []
        entries.append({"ts": time.time(), "tool": tool,
                        "args": (args_json or "")[:4000]})
        entries = entries[-200:]
        fh.seek(0)
        fh.truncate()
        json.dump(entries, fh)
        fcntl.flock(fh, fcntl.LOCK_UN)
    except Exception:
        pass
    finally:
        try:
            fh.close()
        except OSError:
            pass


def _read_log(session_id):
    path = _log_path(session_id)
    try:
        with open(path, encoding="utf-8") as fh:
            entries = json.load(fh)
    except (OSError, ValueError):
        return []
    now = time.time()
    out = [e for e in entries if isinstance(e, dict)
           and now - e.get("ts", 0) <= _TTL]
    try:
        if not out:
            os.unlink(path)
    except OSError:
        pass
    return out


def _covered_by_rules(rules, tool, args_dict, args_json):
    """True if any enabled rule already fires on this call.

    Semantic coverage beats substring heuristics: an existing
    ``block-dangerous-rm`` rule matches ``rm -rf / tmp`` regardless of
    how the draft regex is spelled.
    """
    engine = _rules.RuleEngine()
    try:
        blocking, warnings = engine.evaluate(rules, tool, args_dict,
                                             args_json)
    except Exception:
        return False
    return bool(blocking or warnings)


def _already_suggested(name):
    path = os.path.join(_SUGGESTED_DIR, name + ".md")
    return os.path.exists(path)


def _write_draft(name, fm_lines, message):
    try:
        os.makedirs(_SUGGESTED_DIR, exist_ok=True)
        path = os.path.join(_SUGGESTED_DIR, name + ".md")
        if os.path.exists(path):
            return False
        with open(path, "w", encoding="utf-8") as fh:
            fh.write("---\n")
            for ln in fm_lines:
                fh.write(ln + "\n")
            fh.write("---\n\n")
            fh.write(message + "\n")
        return True
    except OSError:
        return False


def suggest(session_id, rules):
    """Draft rules for repeated-but-uncovered patterns. Returns count.

    Never raises.
    """
    try:
        calls = _read_log(session_id)
    except Exception:
        return 0
    if not calls:
        return 0
    written = 0
    max_drafts = 5

    shells = [(e, _args_command(e)) for e in calls if e.get("tool") == "shell"]
    writes = [(e, _args_path(e)) for e in calls if e.get("tool") == "write_file"]

    for name, rx, why, pattern in _DANGEROUS_SHELL:
        if written >= max_drafts:
            break
        hits = [(e, cmd) for e, cmd in shells
                if cmd and re.search(rx, cmd)]
        if not hits or _already_suggested(name):
            continue
        e0, cmd0 = hits[-1]
        if _covered_by_rules(rules, "shell",
                             {"command": cmd0}, e0.get("args", "")):
            continue
        msg = ("Suggested block rule: the agent ran a shell command "
               "matching a dangerous pattern (%s), e.g.:\n\n    %s\n\n"
               "Review and enable if you want hookify to block this shape "
               "in future sessions." % (why, cmd0[:160]))
        if _write_draft(name,
                        ["name: %s" % name, "enabled: false", "event: shell",
                         "pattern: %s" % pattern, "action: block"],
                        msg):
            written += 1

    for name, rx, what in _SENSITIVE_PATHS:
        if written >= max_drafts:
            break
        hits = [(e, p) for e, p in writes if p and re.search(rx, p)]
        if not hits or _already_suggested(name):
            continue
        e0, p0 = hits[-1]
        if _covered_by_rules(rules, "write_file",
                             {"path": p0}, e0.get("args", "")):
            continue
        msg = ("Suggested warn rule: the agent wrote to a sensitive path "
               "(%s), e.g.:\n\n    %s\n\nReview and enable if you want "
               "hookify to warn on this shape in future sessions."
               % (what, p0[:160]))
        if _write_draft(name,
                        ["name: %s" % name, "enabled: false",
                         "event: write_file", "action: warn",
                         "conditions:",
                         " - field: path", "    operator: regex_match",
                         "    pattern: %s" % rx],
                        msg):
            written += 1
    return written


def _args_command(entry):
    try:
        return json.loads(entry.get("args") or "{}").get("command", "") or ""
    except (ValueError, TypeError):
        return ""


def _args_path(entry):
    try:
        return json.loads(entry.get("args") or "{}").get("path", "") or ""
    except (ValueError, TypeError):
        return ""
