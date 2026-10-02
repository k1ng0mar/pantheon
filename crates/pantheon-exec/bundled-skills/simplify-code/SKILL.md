---
name: simplify-code
description: "Use when code works but has rotted: dead code, duplication, oversized functions. Parallel bounded cleanup passes via subagents, no behavior change."
origin: bundled
---

# Simplify code

Cleanup is real work with a real failure mode: the "quick tidy" that
changes behavior and breaks three things. This skill makes cleanup safe by
making it parallel, bounded, and behavior-frozen: one subagent per target,
each with a mandate that forbids behavior change, tests green before and
after.

## Purpose

Reduce complexity (dead code, duplication, oversized functions, stale
comments) across a codebase without changing what it does.

## Workflow

1. Pick targets. Good targets: files flagged by inspection as large or
   tangled, modules with known duplication, dead code found by coverage
   or by reading. List them explicitly; "clean up the repo" is not a
   target.
2. Run the test suite first and record the baseline. If there is no
   suite, say so and shrink the ambition accordingly.
3. Spawn one subagent per target with a tight mandate: the files it may
   touch, the kind of cleanup allowed (delete dead code / collapse
   duplication / extract functions / update stale comments), and the hard
   rule: no behavior change, no new features, no dependency changes.
   Each agent returns a summary of what it changed and the test result.
4. Review each summary. Apply the ones that are pure simplification;
   reject or narrow anything that smells like a behavior change.
5. Run the full suite again. Green with the same count as baseline is
   the acceptance bar.

## Output contract

- Per target: what was removed or restructured, in concrete terms
  (functions deleted, lines removed, duplications collapsed).
- Test results before and after.
- Anything rejected, with the reason.

## Operating rules

1. No behavior change, ever, in a cleanup pass. If an agent proposes
   one, that is a separate task with its own review.
2. One target per agent. An agent told to "simplify the backend" will
   rewrite the backend.
3. Dead code is deleted, not commented out. Commented-out code is not
   simplification.
4. If the tests do not cover a target, the agent may only do deletions
   it can prove safe by reading all callers. Otherwise it reports the
   candidate and stops.
5. Cleanup commits stay separate from feature commits. Mixed diffs are
   unreviewable.
