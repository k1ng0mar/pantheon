"""Time-gap awareness for Hermes.

Appends an implicit "elapsed time" note to the current user turn *only* when the
gap since the last exchange crosses a threshold - silence is the default, so a
continuous conversation is never interrupted.

Via the ``pre_llm_call`` hook the note rides on the user message at API-call
time only (never persisted, never touches the cached system prompt). The gap is
read from ``state.db`` (read-only) using the last persisted *assistant* message
- the moment the prior turn completed, which naturally excludes the current user
row. No in-process state; fails open on any error.

Set ``plugins.entries.time-gap.debug: true`` in config.yaml to append decisions
to ``debug.log`` next to this file.
"""

import sqlite3
import time
import traceback
from datetime import datetime
from pathlib import Path
from typing import Any, Dict, Optional

_MINUTE, _HOUR, _DAY = 60, 3600, 86400
_DEFAULT_MIN_GAP_SECONDS = 120 * _MINUTE
_DEFAULT_EXCLUDE_PLATFORMS = ("cron",)
_LOG_PATH = Path(__file__).resolve().parent / "debug.log"

# config.yaml doesn't change mid-run and load_config does file I/O - resolve once.
_config_cache: Optional[Dict[str, Any]] = None


def register(ctx) -> None:
    ctx.register_hook("pre_llm_call", _pre_llm_call)


def _log(msg: str) -> None:
    if not _plugin_config().get("debug"):
        return
    try:
        with open(_LOG_PATH, "a", encoding="utf-8") as fh:
            fh.write(f"[{time.strftime('%Y-%m-%d %H:%M:%S')}] {msg}\n")
    except Exception:
        pass  # a broken log path must never break a turn


def _plugin_config() -> Dict[str, Any]:
    """Pantheon port: config comes from PANTHEON_TIMEGAP_* env vars.
    PANTHEON_TIMEGAP_MIN_GAP (seconds), PANTHEON_TIMEGAP_DATE_CHANGE (0/1),
    PANTHEON_TIMEGAP_EXCLUDE (comma list), PANTHEON_TIMEGAP_DEBUG (0/1)."""
    global _config_cache
    if _config_cache is None:
        try:
            import os
            excl = os.environ.get("PANTHEON_TIMEGAP_EXCLUDE", "cron")
            _config_cache = {
                "min_gap_seconds": float(os.environ.get(
                    "PANTHEON_TIMEGAP_MIN_GAP", _DEFAULT_MIN_GAP_SECONDS)),
                "notify_date_change": os.environ.get(
                    "PANTHEON_TIMEGAP_DATE_CHANGE", "1") not in ("0", "false", "no"),
                "exclude_platforms": [x.strip() for x in excl.split(",") if x.strip()],
                "debug": os.environ.get("PANTHEON_TIMEGAP_DEBUG", "") in ("1", "true", "yes"),
            }
        except Exception:
            _config_cache = {}
    return _config_cache


def _min_gap_seconds(cfg: Dict[str, Any]) -> float:
    try:
        val = float(cfg.get("min_gap_seconds", _DEFAULT_MIN_GAP_SECONDS))
        return val if val > 0 else _DEFAULT_MIN_GAP_SECONDS
    except (TypeError, ValueError):
        return _DEFAULT_MIN_GAP_SECONDS


def _excluded(platform: str, cfg: Dict[str, Any]) -> bool:
    try:
        raw = cfg.get("exclude_platforms", _DEFAULT_EXCLUDE_PLATFORMS)
        return (platform or "").strip().lower() in {str(p).strip().lower() for p in raw}
    except TypeError:
        return (platform or "").strip().lower() in _DEFAULT_EXCLUDE_PLATFORMS


def _db_path():
    import os
    base = os.environ.get("PANTHEON_DATA_DIR", "")
    home = Path(base) if base else Path.home() / ".pantheon"
    return home / "ledger.db"


