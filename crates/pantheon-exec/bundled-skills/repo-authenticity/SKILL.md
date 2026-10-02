---
name: repo-authenticity
description: "Write code that belongs in the repository: repo-first, smallest correct change, reuse before abstracting, verify behavior. Trigger when implementing a feature, fixing a bug, refactoring, or reviewing a diff."
origin: bundled
---

# Repository authenticity

Produce code that looks and behaves like it was written by a competent
contributor who understands this repository - not generic AI code. The
repository is the primary source of truth: existing code, abstractions,
conventions, tests, and dependency versions determine implementation
decisions. Full policy: `docs/code-generation-policy.md`.

## Workflow

1. **Inspect before writing.** Read the neighboring files, module,
   types, error handling, tests, and config for the area you are
   changing. For larger changes, inspect the subsystem architecture
   first. Check git history when something looks strange - it may be
   a workaround for a real constraint.
2. **Reuse before creating.** Search for existing functions, types,
   traits, errors, and config fields. Extend an existing mechanism
   rather than building a parallel one.
3. **Smallest correct change.** Change only what the requirement needs.
   No unrelated refactoring, renaming, cleanup, or modernization.
   Every changed file must have a reason.
4. **Verify, don't narrate.** Format, compile, run targeted tests,
   then re-read the final diff as a reviewer. Never claim a check ran
   when it didn't. Distinguish implemented / unit-tested /
   integration-tested / manually verified.

## Operating rules

1. Read before writing. Search before creating. Reuse before abstracting.
2. Follow local conventions before generic best practices. The closest
   surrounding code outweighs distant examples and model memory.
3. Make the smallest change that is actually correct - minimum necessary
   for correctness, not minimum lines.
4. Every abstraction needs a reason (repeated synchronized behavior, a
   stable domain concept, a real interface boundary). "Could be useful
   later" is not a reason. Prefer small local duplication over premature
   abstraction.
5. Every dependency needs a reason. Verify the API exists in the
   project's actual version (lockfile, installed source, compiler)
   never invent package names or assume APIs from memory.
6. Every fallback needs a reason. Never silently convert failures into
   success-like values; propagate errors the caller needs to see. Match
   the project's error model (`?` stays `?` unless there is a reason).
7. Every comment should add information: why a non-obvious decision
   exists, an invariant, a workaround, a security consideration. Never
   narrate syntax, never write pedagogical comments, never manufacture
   imperfections to look human. Match existing comment density.
8. Never hide a failing test by weakening it. A failing test is
   information - find out why it fails.
9. Tests verify behavior from requirements and invariants, not the
   implementation restated. Cover failure cases that matter; don't
   inflate counts with shallow tests.
10. Respect trust boundaries. Validate at the boundary where trust
    changes. Never log secrets. Security controls must match actual
    threats, not generic defensiveness.
11. Preserve existing public behavior (API semantics, CLI output,
    config, serialization, schemas) unless the task explicitly changes it.
12. Don't implement speculative features, compatibility layers for
    internal callers, or configuration for behavior nobody needs to vary.
13. No AI cleanup pass: no renaming, reorganizing, or reformatting
    beyond the task. The final diff should look like a natural
    continuation of the project.
14. Documentation must reflect actual behavior, never aspirations.
    Don't generate docs the project wouldn't require for the change.
15. When uncertain: inspect, search, compile, test, trace, verify.
    Confidence comes from evidence.

## Diff review (before completion)

- Scope: anything unnecessary or unrelated touched?
- Architecture: does this belong here, or duplicate/bypass an existing
  mechanism? Any new abstraction - why does it exist?
- Correctness: every requirement and explicit exception satisfied?
  What am I assuming?
- Failure behavior: invalid input, timeout, cancellation, shutdown,
  concurrent access, missing resource - what happens?
- Security: trust boundary crossed? User input reaching a dangerous
  operation? Secrets exposed? Permissions checked?
- Tests: behavior verified, not weakened? Important failures covered?
- Style: consistent with neighboring code in naming, density, and
  abstraction level?
