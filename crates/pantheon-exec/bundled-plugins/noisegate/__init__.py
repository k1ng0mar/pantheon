"""noisegate: deterministic tool-output compaction for Pantheon.

Fires on the ``transform_tool_result`` hook, i.e. AFTER the tool has run and
BEFORE the model sees the result. Same input -> same output, always: no
model calls, no network, no clock, no randomness.

What it does, in order:

  1. Whole-payload JSON passes through untouched (machine data must parse).
  2. Whole-payload unified diffs pass through untouched (line counts matter).
  3. Carriage-return progress artifacts are folded (``a\\rb`` -> ``b``).
  4. Runs of progress/ticker lines collapse to the single final-state line.
  5. Consecutive identical lines collapse to ``...(N identical lines)...``.
  6. Repeated identical blocks (2-16 lines, consecutive or scattered)
     collapse to ``...(N-line block, M occurrences)...``; first kept.
  7. Single lines repeated 4+ times non-consecutively collapse similarly.
  8. Anything still over 256 KiB is hard-truncated head+tail with a
     ``[noisegate: truncated X→Y bytes]`` marker.

Content is never rewritten: the first occurrence of anything collapsed is
kept verbatim, and every collapse carries its count. Error text is never
altered, only collapsed with its count when it repeats.

Fail-open: any internal error returns the input unchanged (the host also
fails open on transform hooks, so a crash here can never break a turn).

Ordering vs ``compact_output`` (crates/pantheon-exec/src/lib.rs): that
function runs INSIDE tool execution (supervisor, builtins, vault and
browser tools cap at 64 KiB head+tail by default), while this hook fires
later in the session layer, after the tool returned. So this plugin sees
post-compaction text. The 256 KiB cap here is a backstop for tool paths
that bypass ``compact_output`` (custom registries, bare sessions), not a
second truncation of the same bytes; its marker text is deliberately
different so the two are never confused. See README.md.
"""

import json
import re

# ---------------------------------------------------------------------------
# Tuning constants. All fixed: no env, no clock, no randomness -> deterministic.
# ---------------------------------------------------------------------------

#: Backstop byte cap. Deliberately larger than compact_output's 64 KiB
#: default: this plugin runs after it, so anything this size bypassed it.
_MAX_BYTES = 256 * 1024
#: Head bytes kept by the backstop truncation (tail gets the rest).
_HEAD_BYTES = 64 * 1024
#: Longest repeated block we hunt for (covers the "5-line chunk" case).
_MAX_BLOCK_LINES = 16
#: A single line must occur this many times before non-consecutive
#: occurrences are collapsed.
_REPEAT_LINE_THRESHOLD = 4

_ERROR_RE = re.compile(
    r"(?i)(traceback|error|exception|failed|failure|fatal|panic|"
    r"denied|not found|enoent|eaddrinuse|segfault)"
)
_PCT_RE = re.compile(r"\d{1,3}\s*%")
_COUNT_RE = re.compile(r"^\d+\s*/\s*\d+\b")
_VERB_RE = re.compile(
    r"(?i)^(downloading|uploading|fetching|installing|loading|processing|"
    r"building|compiling|extracting|writing|reading|copying|moving|syncing|"
    r"indexing|generating|running|waiting|retrying)\b"
)
_FILL_RE = re.compile(r"[#=\u2588\u2593\u2592\u2591\u25b0\u25b1]{3,}")
_FENCE_RE = re.compile(r"^\s*(`{3,}|~{3,})")


def register(ctx):
    ctx.register_hook("transform_tool_result", _transform_tool_result)


def _transform_tool_result(**kwargs):
    """Hook entry point. Returns {"replacement": str} or {} (no change)."""
    text = kwargs.get("result", "")
    if not isinstance(text, str) or not text.strip():
        return {}
    try:
        out = _compact(text)
    except Exception:
        return {}  # fail-open: never break a turn
    if out == text:
        return {}
    return {"replacement": out}


# ---------------------------------------------------------------------------
# Pipeline
# ---------------------------------------------------------------------------


def _compact(text):
    # 1-2. Whole-payload guards: JSON and diffs pass through compaction
    # untouched. (The byte backstop at the end still applies: at 256 KiB+
    # the payload is a hazard regardless of shape.)
    if _looks_like_json(text):
        body = text
    elif _looks_like_diff(text):
        body = text
    else:
        # Split on "\n" only: splitlines() would also split on "\r",
        # which would destroy the CR-folding in the next step.
        lines = [ln.rsplit("\r", 1)[-1] for ln in text.split("\n")]
        body = "\n".join(_compact_lines(lines))
        # The split/join round-trip drops the trailing newline; put back
        # exactly what was there ("\n" or "\r\n").
        if text.endswith("\r\n"):
            eol = "\r\n"
        elif text.endswith("\n"):
            eol = "\n"
        else:
            eol = ""
        if eol and not body.endswith(eol):
            body += eol
    return _truncate_backstop(body)


