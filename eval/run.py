#!/usr/bin/env python3
"""Pantheon eval harness (wave 3 seed, freebuff lane).

Drives the real `pantheon` CLI against cases derived from Pantheon wave-1
behavior and Hermes-history regressions, plus optional `cargo test` gates.
Stdlib only; no dependencies.

Each case runs in a fresh sandbox (PANTHEON_DATA_DIR + empty PANTHEON_EXT_DIR)
so re-running the suite never inherits state from a previous run.

Usage:
  python3 eval/run.py                # run all active cases
  python3 eval/run.py --list         # show cases + why they would skip
  python3 eval/run.py --only <id>    # run a single case
  python3 eval/run.py --cargo-tests  # gate on `cargo test --workspace` first

Case JSON shape (eval/cases.json):
  commands : list of argv lists. First element "pantheon" subcommand, or the
             special probe "_compact <file>" (deterministic compaction check).
  setup    : {"dirs": [...], "files": {name: content}} inside the case sandbox.
             Content "#GENERATE_LINES:N" expands to "line 0..N-1".
  expect   : {key: [assertions]} where key is "<case_id>" (command 0 stdout)
             or "<case_id>__<n>" (command n stdout). "ANY" matches anything.
  post     : list of [argv..., assertion] run after the main commands.
  Assertion forms:
    contains:X (default when prefixed with nothing), equals:X, exists:X,
    and any assertion prefixed "not:" inverts the match.
  expect_fail : indices of commands expected to exit non-zero.
  skip     : reason string ("needs binary", "python3", ...) to skip the case.
Placeholders in args/paths: <TMP> = case sandbox dir, <VENDOR> = vendor/.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CASES = ROOT / "eval" / "cases.json"
POLICY = {"max_lines": 200, "head_lines": 60, "max_bytes": 64 * 1024}
CMD_TIMEOUT = 60


def find_binary():
    env = os.environ.get("PANTHEON_BIN")
    if env and Path(env).exists():
        return Path(env)
    local = ROOT / "target" / "debug" / "pantheon"
    if local.exists() and os.access(local, os.X_OK):
        return local
    return shutil.which("pantheon")


def find_python3():
    return shutil.which("python3")


def skip_reason(case, bin_path, py3):
    why = case.get("skip")
    if why == "needs binary" and bin_path is None:
        return "pantheon binary not found (cargo build -p pantheon-cli)"
    if why == "needs binary" and bin_path is not None:
        return None
    if why == "python3" and py3 is None:
        return "python3 not found"
    if why == "python3" and py3 is not None:
        return None
    return why  # None means run; any other string means skip for that reason


def fnv1a_hex(data: bytes) -> str:
    h = 0xCBF29CE484222325
    for b in data:
        h ^= b
        h = (h * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return f"{h:016x}"


def compact_output(text: str) -> str:
    """Mirror of pantheon-exec compact_output with the default policy."""
    lines = text.split("\n")
    if lines and lines[-1] == "":
        lines.pop()  # Rust str::lines() drops the trailing empty piece
    if len(lines) <= POLICY["max_lines"] and len(text.encode()) <= POLICY["max_bytes"]:
        return text
    tail_n = POLICY["max_lines"] - POLICY["head_lines"]
    head = lines[: POLICY["head_lines"]]
    tail = lines[len(lines) - tail_n:] if tail_n > 0 else []
    dropped = lines[len(head): len(lines) - len(tail)]
    dropped_text = "\n".join(dropped)
    out = "\n".join(head)
    out += (
        f"\n[... compacted: dropped {len(dropped)} lines, "
        f"{len(dropped_text.encode())} bytes, hash {fnv1a_hex(dropped_text.encode())} ...]\n"
    )
    out += "\n".join(tail)
    if len(out.encode()) > POLICY["max_bytes"]:
        out = out.encode()[: POLICY["max_bytes"]].decode(errors="ignore")
        out += "\n[... byte-cap ...]"
    return out


def materialize_setup(case, sandbox: Path):
    for d in case.get("setup", {}).get("dirs", []):
        (sandbox / d).mkdir(parents=True, exist_ok=True)
    for name, content in case.get("setup", {}).get("files", {}).items():
        p = sandbox / name
        p.parent.mkdir(parents=True, exist_ok=True)
        if content.startswith("#GENERATE_LINES:"):
            n = int(content.split(":", 1)[1])
            p.write_text("".join(f"line {i}\n" for i in range(n)))
        else:
            p.write_text(content)


def resolve(arg: str, sandbox: Path) -> str:
    return arg.replace("<TMP>", str(sandbox)).replace("<VENDOR>", str(ROOT / "vendor"))


def assertion_ok(assertion: str, stdout: str) -> bool:
    negated = assertion.startswith("not:")
    a = assertion[4:] if negated else assertion
    if a == "ANY":
        ok = True
    elif a.startswith("equals:"):
        ok = stdout.strip() == a[len("equals:"):]
    elif a.startswith("contains:"):
        ok = a[len("contains:"):] in stdout
    elif a.startswith("exists:"):
        ok = Path(resolve(a[len("exists:"):], Path("/"))).exists() or Path(
            resolve(a[len("exists:"):], Path(os.environ.get("PANTHEON_EVAL_TMP", "/tmp")))
        ).exists()
    else:
        ok = a in stdout  # bare string behaves as contains
    return not ok if negated else ok


def run_cmd(argv, env):
    try:
        p = subprocess.run(
            argv, env=env, cwd=ROOT, capture_output=True, text=True, timeout=CMD_TIMEOUT
        )
        return p.returncode, p.stdout, p.stderr
    except subprocess.TimeoutExpired:
        return 124, "", f"timeout after {CMD_TIMEOUT}s"
    except FileNotFoundError as e:
        return 127, "", str(e)


def run_case(case, bin_path, py3):
    """Returns (status, detail). status in {pass, fail, skip}."""
    sandbox = Path(tempfile.mkdtemp(prefix="pantheon-eval-"))
    env = dict(os.environ)
    env["PANTHEON_DATA_DIR"] = str(sandbox / "data")
    env["PANTHEON_EXT_DIR"] = str(sandbox / "ext")
    env["PANTHEON_EVAL_TMP"] = str(sandbox)
    os.makedirs(sandbox / "data", exist_ok=True)
    os.makedirs(sandbox / "ext", exist_ok=True)

    try:
        materialize_setup(case, sandbox)
        failures = []

        for i, cmd in enumerate(case.get("commands", [])):
            if cmd and cmd[0] == "_compact":
                path = Path(resolve(cmd[1], sandbox))
                out = compact_output(path.read_text())
                code, stdout, stderr = 0, out, ""
            else:
                argv = [str(bin_path)] + [resolve(a, sandbox) for a in cmd]
                code, stdout, stderr = run_cmd(argv, env)

            want_fail = i in case.get("expect_fail", [])
            if want_fail and code == 0:
                failures.append(f"cmd {i} should have failed but exited 0: {cmd}")
            if not want_fail and code != 0:
                failures.append(
                    f"cmd {i} exited {code}: {cmd}\n  stderr: {stderr.strip()[:400]}"
                )

            key = case["id"] if i == 0 else f"{case['id']}__{i}"
            for assertion in case.get("expect", {}).get(key, []):
                if not assertion_ok(assertion, stdout):
                    failures.append(f"cmd {i} stdout missing {assertion!r}")

        for post in case.get("post", []):
            *argv, assertion = post
            if assertion.startswith("exists:"):
                ok = Path(resolve(assertion[len("exists:"):], sandbox)).exists()
                if not ok:
                    failures.append(f"post: path does not exist: {assertion}")
                continue
            code, stdout, stderr = run_cmd(
                [str(bin_path)] + [resolve(a, sandbox) for a in argv], env
            )
            if code != 0:
                failures.append(f"post {argv} exited {code}: {stderr.strip()[:200]}")
            if not assertion_ok(assertion, stdout):
                failures.append(f"post {argv}: stdout missing {assertion!r}")

        if failures:
            return "fail", "; ".join(failures)
        return "pass", ""
    finally:
        shutil.rmtree(sandbox, ignore_errors=True)


def main():
    ap = argparse.ArgumentParser(description="Pantheon eval harness")
    ap.add_argument("--list", action="store_true", help="list cases and skip status")
    ap.add_argument("--only", help="run a single case by id")
    ap.add_argument("--cargo-tests", action="store_true", help="gate on cargo test first")
    args = ap.parse_args()

    spec = json.loads(CASES.read_text())
    cases = spec["cases"]
    if args.only:
        cases = [c for c in cases if c["id"] == args.only]
        if not cases:
            print(f"no case id {args.only!r}", file=sys.stderr)
            return 2

    bin_path = find_binary()
    py3 = find_python3()

    if args.list:
        for case in spec["cases"]:
            why = skip_reason(case, bin_path, py3)
            status = f"SKIP ({why})" if why else "RUN"
            print(f"{case['id']:<40} {status}  — {case['title']}")
        return 0

    results = []

    if args.cargo_tests:
        t0 = time.time()
        code, out, err = run_cmd(["cargo", "test", "--workspace", "--quiet"], dict(os.environ))
        status = "pass" if code == 0 else "fail"
        results.append(("cargo-tests", status, f"exit {code} in {time.time()-t0:.0f}s"))
        if code != 0:
            print(err[-3000:] or out[-3000:], file=sys.stderr)

    for case in cases:
        why = skip_reason(case, bin_path, py3)
        if why:
            results.append((case["id"], "skip", why))
            continue
        t0 = time.time()
        status, detail = run_case(case, bin_path, py3)
        results.append((case["id"], status, detail or f"{time.time()-t0:.1f}s"))

    width = max(len(r[0]) for r in results) if results else 10
    n_pass = n_fail = n_skip = 0
    for cid, status, detail in results:
        mark = {"pass": "PASS", "fail": "FAIL", "skip": "SKIP"}[status]
        print(f"{mark} {cid:<{width}}  {detail}")
        n_pass += status == "pass"
        n_fail += status == "fail"
        n_skip += status == "skip"

    print(f"\n{n_pass} passed, {n_fail} failed, {n_skip} skipped")
    return 1 if n_fail else 0


if __name__ == "__main__":
    sys.exit(main())
