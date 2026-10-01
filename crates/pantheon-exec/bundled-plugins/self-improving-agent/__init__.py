"""self-improving-agent: disciplined learning loop as a Pantheon hook plugin.

Adapted from pskoett's self-improving-agent skill (MIT-0, OpenClaw) for
Pantheon's Python hook protocol. OpenClaw install paths, the OpenClaw CLI,
and OpenClaw session tools are stripped; paths are remapped to Pantheon
(the workspace root is the home directory, where MEMORY.md / AGENTS.md /
SOUL.md / TOOLS.md live).

Hooks:
  on_session_start (Context) — ensure .learnings/ exists, then inject the
      logging protocol quick reference plus a pending-triage note.
  on_session_end (Observer) — ensure .learnings/ exists and record a
      session-end marker for the next session's triage note.

Deviation from upstream, stated plainly: the OpenClaw version swept the
ended session's transcript for error patterns at session end. Pantheon
hook children receive no transcript (only hook/session/run ids) and run
with a scrubbed environment, so automatic error extraction is not
possible here. Learning *content* comes from the agent following the
injected protocol; these hooks supply the reminder and the bookkeeping.

Security: local files only. The protocol text carries the upstream
warning — never log secrets, tokens, private keys, environment
variables, or full source/config files. This module never reads the
process environment beyond PATH/HOME resolution and never exfiltrates.
"""

import json
import os
import re
from datetime import datetime, timezone

ENTRY_RE = re.compile(r"^## \[(LRN|ERR|FEAT)-[^\]]+\]\s*(.*)$")
FIELD_RES = {
    "priority": re.compile(r"^\*\*Priority\*\*:\s*(.+)$"),
    "status": re.compile(r"^\*\*Status\*\*:\s*(.+)$"),
    "area": re.compile(r"^\*\*Area\*\*:\s*(.+)$"),
    "pattern_key": re.compile(r"^-\s*Pattern-Key:\s*(\S+)", re.I),
    "recurrence": re.compile(r"^-\s*Recurrence-Count:\s*(\d+)", re.I),
}
PATTERN_KEY_RE = re.compile(r"^[a-z][a-z0-9-]*\.[a-z][a-z0-9-]*$")

FILES = {
    "learnings": "LEARNINGS.md",
    "errors": "ERRORS.md",
    "features": "FEATURE_REQUESTS.md",
}

FILE_HEADERS = {
    "LEARNINGS.md": (
        "# Learnings\n\nCorrections, insights, and knowledge gaps captured during development.\n\n"
        "**Categories**: correction | insight | knowledge_gap | best_practice\n\n---\n"
    ),
    "ERRORS.md": "# Errors\n\nCommand failures and integration errors.\n\n---\n",
    "FEATURE_REQUESTS.md": "# Feature Requests\n\nCapabilities requested by the user.\n\n---\n",
}


def _plugin_dir():
    return os.path.dirname(os.path.abspath(__file__))


def resolve_learnings_dir(plugin_dir=None, env=None):
    """Where .learnings/ lives. Explicit config wins, then env, then home."""
    env = env if env is not None else os.environ
    plugin_dir = plugin_dir or _plugin_dir()
    cfg_path = os.path.join(plugin_dir, "config.json")
    try:
        with open(cfg_path, "r", encoding="utf-8") as f:
            cfg = json.load(f)
        configured = (cfg.get("learnings_dir") or "").strip()
        if configured:
            return os.path.expanduser(configured)
    except (OSError, ValueError):
        pass
    data_dir = env.get("PANTHEON_DATA_DIR", "").strip()
    if data_dir:
        return os.path.join(os.path.expanduser(data_dir), ".learnings")
    home = os.path.expanduser("~")
    return os.path.join(home, ".learnings")


def init_learnings(root):
    """First-use init: create .learnings/ and the three log files. Never overwrites."""
    created = []
    os.makedirs(root, exist_ok=True)
    for fname, header in FILE_HEADERS.items():
        path = os.path.join(root, fname)
        if not os.path.exists(path):
            with open(path, "w", encoding="utf-8") as f:
                f.write(header)
            created.append(fname)
    return created


def validate_pattern_key(key):
    """Pattern-Key must be exactly `area.symptom`, lowercase, hyphenated."""
    return bool(PATTERN_KEY_RE.match(key or ""))


