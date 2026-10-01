---
name: sdlc-review
description: "Use when reviewing a Kanban handoff from a builder seat: verify against the spec and evidence, then approve, request changes, or escalate."
origin: bundled
---

# SDLC review

The reviewer's job is not to re-do the work. It is to answer one question:
does the handoff prove the work is done? Read the handoff, check it against
the spec and the evidence, then route it. Three outcomes exist, and "looks
fine" is not one of them.

## Workflow

1. **Read the handoff** (see the `kanban-handoff` skill for the format).
   Note what it claims: what was built, where, and how to verify it.
2. **Check the spec first.** Pull up the task or ticket the work claims to
   satisfy. Review against the spec, not against your impression of what
   would be nice.
3. **Verify the evidence.** Run the verification steps the handoff gives
   you, or spot-check them. A handoff that says "tests pass" without
   naming the tests gets the tests run by you.
4. **Route the outcome.** Exactly one of:
   - **Approve**: the spec is met and the evidence checks out. Say what
     you verified, in one line each.
   - **Request changes**: specific, actionable, and tied to the spec.
     "Fix the retry logic to cap at three attempts per the spec" is a
     change request. "This could be cleaner" is not.
   - **Escalate**: the spec is ambiguous, the work conflicts with other
     work, or the right call needs the user's judgment. Say what decision
     is needed and from whom. Do not escalate vagueness; escalate a named
     decision.
5. **Write the review record.** Outcome, what was checked, and what happens
   next. Short. The next person should know the verdict without reading
   the whole thread.

## Operating rules

- Review the handoff, not the person. No commentary on style, speed, or
  effort. The work either meets the spec or it does not.
- Do not expand scope in review. If the spec was wrong, that is a new
  task, not a change request on this one.
- A change request must be fixable without re-reading the spec. If your
  request needs three paragraphs of context, the spec was unclear and the
  outcome is escalate, not request-changes.
- Timebox it. A review that takes longer than the work is a sign the
  handoff was bad; request changes on the handoff itself.
- Never approve what you did not check. "The builder is usually right" is
  how bugs get a signature.
