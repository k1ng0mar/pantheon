---
name: kanban-handoff
description: "Use when handing work from a builder seat to a checker seat: the handoff format, what it must contain, and how to write one."
origin: bundled
exec:
  - name: new-handoff
    description: "Create a handoff file from the standard template"
    command: scripts/new-handoff.sh
    args: "<task-slug>"
    runtime: shell
    side_effects: write
    timeout_secs: 30
---

# Kanban handoff

A handoff is the contract between the seat that built the work and the seat
that checks it. The checker should be able to verify everything without
asking the builder a single question. If the handoff raises a question the
builder could have answered in writing, the handoff failed.

## What a handoff must contain

1. **Task**: what was asked, in one paragraph, with a pointer to the spec
   or ticket. Not what you think was asked; what was written.
2. **What changed**: the files, commits, or artifacts. Paths, not
   descriptions. "Updated the retry logic" is a description;
   `crates/pantheon-exec/src/retry.rs` is a path.
3. **How to verify**: the exact steps. Commands to run, pages to open,
   behaviors to check. The checker runs these; if a step needs setup the
   builder did by hand, the handoff is incomplete.
4. **Spec compliance**: which parts of the spec each change satisfies. One
   line per requirement is fine.
5. **Known gaps**: what is unfinished, untested, or deliberately out of
   scope. Surprises found in review are not known gaps; they are missing
   gaps.
6. **Do not re-check**: what the checker can skip because it was already
   verified, with the evidence. This is a courtesy, not a shield: the
   checker may still check it.

## Workflow

1. Finish the work first. A handoff written mid-task is a status update
   wearing a handoff's clothes.
2. Generate the template with the skill's `new-handoff` helper and fill in
   every section. An empty section is a lie by omission; write "none" if
   there is genuinely nothing.
3. Re-read it as the checker: can you verify every claim without talking
   to the builder? If not, fix the handoff before sending it.
4. Send it through the channel the board uses. The handoff lives with the
   task, not in a chat thread that scrolls away.

## Operating rules

- Write it the day the work finishes. A handoff written from memory a week
  later is fiction.
- Evidence over adjectives. "Thoroughly tested" means nothing; "cargo
  test -p pantheon-exec, 214 passed" means something.
- One handoff per task. Bundling three tasks into one handoff guarantees
  the checker approves two and waves through the third.
- If the work diverged from the spec, say so in the handoff and say why.
  Divergence found in review is a trust problem; divergence declared in
  the handoff is a decision the checker can evaluate.
