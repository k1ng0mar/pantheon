"""Per-session finding relay for the security-guidance plugin.

Pantheon's hook runner spawns a FRESH process per hook fire, so a
``pre_tool_call`` fire cannot hand findings to the later
``transform_tool_result`` fire in memory. This module is the side
channel: small JSON files under the OS temp dir, keyed by sanitized
session id.

Design notes:

- Best-effort by contract. Every function swallows its own errors and
  callers additionally guard; a relay failure must degrade to "no
  advisory", never to a crash. (On the ``pre_tool_call`` GATE hook a
  crash would fail CLOSED and deny the tool call - see __init__.py.)
- Entries carry a timestamp; anything older than ``_TTL`` seconds is
  dropped on read. Files are removed once empty.
- ``fcntl.flock`` serialises concurrent fires from parallel tool calls.
- Files live in the temp dir, not the plugin dir: bundled plugin dirs
  may be read-only, and findings are session-ephemeral anyway.
"""

import fcntl
import json
import os
import re
import tempfile
import time

#: Findings older than this are dropped on read (seconds).
_TTL = 600
#: Hard cap on queued entries per session file.
_MAX_ENTRIES = 50
_SESSION_RE = re.compile(r"[^A-Za-z0-9_.-]")


def _dir():
    d = os.path.join(tempfile.gettempdir(), "pantheon-security-guidance")
    try:
        os.makedirs(d, exist_ok=True)
    except OSError:
        pass
    return d


def _path(session_id):
    safe = _SESSION_RE.sub("_", session_id or "unknown")[:128] or "unknown"
    return os.path.join(_dir(), "findings-%s.json" % safe)


def _read_locked(path):
    """Read+prune the entries file. Returns (entries, fileobj-or-None)."""
    try:
        fh = open(path, "r+", encoding="utf-8")
    except OSError:
        return [], None
    try:
        fcntl.flock(fh, fcntl.LOCK_EX)
        try:
            entries = json.load(fh)
        except (ValueError, OSError):
            entries = []
        now = time.time()
        entries = [e for e in entries
                   if isinstance(e, dict) and now - e.get("ts", 0) <= _TTL]
        return entries, fh
    except OSError:
        try:
            fh.close()
        except OSError:
            pass
        return [], None


def _write_locked(fh, path, entries):
    try:
        fh.seek(0)
        fh.truncate()
        json.dump(entries[-_MAX_ENTRIES:], fh)
        fh.flush()
    finally:
        try:
            fcntl.flock(fh, fcntl.LOCK_UN)
        except OSError:
            pass
        try:
            fh.close()
        except OSError:
            pass
    if not entries:
        try:
            os.unlink(path)
        except OSError:
            pass


def append(session_id, tool, findings):
    """Queue ``findings`` (list of {"rule","reminder"}) for later pickup.

    Never raises.
    """
    if not findings:
        return
    path = _path(session_id)
    try:
        entries, fh = _read_locked(path)
        if fh is None:
            try:
                fh = open(path, "w", encoding="utf-8")
                fcntl.flock(fh, fcntl.LOCK_EX)
            except OSError:
                return
        entries.append({
            "ts": time.time(),
            "tool": tool,
            "findings": [
                {"rule": f.get("rule", "?"), "reminder": f.get("reminder", "")}
                for f in findings
            ],
        })
        _write_locked(fh, path, entries)
    except Exception:
        pass


def pop(session_id, tool):
    """Take and return queued findings for ``tool`` (list of dicts).

    Consumed entries are removed. Never raises; returns [] on any failure.
    """
    path = _path(session_id)
    try:
        entries, fh = _read_locked(path)
        if fh is None:
            return []
        kept, taken = [], []
        for e in entries:
            if e.get("tool") == tool:
                taken.extend(e.get("findings", []))
            else:
                kept.append(e)
        _write_locked(fh, path, kept)
        return taken
    except Exception:
        return []
