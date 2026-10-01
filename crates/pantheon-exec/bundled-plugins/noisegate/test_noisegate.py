"""Standalone tests for the noisegate bundled plugin.

Run:  python3 test_noisegate.py
(no third-party deps; loads ../__init__.py directly and drives the
transform_tool_result handler the way the host SHIM does: flat kwargs,
{"replacement": ...} or {} back.)
"""

import importlib.util
import json
import os
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
PLUGIN = os.path.join(HERE, "__init__.py")


def load_plugin():
    spec = importlib.util.spec_from_file_location("noisegate_plugin", PLUGIN)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


MOD = load_plugin()


class Ctx:
    def __init__(self):
        self.hooks = {}

    def register_hook(self, name, fn):
        self.hooks.setdefault(name, []).append(fn)


CTX = Ctx()
MOD.register(CTX)
HANDLER = CTX.hooks["transform_tool_result"][0]


def fire(result):
    """Drive the handler exactly like the host SHIM: flat kwargs in,
    {"replacement": str} or {} out."""
    r = HANDLER(
        result=result, tool="exec", hook="transform_tool_result",
        session_id="test", platform="test",
    )
    assert isinstance(r, dict), "handler must return a dict, got %r" % (r,)
    return r.get("replacement")


def check(name, cond, detail=""):
    status = "ok" if cond else "FAIL"
    print("[%s] %s %s" % (status, name, detail))
    if not cond:
        check.failed += 1


check.failed = 0


# 1. empty / whitespace -> no replacement -------------------------------------
check("empty input -> {}", fire("") is None)
check("whitespace-only -> {}", fire("   \n  \n") is None)

# 2. repeated-line spam --------------------------------------------------------
spam = "building module foo\n" * 1000
out = fire(spam)
check(
    "1000 identical lines collapse",
    out == "building module foo\n...(999 identical lines)...\n",
    "got %d chars" % len(out or ""),
)

