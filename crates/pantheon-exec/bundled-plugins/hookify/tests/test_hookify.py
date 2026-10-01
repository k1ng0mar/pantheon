"""Unit tests for the hookify plugin. Stdlib only; run with:
    python3 tests/test_hookify.py
"""

import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
PLUGIN = os.path.dirname(HERE)
sys.path.insert(0, PLUGIN)

import _rules  # noqa: E402
import _suggest  # noqa: E402
import __init__ as hookify  # noqa: E402,E999

RULES_DIR = os.path.join(PLUGIN, "rules")


def test_shipped_rules_load():
    rules = _rules.load_rules(RULES_DIR)
    names = sorted(r.name for r in rules)
    assert names == ["block-dangerous-rm", "warn-curl-pipe-shell",
                     "warn-sensitive-files"], names
    assert all(r.enabled for r in rules)


def test_frontmatter_conditions_parsed():
    rule = _rules.load_rule_file(
        os.path.join(RULES_DIR, "warn-sensitive-files.md"))
    assert rule is not None
    assert rule.action == "warn"
    assert rule.conditions == [
        ("path", "regex_match", r"\.env(\.|$)|credentials|secrets|\.pem$|\.ssh/")
    ], rule.conditions


def test_legacy_pattern_shorthand_parsed():
    rule = _rules.load_rule_file(
        os.path.join(RULES_DIR, "block-dangerous-rm.md"))
    assert rule is not None
    assert rule.action == "block"
    assert rule.pattern == r"rm\s+-[a-z]*r[a-z]*f?\s+/(?:\s|$)"


def test_upstream_bash_event_maps_to_shell():
    engine = _rules.RuleEngine()
    rules = _rules.load_rules(RULES_DIR)
    args = json.dumps({"command": "rm -rf / tmp/x"})
    blocking, _ = engine.evaluate(rules, "shell",
                                  {"command": "rm -rf / tmp/x"}, args)
    assert [r.name for r in blocking] == ["block-dangerous-rm"], \
        [r.name for r in blocking]


def test_file_event_maps_to_write_file():
    engine = _rules.RuleEngine()
    rules = _rules.load_rules(RULES_DIR)
    args = json.dumps({"path": "/tmp/x.env", "content": "A=1"})
    _, warnings = engine.evaluate(rules, "write_file",
                                 {"path": "/tmp/x.env", "content": "A=1"},
                                 args)
    assert [r.name for r in warnings] == ["warn-sensitive-files"], \
        [r.name for r in warnings]


def test_safe_commands_do_not_match():
    engine = _rules.RuleEngine()
    rules = _rules.load_rules(RULES_DIR)
    args = json.dumps({"command": "ls -la /tmp"})
    blocking, warnings = engine.evaluate(rules, "shell",
                                         {"command": "ls -la /tmp"}, args)
    assert blocking == [] and warnings == []


def test_operators():
    engine = _rules.RuleEngine()
    mk = lambda field, op, pattern: _rules.Rule(
        name="t", enabled=True, event="all", action="warn", pattern=None,
        message="m", conditions=[(field, op, pattern)], tool_matcher=None,
        source="test")
    a = {"command": "echo Hello World", "path": "/x/y.txt"}
    aj = json.dumps(a)
    assert engine._rule_matches(mk("command", "contains", "hello"), "shell",
                               a, aj)
    assert engine._rule_matches(mk("command", "not_contains", "bye"), "shell",
                               a, aj)
    assert engine._rule_matches(mk("path", "starts_with", "/x"), "write_file",
                                a, aj)
    assert engine._rule_matches(mk("path", "ends_with", ".txt"), "write_file",
                                a, aj)
    assert engine._rule_matches(mk("tool", "equals", "shell"), "shell", a, aj)
    assert not engine._rule_matches(mk("command", "contains", "zzz"), "shell",
                                      a, aj)
    # Unknown operator never matches.
    assert not engine._rule_matches(mk("command", "frobnicates", "x"),
                                    "shell", a, aj)
    # Bad regex never matches and never raises.
    assert not engine._rule_matches(mk("command", "regex_match", "(["),
                                    "shell", a, aj)


def test_tool_matcher_overrides_event():
    engine = _rules.RuleEngine()
    rule = _rules.Rule(name="t", enabled=True, event="all", action="warn",
                       pattern=None, message="m", conditions=[],
                       tool_matcher="write_file", source="test")
    assert engine._event_matches(rule, "write_file")
    assert not engine._event_matches(rule, "shell")


def test_block_denies_with_reason():
    rec = {}

    class Ctx:
        def register_hook(self, name, fn):
            rec[name] = fn

    hookify.register(Ctx())
    assert set(rec) == {"pre_tool_call", "transform_tool_result",
                        "on_session_end"}, set(rec)
    out = rec["pre_tool_call"](tool="shell",
                               args=json.dumps({"command": "rm -rf / a"}),
                               session_id="hsess1")
    assert out.get("deny") is True, out
    assert "block-dangerous-rm" in out.get("reason", ""), out


def test_warn_stashed_then_prepended():
    import shutil
    rec = {}

    class Ctx:
        def register_hook(self, name, fn):
            rec[name] = fn

    hookify.register(Ctx())
    out = rec["pre_tool_call"](
        tool="write_file",
        args=json.dumps({"path": "secrets.env", "content": "K=1"}),
        session_id="hsess2")
    assert out == {}, out
    out = rec["transform_tool_result"](tool="write_file", result="done",
                                       session_id="hsess2")
    assert "replacement" in out, out
    assert "warn-sensitive-files" in out["replacement"]
    assert out["replacement"].endswith("\ndone")
    # Consumed: second transform is a no-op.
    out = rec["transform_tool_result"](tool="write_file", result="done",
                                       session_id="hsess2")
    assert out == {}, out


def test_hooks_never_raise_on_garbage():
    rec = {}

    class Ctx:
        def register_hook(self, name, fn):
            rec[name] = fn

    hookify.register(Ctx())
    assert rec["pre_tool_call"](tool=None, args=None,
                               session_id=None) == {}
    assert rec["pre_tool_call"](tool="shell", args="not-json",
                               session_id="x") == {}
    assert rec["transform_tool_result"](tool=None, result=None,
                                        session_id=None) == {}
    assert rec["on_session_end"](session_id=None) == {}


def test_suggester_drafts_disabled_rule():
    import shutil
    sess = "suggest-test-1"
    # Clean any prior state.
    _suggest._read_log(sess)
    for i in range(3):
        _suggest.log_call(sess, "shell",
                          json.dumps({"command": "mkfs /dev/sda%d" % i}))
    rules = _rules.load_rules(RULES_DIR)
    draft = os.path.join(_suggest._SUGGESTED_DIR,
                         "block-dangerous-shell-mkfs.md")
    if os.path.exists(draft):
        os.unlink(draft)
    n = _suggest.suggest(sess, rules)
    assert n == 1, n
    assert os.path.exists(draft)
    rule = _rules.load_rule_file(draft)
    assert rule is not None and not rule.enabled
    assert rule.action == "block"
    # A second run must not duplicate the draft.
    assert _suggest.suggest(sess, rules) == 0
    os.unlink(draft)


def test_suggester_skips_covered_patterns():
    sess = "suggest-test-2"
    _suggest._read_log(sess)
    _suggest.log_call(sess, "shell",
                      json.dumps({"command": "rm -rf / tmp"}))
    rules = _rules.load_rules(RULES_DIR)  # covers rm -rf already
    n = _suggest.suggest(sess, rules)
    assert n == 0, n


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
