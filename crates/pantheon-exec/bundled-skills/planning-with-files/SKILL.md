---
name: planning-with-files
description: "Use for multi-step work that will outlive one context window (roughly 5+ tool calls, or any task with phases): keep task_plan.md, findings.md, and progress.md on disk so the work survives compaction, a subagent boundary, or a lost session."
origin: bundled
---

# Planning with files

The context window is RAM: volatile and limited. The filesystem is disk:
persistent and unlimited. Anything that must survive a compaction, a
subagent spawn, or a session restart gets written to disk, because a
conversation that is still in the window is a plan nobody else can read.

This skill is for work with phases. A single-file fix does not need it; the
overhead of maintaining three files is real and the payoff is zero.

## The three files

| File | Holds | Update when |
|------|-------|-------------|
| `task_plan.md` | Phases, status, decisions | After each phase |
| `findings.md` | Research, discoveries, references | After any discovery |
| `progress.md` | Session log, test results, errors | Continuously |

Write them where the work lives, not in a temp directory. A plan in `/tmp` is
a plan the next session cannot find.

## Restore before continuing

If planning files already exist for this task, read all three before doing
anything else. Then run `git diff --stat`: code changes exist that the
planning files do not know about, and the plan is stale in a way that will
mislead the next step.

Do this even when the conversation feels continuous. A compaction looks like
a normal turn from the inside.

## The 2-action rule

After every two tool calls that produced information, write something to
disk. Not a summary of what you are about to do: the facts you just learned,
in a form that would let a different agent continue without asking you a
question.

Two calls is the trigger because that is roughly where the cost of losing
the output exceeds the cost of writing it down.

## Read versus write

| Situation | Action |
|-----------|--------|
| Just wrote a file | Do not read it back, it is still in context |
| Viewed an image or PDF | Write the findings now, before the pixels are gone |
| A tool returned data | Write it to `findings.md` |
| Starting a new phase | Read the plan and findings to re-orient |
| An error occurred | Read the relevant file to see current state |
| Resuming after a gap | Read all three files |

## Errors

Log every error to `progress.md` with what was tried and what happened. Do
not retry the same thing twice: after the second identical failure, change
the approach or stop and report. A third attempt at a known failure is not
persistence, it is a loop.

An error that looks like a bug in the tooling is worth its own line. "The
test fails because the fixture is malformed, not because the code is wrong"
saves the next agent an hour.

## Anti-patterns

- Planning files that restate the code. They record decisions and findings,
  not a summary of the diff, which git already has.
- A plan with more than one in-progress phase. Two means neither finishes.
- Editing the plan to match what happened instead of recording that it
  diverged. Divergence is information.
- Maintaining the files for a task small enough to hold in one context.