def _compact_lines(lines):
    # Fenced code blocks are verbatim: compact only the prose between them.
    segments = _split_fences(lines)
    out = []
    for fenced, seg in segments:
        if fenced or not seg:
            out.extend(seg)
            continue
        seg = _collapse_progress_runs(seg)
        seg = _collapse_consecutive(seg)
        counts, top = _line_counts(seg)
        if top >= 3:
            seg = _collapse_consecutive_blocks(seg)
            seg = _collapse_scattered_blocks(seg)
            counts, top = _line_counts(seg)  # refresh: blocks reshaped lines
        seg = _collapse_repeated_lines(seg, counts)
        out.extend(seg)
    return out


def _line_counts(lines):
    """(counts, max) over non-blank lines. One pass, shared by the guard
    and the repeated-line collapse so neither re-scans."""
    counts = {}
    top = 0
    for ln in lines:
        if ln.strip():
            c = counts.get(ln, 0) + 1
            counts[ln] = c
            if c > top:
                top = c
    return counts, top


def _looks_like_json(text):
    s = text.strip()
    if len(s) < 2 or s[0] not in "{[":
        return False
    try:
        json.loads(s)
    except Exception:
        return False
    return True


def _looks_like_diff(text):
    """Conservative unified-diff heuristic: needs real diff markers."""
    strong = 0
    headers = 0
    plusminus = 0
    # maxsplit: never split more than we inspect (1MB inputs exist).
    for ln in text.split("\n", 400)[:400]:
        if ln.startswith(("diff --git ", "@@ ")):
            strong += 1
        elif ln.startswith(("--- ", "+++ ")):
            headers += 1
        elif len(ln) > 1 and ln[0] in "+-" and not ln.startswith(("+++", "---")):
            plusminus += 1
    return strong >= 1 or (headers >= 1 and plusminus >= 3)


def _split_fences(lines):
    """Split into (fenced, lines) segments; fence markers stay with content."""
    segments = []
    cur = []
    fenced = False
    for ln in lines:
        if _FENCE_RE.match(ln):
            if cur:
                segments.append((fenced, cur))
                cur = []
            fenced = not fenced
            segments.append((True, [ln]))
        else:
            cur.append(ln)
    if cur:
        segments.append((fenced, cur))
    return segments


_FILL_CHARS = ("#", "=", "\u2588", "\u2593", "\u2592", "\u2591", "\u25b0", "\u25b1")
_DIGIT_RE = re.compile(r"\d")


def _is_progress_line(line):
    s = line.strip()
    if not s or len(s) > 200:
        return False
    # Fast C-level `in` pre-filters first: most lines are decided here
    # without touching a regex. Semantics match the unrolled version:
    # percentage, N/M counter, verb+number, or fill-bar glyphs.
    if "%" in s:
        hit = _PCT_RE.search(s) is not None
    elif "/" in s:
        hit = _COUNT_RE.match(s) is not None
    elif s[0].isalpha():
        hit = _VERB_RE.match(s) is not None and _DIGIT_RE.search(s) is not None
    else:
        hit = False
        for ch in _FILL_CHARS:
            if ch in s:
                hit = _FILL_RE.search(s) is not None
                break
    if hit and _ERROR_RE.search(s):
        return False  # error text is never progress spam
    return hit


def _collapse_progress_runs(lines):
    """Runs of consecutive progress lines -> the last (final-state) line."""
    out = []
    run_last = None
    for ln in lines:
        if _is_progress_line(ln):
            run_last = ln  # keep overwriting; the last one is the state
            continue
        if run_last is not None:
            out.append(run_last)
            run_last = None
        out.append(ln)
    if run_last is not None:
        out.append(run_last)
    return out


def _collapse_consecutive(lines):
    """Consecutive identical lines -> first + ...(N identical lines)... .

    Runs of blank lines collapse to a single blank line silently:
    whitespace carries no information worth a marker.
    """
    out = []
    i = 0
    n = len(lines)
    while i < n:
        j = i + 1
        while j < n and lines[j] == lines[i]:
            j += 1
        run = j - i
        if run > 1:
            if lines[i].strip():
                out.append(lines[i])
                out.append("...(%d identical lines)..." % (run - 1))
            else:
                out.append(lines[i])
        else:
            out.append(lines[i])
        i = j
    return out