def read_entries(path):
    """Parse log-file entries into dicts. Tolerant of hand-edited files."""
    entries = []
    try:
        with open(path, "r", encoding="utf-8") as f:
            lines = f.read().splitlines()
    except OSError:
        return entries
    cur = None
    for line in lines:
        m = ENTRY_RE.match(line.strip())
        if m:
            if cur:
                entries.append(cur)
            cur = {
                "kind": m.group(1),
                "id": m.group(0)[3:].split("]")[0],
                "title": m.group(2).strip(),
                "priority": "", "status": "", "area": "",
                "pattern_key": "", "recurrence": 1,
            }
            continue
        if cur is None:
            continue
        s = line.strip()
        for field, rx in FIELD_RES.items():
            fm = rx.match(s)
            if fm:
                if field == "recurrence":
                    try:
                        cur[field] = int(fm.group(1))
                    except ValueError:
                        pass
                else:
                    cur[field] = fm.group(1).strip().lower()
    if cur:
        entries.append(cur)
    return entries


def find_by_pattern_key(entries, key):
    """Dedup lookup: the stable key that catches reworded duplicates."""
    key = (key or "").strip().lower()
    return [e for e in entries if e.get("pattern_key") == key]


def pending_summary(root):
    """Counts of pending entries per file + the pending high-priority items."""
    summary = {"learnings": 0, "errors": 0, "features": 0, "high": []}
    for kind, fname in FILES.items():
        for e in read_entries(os.path.join(root, fname)):
            if e.get("status") == "pending":
                summary[kind] += 1
                if e.get("priority") == "high":
                    summary["high"].append((fname, e["id"], e["title"]))
    return summary


PROTOCOL_TEXT = """\
Logging protocol (self-improving-agent):
- Command/operation fails -> .learnings/ERRORS.md (error entry)
- User corrects you -> .learnings/LEARNINGS.md, category `correction`
- User wants a missing feature -> .learnings/FEATURE_REQUESTS.md
- API/external tool fails -> ERRORS.md with integration details
- Your knowledge was outdated -> LEARNINGS.md, category `knowledge_gap`
- Found a better approach -> LEARNINGS.md, category `best_practice`
- Entry format: `## [LRN-YYYYMMDD-XXX] category` with Logged/Priority/Status/Area,
  a `Pattern-Key: area.symptom` (exactly two lowercase hyphenated levels, e.g.
  `deps.module-not-found`), Recurrence-Count, and See Also links.
- Dedup before logging: grep by Pattern-Key first; on a hit, bump
  Recurrence-Count and Last-Seen instead of creating a new entry.
- Promote broadly-applicable learnings to the workspace files: behavioral
  patterns -> SOUL.md, tool gotchas -> TOOLS.md, workflows -> AGENTS.md.
  Promotion rule: Recurrence-Count >= 3, seen in 2+ distinct tasks, within 30 days.
- SECURITY: never log secrets, tokens, private keys, environment variables,
  or full source/config files unless the user explicitly asks for that level
  of detail. Prefer short summaries or redacted excerpts over raw output."""


def render_start_context(root, run_id):
    """Build the on_session_start injection: protocol + triage note."""
    init_learnings(root)
    s = pending_summary(root)
    total = s["learnings"] + s["errors"] + s["features"]
    lines = ["[self-improving-agent] Learning loop active. Log dir: %s" % root]
    if total:
        lines.append(
            "Pending triage: %d learning(s), %d error(s), %d feature request(s)."
            % (s["learnings"], s["errors"], s["features"])
        )
        for fname, eid, title in s["high"][:3]:
            lines.append("  HIGH pending in %s: [%s] %s" % (fname, eid, title[:80]))
    else:
        lines.append("No pending learnings to triage.")
    marker = os.path.join(root, ".last-session")
    try:
        with open(marker, "r", encoding="utf-8") as f:
            last = f.read().strip()
        if last and last != run_id:
            lines.append("Previous session marker: %s." % last[:64])
    except OSError:
        pass
    lines.append("")
    lines.append(PROTOCOL_TEXT)
    text = "\n".join(lines)
    return text[:4000]


def record_session_end(root, run_id):
    """Observer bookkeeping: init + stamp the session-end marker."""
    init_learnings(root)
    marker = os.path.join(root, ".last-session")
    stamp = "%s %s" % (datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"), run_id or "?")
    try:
        with open(marker, "w", encoding="utf-8") as f:
            f.write(stamp)
    except OSError:
        pass
    return stamp


def on_session_start(hook=None, session_id=None, run_id=None, platform=None, **kw):
    try:
        root = resolve_learnings_dir()
        return {"context": render_start_context(root, run_id or session_id or "")}
    except Exception:
        return {}


def on_session_end(hook=None, session_id=None, run_id=None, platform=None, **kw):
    # Observer: return value is ignored; the work is the side effect.
    try:
        root = resolve_learnings_dir()
        record_session_end(root, run_id or session_id or "")
    except Exception:
        pass
    return {}


def register(ctx):
    ctx.register_hook("on_session_start", on_session_start)
    ctx.register_hook("on_session_end", on_session_end)
