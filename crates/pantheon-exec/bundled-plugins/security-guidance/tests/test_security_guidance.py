"""Unit tests for the security-guidance plugin. Stdlib only; run with:
    python3 tests/test_security_guidance.py
"""

import json
import os
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
PLUGIN = os.path.dirname(HERE)
sys.path.insert(0, PLUGIN)

import _findings  # noqa: E402
import patterns  # noqa: E402
import __init__ as sg  # noqa: E402,E999


def _reset_relay():
    d = _findings._dir()
    for f in os.listdir(d):
        if f.startswith("findings-"):
            try:
                os.unlink(os.path.join(d, f))
            except OSError:
                pass


def test_upstream_pattern_count():
    # The full 25-rule upstream set must be present.
    assert len(patterns.SECURITY_PATTERNS) == 25, \
        len(patterns.SECURITY_PATTERNS)


def test_scan_text_pickle():
    hits = patterns.scan_text("a.py", "import pickle\npickle.loads(payload)")
    rules = [h["rule"] for h in hits]
    assert "pickle_deserialization" in rules, rules


def test_scan_text_path_filter():
    # innerHTML_xss only applies to JS-family files.
    hits = patterns.scan_text("app.js", "el.innerHTML = userInput")
    assert "innerHTML_xss" in [h["rule"] for h in hits]
    hits = patterns.scan_text("notes.md", "el.innerHTML = userInput")
    assert "innerHTML_xss" not in [h["rule"] for h in hits]


def test_scan_secrets_never_echoes():
    secret = "AKIAIOSFODNN7EXAMPLE"
    hits = patterns.scan_secrets("token=" + secret)
    assert any(h["rule"] == "aws_access_key" for h in hits)
    for h in hits:
        assert secret not in json.dumps(h), h


def test_scan_secrets_private_key():
    hits = patterns.scan_secrets(
        "-----BEGIN RSA PRIVATE KEY-----\nMIIBOwIBAAJBAK...")
    assert any(h["rule"] == "private_key" for h in hits)


def test_relay_roundtrip_and_pop_consumes():
    _reset_relay()
    _findings.append("sess1", "write_file",
                     [{"rule": "r", "reminder": "do not do x"}])
    got = _findings.pop("sess1", "write_file")
    assert got == [{"rule": "r", "reminder": "do not do x"}], got
    # Consumed entries are gone.
    assert _findings.pop("sess1", "write_file") == []


def test_relay_pop_is_tool_specific():
    _reset_relay()
    _findings.append("sess2", "write_file", [{"rule": "a", "reminder": "b"}])
    _findings.append("sess2", "shell", [{"rule": "c", "reminder": "d"}])
    got = _findings.pop("sess2", "shell")
    assert [f["rule"] for f in got] == ["c"]
    # write_file entry survives.
    assert [f["rule"] for f in _findings.pop("sess2", "write_file")] == ["a"]


def test_pre_tool_call_never_raises_and_stashes():
    _reset_relay()
    rec = {}

    class Ctx:
        def register_hook(self, name, fn):
            rec[name] = fn

    sg.register(Ctx())
    args = json.dumps({"path": "vuln.py",
                       "content": "import pickle\npickle.loads(x)"})
    out = rec["pre_tool_call"](tool="write_file", args=args,
                               session_id="s3")
    assert out == {}, out
    got = _findings.pop("s3", "write_file")
    assert any("pickle" in f["rule"] for f in got), got


def test_pre_tool_call_allows_on_anything():
    class Ctx:
        def register_hook(self, name, fn):
            pass
    # Garbage inputs must still yield an allow, never raise.
    out = sg._pre_tool_call(tool=None, args=None, session_id=None)
    assert out == {}, out
    out = sg._pre_tool_call(tool="write_file", args="not-json",
                            session_id="x")
    assert out == {}, out


def test_transform_prepends_banner():
    _reset_relay()
    _findings.append("s4", "shell",
                     [{"rule": "x", "reminder": "y advisory"}])
    out = sg._transform_tool_result(tool="shell", result="ok",
                                    session_id="s4")
    assert "replacement" in out, out
    assert "[security-guidance]" in out["replacement"]
    assert "y advisory" in out["replacement"]
    assert out["replacement"].endswith("\nok")


def test_transform_flags_leaked_secret_in_result():
    _reset_relay()
    out = sg._transform_tool_result(tool="shell",
                                    result="export KEY=AKIAIOSFODNN7EXAMPLE",
                                    session_id="s5")
    assert "replacement" in out, out
    assert "aws_access_key" in out["replacement"]
    assert "AKIAIOSFODNN7EXAMPLE" not in out["replacement"]
    assert "[redacted:aws_access_key]" in out["replacement"]


def test_redact_secrets_replaces_span():
    text, hits = patterns.redact_secrets("k=AKIAIOSFODNN7EXAMPLE tail")
    assert "AKIAIOSFODNN7EXAMPLE" not in text
    assert "[redacted:aws_access_key]" in text
    assert [h["rule"] for h in hits] == ["aws_access_key"]
    # Clean text passes through untouched.
    text, hits = patterns.redact_secrets("nothing here")
    assert text == "nothing here" and hits == []


def test_transform_noop_when_clean():
    _reset_relay()
    out = sg._transform_tool_result(tool="shell", result="hello",
                                    session_id="s6")
    assert out == {}, out


if __name__ == "__main__":
    this = sys.modules[__name__]
    tests = [(k, v) for k, v in sorted(vars(this).items())
             if k.startswith("test_") and callable(v)]
    failed = 0
    for name, fn in tests:
        try:
            fn()
        except Exception as e:
            failed += 1
            print("FAIL %s: %r" % (name, e))
        else:
            print("ok   %s" % name)
    print("%d/%d passed" % (len(tests) - failed, len(tests)))
    if failed:
        sys.exit(1)