def _last_exchange_ts(session_id: str) -> Optional[float]:
    """Timestamp of the last assistant message, or None. Read-only, no write lock."""
    path = _db_path()
    if not session_id or not path.exists():
        return None
    conn = None
    try:
        conn = sqlite3.connect(
            f"file:{path}?mode=ro", uri=True, check_same_thread=False, timeout=1.0
        )
        try:
            (ts,) = conn.execute(
                "SELECT MAX(ts_ms) / 1000.0 FROM events WHERE run_id = ?",
                (session_id,),
            ).fetchone()
            if isinstance(ts, (int, float)) and ts > 0:
                return float(ts)
        except Exception:
            pass
        (ts,) = conn.execute(
            "SELECT MAX(timestamp) FROM messages "
            "WHERE session_id = ? AND role = 'assistant'",
            (session_id,),
        ).fetchone()
        return float(ts) if isinstance(ts, (int, float)) and ts > 0 else None
    except Exception:
        _log(f"DB read failed:\n{traceback.format_exc()}")
        return None
    finally:
        if conn is not None:
            conn.close()


def _local_date(epoch: float):
    """Calendar date at ``epoch`` in the user's configured timezone.

    Reuses Hermes' own timezone resolution so "midnight" matches the user's wall
    clock, not UTC. Falls back to server-local time if the helper is unavailable.
    """
    import os
    try:
        from zoneinfo import ZoneInfo
        tzname = os.environ.get("PANTHEON_TZ", "")
        if tzname:
            return datetime.fromtimestamp(epoch, ZoneInfo(tzname)).date()
    except Exception:
        pass
    try:
        from hermes_time import get_timezone

        return datetime.fromtimestamp(epoch, get_timezone()).date()
    except Exception:
        return datetime.fromtimestamp(epoch).astimezone().date()


def _days_crossed(prev: float, now: float) -> int:
    """Number of calendar-day boundaries between prev and now (0 if same day)."""
    return (_local_date(now) - _local_date(prev)).days


def _humanize(gap: float) -> str:
    """Coarse phrasing, rounded to the nearest tier unit."""
    if gap >= _DAY:
        days = round(gap / _DAY)
        return "about a day" if days <= 1 else f"about {days} days"
    if gap >= _HOUR:
        hours = round(gap / _HOUR)
        if hours >= 24:  # 23.5h+ rounds up - call it a day, not "24 hours".
            return "about a day"
        return "about an hour" if hours <= 1 else f"about {hours} hours"
    minutes = round(gap / _MINUTE)
    return "about a minute" if minutes <= 1 else f"about {minutes} minutes"


def _build_context(gap: float, days_crossed: int, now: float) -> str:
    """Compose the note from whichever signals fired"""
    head = f"[time-gap: {_humanize(gap)} have passed since the previous exchange"
    if days_crossed >= 1:
        today = _local_date(now)
        weekday = today.strftime('%A')
        today_iso = today.isoformat()
        if gap < _DAY:
            head += f", crossing into a new calendar day (today is {weekday}, {today_iso})]"
        else:
            head += f" (today is {weekday}, {today_iso})]"
    else:
        head += "]"
    return (
        f"{head} (Your own sense of time - never quote or mention it. Just factor it in: "
        "earlier context or the current date may be stale; re-ground if needed.)"
    )


def _pre_llm_call(*, session_id: str = "", platform: str = "", **_: Any):
    """Return time-gap context when a gap or a date rollover crosses, else None."""
    try:
        cfg = _plugin_config()
        if cfg.get("enabled", True) is False or _excluded(platform, cfg):
            return None
        prev = _last_exchange_ts(session_id)
        if prev is None:
            return None
        now = time.time()
        elapsed = now - prev
        days_crossed = (
            0
            if cfg.get("notify_date_change", True) is False
            else _days_crossed(prev, now)
        )
        # Two independent triggers: a large enough gap, OR a calendar rollover.
        if elapsed < _min_gap_seconds(cfg) and days_crossed < 1:
            return None
        context = _build_context(elapsed, days_crossed, now)
        _log(
            f"INJECT session={session_id[:16]} elapsed={int(elapsed)}s days={days_crossed}"
        )
        return {"context": context}
    except Exception:
        _log(f"error:\n{traceback.format_exc()}")
        return None