def _collapse_consecutive_blocks(lines):
    """Collapse immediately-repeating blocks, smallest period first.

    For a truly periodic stream (the same 5-line chunk printed 20 times)
    the natural unit is the smallest L with lines[i:i+L]==lines[i+L:i+2L];
    trying larger L first would fragment the stream into ragged blocks.
    Runs extend maximally: ``...(L-line block, M occurrences)...``.
    """
    n = len(lines)
    if n < 4:
        return lines
    out = []
    i = 0
    while i < n:
        period = 0
        max_l = min(_MAX_BLOCK_LINES, (n - i) // 2)
        for L in range(2, max_l + 1):
            if lines[i : i + L] == lines[i + L : i + 2 * L]:
                period = L
                break
        if not period:
            out.append(lines[i])
            i += 1
            continue
        count = 2
        while (
            i + (count + 1) * period <= n
            and lines[i : i + period]
            == lines[i + count * period : i + (count + 1) * period]
        ):
            count += 1
        out.extend(lines[i : i + period])
        out.append("...(%d-line block, %d occurrences)..." % (period, count))
        i += count * period
    return out


def _collapse_scattered_blocks(lines):
    """Collapse repeated identical blocks that are NOT adjacent.

    Consecutive repeats are already gone (handled above with the correct
    smallest-period unit), so this pass hunts the scattered case: the same
    2-16 line chunk recurring at distant positions. Larger blocks win.
    64-bit rolling hash over line ids, O(n) per block length, with exact
    verification on hash hits; overlapping candidates are skipped.
    """
    n = len(lines)
    if n < 4:
        return lines
    ids = {}
    seq = []
    for ln in lines:
        v = ids.get(ln)
        if v is None:
            v = len(ids)
            ids[ln] = v
        seq.append(v)

    base = 91138233
    mask = (1 << 64) - 1
    claimed = bytearray(n)  # lines already collapsed away
    # (keep_start, L) -> list of repeat starts
    collapses = {}

    top = min(_MAX_BLOCK_LINES, n // 2)
    for L in range(top, 1, -1):
        if n < 2 * L:
            continue
        # rolling hash over seq
        hashes = [0] * (n - L + 1)
        h = 0
        for k in range(L):
            h = (h * base + seq[k] + 1) & mask
        hashes[0] = h
        pw = 1
        for _ in range(L - 1):
            pw = (pw * base) & mask
        for i in range(1, n - L + 1):
            h = (h - ((seq[i - 1] + 1) * pw & mask)) & mask
            h = (h * base + seq[i + L - 1] + 1) & mask
            hashes[i] = h
        # prefix sums over claimed: O(1) overlap queries
        ps = [0] * (n + 1)
        for i, c in enumerate(claimed):
            ps[i + 1] = ps[i] + c
        first = {}  # hash -> [keep_start, end]
        for i, hh in enumerate(hashes):
            if ps[i + L] - ps[i]:
                continue
            g = first.get(hh)
            if g is None:
                first[hh] = [i, i + L]
                continue
            keep, end = g
            if i < end:
                continue  # overlapping; leave for a smaller L or as-is
            if seq[keep : keep + L] != seq[i : i + L]:
                continue  # 64-bit collision that isn't real
            for k in range(i, i + L):
                claimed[k] = 1
            collapses.setdefault((keep, L), []).append(i)
            g[1] = i + L

    if not collapses:
        return lines
    marker_after = {}
    for (keep, L), repeats in collapses.items():
        total = 1 + len(repeats)
        marker_after.setdefault(keep + L - 1, []).append(
            "...(%d-line block, %d occurrences)..." % (L, total)
        )
    out = []
    for i, ln in enumerate(lines):
        if claimed[i]:
            continue
        out.append(ln)
        out.extend(marker_after.get(i, ()))
    return out


def _collapse_repeated_lines(lines, counts):
    """Single lines occurring 4+ times non-consecutively: keep first + count.

    `counts` is the precomputed non-blank line histogram (see _line_counts);
    callers refresh it after any structural change so the counts in the
    marker are always honest.
    """
    hot = {ln for ln, c in counts.items() if c >= _REPEAT_LINE_THRESHOLD}
    if not hot:
        return lines
    seen = set()
    marker_after = {}
    drop = bytearray(len(lines))
    for i, ln in enumerate(lines):
        if ln in hot:
            if ln in seen:
                drop[i] = 1
            else:
                seen.add(ln)
                marker_after[i] = (
                    "...(repeated line, %d occurrences total)..." % counts[ln]
                )
    out = []
    for i, ln in enumerate(lines):
        if drop[i]:
            continue
        out.append(ln)
        if i in marker_after:
            out.append(marker_after[i])
    return out


def _truncate_backstop(text):
    """Hard byte cap. Head+tail so errors (usually at the tail) survive.

    The marker reserves its own space, so the final payload is exactly
    ``_MAX_BYTES`` and the ``X->Y`` numbers are honest.
    """
    data = text.encode("utf-8", errors="replace")
    if len(data) <= _MAX_BYTES:
        return text
    marker = "\n[noisegate: truncated %d\u2192%d bytes]\n" % (len(data), _MAX_BYTES)
    marker_b = marker.encode("utf-8")
    tail_len = _MAX_BYTES - _HEAD_BYTES - len(marker_b)
    head = data[:_HEAD_BYTES].decode("utf-8", errors="replace")
    tail = data[len(data) - tail_len :].decode("utf-8", errors="replace")
    return head + marker + tail