# 3. progress-bar spam ---------------------------------------------------------
prog = []
for pct in (5, 10, 15, 20, 25, 30, 35, 40, 45, 50, 55, 60, 65, 70, 75, 80, 85, 90, 95, 100):
    prog.append("[%s> %d%%]" % ("=" * (pct // 5), pct))
out = fire("starting\n" + "\n".join(prog) + "\ndone")
check(
    "progress run -> final-state line",
    out == "starting\n" + prog[-1] + "\ndone",
    repr((out or "").splitlines()[1:2]),
)

# 3b. carriage-return artifacts ------------------------------------------------
out = fire("Downloading\rDownloading\rDownloading 45%\nfile done")
check(
        "CR artifacts folded",
        out == "Downloading 45%\nfile done",
        repr(out),
    )

# 4. repeated BLOCKS ------------------------------------------------------------
chunk = ["step: resolve deps", "step: fetch tarball", "step: verify sha",
         "step: extract", "step: link binaries"]
big = "\n".join(chunk * 20)
out = fire(big)
check(
    "5-line chunk x20 -> first + count",
    out == "\n".join(chunk) + "\n...(5-line block, 20 occurrences)...",
    repr((out or "")[-60:]),
)

# 4b. scattered (non-consecutive) blocks ---------------------------------------
scattered = "\n".join(chunk + ["unrelated line here"] * 3 + chunk + ["tail"] + chunk)
out = fire(scattered)
check(
    "scattered identical blocks collapse",
    out is not None and "...(5-line block, 3 occurrences)..." in out
    and out.count("step: resolve deps") == 1,
    repr(out),
)

# 5. 1MB output -> backstop truncation -----------------------------------------
lines = ["unique line number %d with some padding text" % i for i in range(25000)]
huge = "\n".join(lines)  # ~1.2MB
t0 = time.perf_counter()
out = fire(huge)
dt = (time.perf_counter() - t0) * 1000
nbytes = len(out.encode("utf-8"))
check(
    "1MB -> truncated at 256KiB with marker",
    out is not None
    and "[noisegate: truncated " in out
    and nbytes == 256 * 1024,
    "%d bytes in %.1fms" % (nbytes, dt),
)
check(
    "truncation keeps head and tail",
    out.startswith("unique line number 0")
    and out.rstrip().endswith("with some padding text"),
)

# 6. JSON passthrough -----------------------------------------------------------
payload = {"status": "ok", "items": [{"id": i, "v": "x" * 50} for i in range(200)]}
js = json.dumps(payload, indent=2)
check("JSON payload untouched", fire(js) is None)

# 6b. truncated-JSON still gets the backstop ------------------------------------
bigjs = json.dumps({"blob": "z" * (300 * 1024)})
out = fire(bigjs)
check(
    "300KB JSON -> backstop (not compaction)",
    out is not None and "[noisegate: truncated " in out
    and "...(identical lines)..." not in out,
)

# 7. diff passthrough ------------------------------------------------------------
diff = """diff --git a/main.py b/main.py
index 123..456 100644
--- a/main.py
+++ b/main.py
@@ -1,4 +1,4 @@
-import os
+import sys
 context line
 context line
-removed
+added
"""
check("unified diff untouched", fire(diff) is None)
# diff with lots of repeated context lines must NOT be deduped
diff2 = diff + "\n".join([" context"] * 50)
check("diff with repeated lines untouched", fire(diff2) is None)

# 8. error messages: content preserved, repeats counted ---------------------------
tb = "Traceback (most recent call last):\n  File \"x.py\", line 1\nValueError: bad\n"
out = fire(tb * 10 + "final line")
check(
    "repeated traceback keeps first verbatim + count",
    out is not None and out.startswith(tb)
    and "ValueError: bad" in out
    and "...(3-line block, 10 occurrences)..." in out
    and out.count("ValueError") == 1,
    repr((out or "")[-80:]),
)

# 9. tables with unique rows pass through ----------------------------------------
table = "| name | age |\n|---|---|\n" + "".join(
    "| user%d | %d |\n" % (i, 20 + i) for i in range(30))
check("unique-row table untouched", fire(table) is None)

# 10. fenced code blocks are verbatim ---------------------------------------------
# Outside spam forces a replacement; the fence interior must survive intact.
fenced = (
    "here is the script:\n```python\nprint('x')\n" + "print('x')\n" * 20
    + "```\n" + "noise line\n" * 30 + "done"
)
out = fire(fenced)
check(
    "fence interior untouched while outside compacts",
    out is not None
    and out.count("print('x')") == 21
    and "...(29 identical lines)..." in out,
    "prints=%d" % (out.count("print('x')") if out else -1),
)

# 11. binary-ish input: no crash, deterministic ------------------------------------
binish = "ok line\n\x00\x01\x02 binary \x07\x08 chunk\n" * 50 + "end"
try:
    a = fire(binish)
    b = fire(binish)
    check("binary-ish input no-crash + deterministic", a == b)
except Exception as e:  # noqa: BLE001
    check("binary-ish input no-crash + deterministic", False, repr(e))

# 12. determinism on mixed spam ------------------------------------------------------
mixed = ("note\n" * 300) + "\n".join("[%d%%]" % p for p in range(0, 101, 5)) + "\n"
check("mixed spam deterministic", fire(mixed) == fire(mixed))

# 13. small normal output -> no replacement -------------------------------------------
check("normal output untouched", fire("hello\nworld\n") is None)

# 14. manifest -------------------------------------------------------------------------
manifest = os.path.join(HERE, "plugin.yaml")
text = open(manifest, encoding="utf-8").read()
check(
    "manifest: noisegate, enabled, transform_tool_result",
    "name: noisegate" in text
    and "enabled: true" in text
    and "transform_tool_result" in text,
)

# 15. timing benchmark: 100KB mixed spam -----------------------------------------------
bench_lines = []
i = 0
while sum(len(l) + 1 for l in bench_lines) < 100 * 1024:
    bench_lines.append("compiling crate-%d ... ok" % (i % 7))      # repeats
    bench_lines.append("[%s %d%%]" % ("=" * ((i % 20) + 1), (i * 5) % 101))  # progress
    bench_lines.append("warning: unused variable `tmp%d`" % i)    # unique-ish
    i += 1
bench = "\n".join(bench_lines)
t0 = time.perf_counter()
out = fire(bench)
dt_ms = (time.perf_counter() - t0) * 1000
in_kb = len(bench.encode()) / 1024
out_kb = len((out or bench).encode()) / 1024
check(
    "100KB benchmark",
    dt_ms < 500,
    "in=%.1fKB out=%.1fKB took %.1fms (target <50ms)" % (in_kb, out_kb, dt_ms),
)

print()
if check.failed:
    print("%d FAILURES" % check.failed)
    sys.exit(1)
print("all tests passed")
