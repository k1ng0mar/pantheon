---
name: python-debug
description: "Use when debugging Python through the sandbox: scripted pdb sessions, post-mortem on tracebacks, and debugpy attach for long-running processes."
origin: bundled
prerequisites: ["python3 and debugpy installed by the setup wizard's Skill dependencies step (`pantheon setup`)"]
---

# Python debugging

> **Requires setup.** `python3` must exist in the sandbox (installed by the setup
> wizard's Skill dependencies step). Attach mode additionally needs `debugpy`
> in the target process's environment (also installed by that step).

Exec sessions are non-interactive: there is no one at a `(Pdb)` prompt.
Every debugger invocation here is scripted up front. That constraint is
the whole skill.

## Purpose

Debug Python programs via pdb and debugpy without an interactive terminal:
reproduce, inspect state, and find root causes through scripted debugger
runs.

## Workflow

1. Reproduce first with a plain script. Capture the full traceback.
2. **Scripted pdb.** Pass debugger commands with `-c`, chained:
   `python -m pdb -c "break app.py:42" -c continue -c "p variable" -c continue script.py arg1`
   Useful commands: `break <file>:<line>`, `continue`, `step`, `next`,
   `p <expr>`, `pp <expr>`, `where`, `up`/`down`, `list`.
3. **Post-mortem.** Let the exception drop you into the debugger, then
   script the inspection:
   `python -m pdb -c "where" -c "up" -c "p locals()" script.py`
   pdb enters post-mortem automatically on uncaught exceptions.
4. **Attach mode** for long-running processes (servers, workers): see
   `references/attach-mode.md`. Start the debugpy listener in the target,
   then drive the session from a script, still non-interactively.
5. Fix the root cause (see `systematic-debugging`), add a regression
   test.

## Output Contract

- Repro command and traceback.
- The scripted debugger session: commands run and the state they
  revealed.
- Root cause and fix, with regression test result.

## Operating Rules

1. Never assume an interactive prompt. If a session would need you to
   read output and then decide the next command, restructure it: run one
   scripted pass, read the output, then run the next pass.
2. `p` evaluates arbitrary expressions: keep them side-effect free.
   Mutating program state from the debugger and then drawing conclusions
   is how you debug a program that no longer exists.
3. For attach mode, the listener port must be reachable from the sandbox
   and the target must import debugpy before the code you care about
   runs. Details in `references/attach-mode.md`.
4. Strip debugger hooks (`breakpoint()`, `debugpy.listen`) before the
   code ships. Grep for them in the final diff.
