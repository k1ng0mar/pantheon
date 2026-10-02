---
name: node-debug
description: "Use when debugging Node.js through the sandbox: scripted `node inspect` sessions, trace flags, and heap snapshots for memory issues."
origin: bundled
prerequisites: ["node installed by the setup wizard's Skill dependencies step (`pantheon setup`)"]
---

# Node debugging

> **Requires setup.** `node` must exist in the sandbox (installed by the setup wizard's Skill dependencies step).

Same constraint as Python: exec sessions are non-interactive, so the
built-in `node inspect` debugger is driven by scripted stdin, not by a
human at a `debug>` prompt. The full Chrome DevTools `--inspect` flow
needs a live client the sandbox does not provide; this skill does not
pretend otherwise.

## Purpose

Debug Node.js programs via the scripted CLI inspector and trace flags,
without an interactive terminal.

## Workflow

1. Reproduce with a plain `node script.js` run. Capture the full stack.
2. **Scripted inspector.** Pipe commands into `node inspect`:
   ```
   printf '%s\n' 'sb("app.js", 42)' 'c' 'exec("JSON.stringify(state)")' 'c' '.exit' | node inspect app.js
   ```
   Useful commands: `sb(<file>, <line>)` set breakpoint, `c` continue,
   `n` next, `s` step in, `exec("<expr>")` evaluate, `bt` backtrace,
   `list(n)`, `watch("<expr>")`, `.exit`.
   Read the output, then run a second pass with refined commands. One
   pass, read, next pass.
3. **Trace flags** for the quick wins before reaching for the debugger:
   `node --trace-uncaught --trace-warnings app.js`,
   `node --unhandled-rejections=strict app.js`,
   `node --trace-event-categories` for perf-adjacent mysteries.
4. **Memory issues**: `node --heapsnapshot-near-heap-limit=3
  --heapsnapshot-signal=SIGUSR2 app.js`, then send SIGUSR2 and analyze
   the snapshot offline. Growing heap across scripted runs is the
   signal; the snapshot names the retainer.
5. Fix the root cause (see `systematic-debugging`), add a regression
   test.

## Output contract

- Repro command and stack trace.
- Scripted inspector session: commands run and the state they revealed.
- Root cause and fix, with regression test result.

## Operating rules

1. Script every debugger interaction. If you need to react to output
   mid-session, split into passes.
2. `exec()` evaluates in the target: keep expressions side-effect free.
3. Async bugs: `node inspect` pauses per tick, which lies about timing.
   Prefer trace flags and targeted logging for race conditions; the
   step debugger will mislead you.
4. `--inspect` / `--inspect-brk` with a DevTools client is out of scope
   here: no live client exists in the sandbox. Do not start the process
   with `--inspect` and wait for a connection that never comes.
