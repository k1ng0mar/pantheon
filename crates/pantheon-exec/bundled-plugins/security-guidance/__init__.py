"""security-guidance: advisory security review of tool calls for Pantheon.

Adapted from anthropics/claude-plugins-official
``plugins/security-guidance`` (Apache-2.0). Upstream fires pattern
hooks on Claude Code's PostToolUse and injects warnings via
``additionalContext``; the second half runs an async LLM diff review.

Pantheon port notes (read before changing the hook wiring):

- Pantheon's ``post_tool_call`` is an Observer-class hook: it fires
  asynchronously, receives only run_id/call_id/tool (no arguments, no
  result), and its return value is discarded by design. Claude's
  PostToolUse -> additionalContext mapping is therefore NOT portable,
  so this plugin does not register ``post_tool_call``.
- The supported per-call ADVISORY path to the model is
  ``transform_tool_result`` (Transform class, fail-open): the plugin
  returns a ``replacement`` payload with warnings prepended to the
  tool result the model sees.
- Tool ARGUMENTS are visible only at ``pre_tool_call`` (Gate class).
  This plugin registers there purely to SCAN the arguments of
  code-writing tools (``write_file`` content, ``shell`` commands) and
  stash findings for the later transform fire. It NEVER denies:
  ``pre_tool_call`` fails CLOSED on plugin error/timeout, so every
  code path below is wrapped to return silence ({}) instead of
  raising. A crash here would deny every tool call in the session.
- The upstream async LLM diff-review half is OMITTED: a Pantheon hook
  subprocess receives one JSON line on stdin and returns one JSON
  line on stdout; there is no supported path to reach the model and
  no backchannel is invented here. (Deferred, pending a host-side
  review facility.)

Behavior:

- ``pre_tool_call``: scan arguments of ``write_file`` (full 25-rule
  pattern set on the content, gated by file extension) and ``shell``
  (secret shapes + full set on the command), plus secret shapes on any
  other tool's raw argument JSON. Findings are stashed per session.
  Always returns {} (allow).
- ``transform_tool_result``: pops stashed input findings for the tool
  and scans the RESULT for leaked credentials (API keys, private
  keys, tokens — a Pantheon addition). Matched spans are REDACTED
  (replaced with ``[redacted:<rule>]``) so secrets never reach the
  model, and a compact banner is prepended; otherwise {} (no change).

Stdlib only. No network, no model calls. Target <100ms per fire.
"""

import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import _findings  # noqa: E402
import patterns  # noqa: E402

#: Upper bound on scanned text per fire; keeps the <100ms budget.
_MAX_SCAN_CHARS = 200_000
#: Max findings surfaced per message (context budget).
_MAX_FINDINGS = 4
#: Max banner bytes (findings + secrets).
_MAX_BANNER = 6000

_BANNER_HEAD = (
    "[security-guidance] Advisory findings (NOT a block — review and continue):\n"
)


def register(ctx):
    ctx.register_hook("pre_tool_call", _pre_tool_call)
    ctx.register_hook("transform_tool_result", _transform_tool_result)


def _pre_tool_call(**kwargs):
    """Scan tool arguments; stash findings; never deny, never raise."""
    try:
        tool = kwargs.get("tool", "") or ""
        args = kwargs.get("args", "") or ""
        session_id = kwargs.get("session_id", "") or ""
        path, text = _extract_scannable(tool, args)
        if not text:
            return {}
        findings = []
        if tool in ("write_file", "shell"):
            # Full vuln-pattern set only where there is real code/content
            # to scan; path_filter gates apply via the file path.
            findings = patterns.scan_text(path, text[:_MAX_SCAN_CHARS])
        # Secret shapes in arguments of any tool (e.g. a key pasted into
        # a shell command or a write).
        for hit in patterns.scan_secrets(text[:_MAX_SCAN_CHARS]):
            findings.append({
                "rule": "secret_in_arguments:" + hit["rule"],
                "reminder": ("Credential-shaped text in tool arguments. "
                             + hit["guidance"]),
            })
        if findings:
            _findings.append(session_id, tool, findings)
    except Exception:
        pass
    return {}


def _transform_tool_result(**kwargs):
    """Prepend advisories to the tool result the model sees."""
    try:
        tool = kwargs.get("tool", "") or ""
        result = kwargs.get("result", "") or ""
        session_id = kwargs.get("session_id", "") or ""
        input_findings = _findings.pop(session_id, tool)
        redacted, secret_hits = ("", [])
        if isinstance(result, str) and result:
            redacted, secret_hits = patterns.redact_secrets(
                result[:_MAX_SCAN_CHARS])
        if not input_findings and not secret_hits:
            return {}
        parts = [_BANNER_HEAD]
        if input_findings:
            parts.append(
                "From the arguments of a recent `%s` call in this session:"
                % tool)
            for f in input_findings[:_MAX_FINDINGS]:
                parts.append("- [%s]\n%s" % (f["rule"], f["reminder"]))
            if len(input_findings) > _MAX_FINDINGS:
                parts.append("(...%d more findings omitted...)"
                             % (len(input_findings) - _MAX_FINDINGS))
        if secret_hits:
            parts.append("Possible leaked credentials in this tool's output "
                         "have been REDACTED (matched spans replaced with "
                         "[redacted:<rule>]); do not reproduce them.")
            for h in secret_hits:
                parts.append("- [%s] %s" % (h["rule"], h["guidance"]))
        banner = "\n".join(parts)
        if len(banner) > _MAX_BANNER:
            banner = banner[:_MAX_BANNER] + "\n(...banner truncated...)"
        return {"replacement": banner + "\n\n" + redacted}
    except Exception:
        return {}


def _extract_scannable(tool, args):
    """Return (path, text) to scan from a tool's JSON argument string."""
    try:
        data = json.loads(args) if isinstance(args, str) else {}
    except (ValueError, TypeError):
        data = {}
    if not isinstance(data, dict):
        data = {}

    def _s(v):
        return v if isinstance(v, str) else ""

    if tool == "write_file":
        return _s(data.get("path")), _s(data.get("content"))
    if tool == "shell":
        return "", _s(data.get("command"))
    # Unknown tool: scan the raw argument JSON for secret shapes only.
    # (Full vuln patterns need a file path for their path_filter gates;
    # without one they would false-positive on prose.)
    return "", args if isinstance(args, str) else ""
