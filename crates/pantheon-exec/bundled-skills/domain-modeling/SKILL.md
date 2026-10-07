---
name: domain-modeling
description: "Use when a project's terminology is being built or sharpened: challenge terms against GLOSSARY.md, propose precise canonical names, cross-reference claims against the code, and write ADRs for hard-to-reverse decisions."
origin: bundled
---

# Domain modeling

A project whose terms are fuzzy gets imprecise code, because the glossary
the developers carry in their heads is the schema they build against. This
skill is the discipline of writing the shared model down: challenge vague
language, name things precisely, and record the decisions so the next person
inherits the model, not the confusion.

Merely reading `GLOSSARY.md` for vocabulary is not this skill. That is a
line any skill can do. This one is for when you are changing the model.

## Layout

Single-context projects keep it simple:

```
/GLOSSARY.md
/docs/developer/decisions/<NNNN-topic>.md
```

Pantheon's ADR path is `docs/developer/decisions/` with zero-padded
four-digit numbers (see the existing `0001`, `0002`). Do not invent a new
ADR location for this skill; use the repo's.

A glossary gets created the first time a term is resolved, an ADR the first
time a hard-to-reverse decision happens. Do not pre-create the files.

## During the session

### Challenge against the glossary

When the user uses a term that conflicts with existing language in
`GLOSSARY.md`, call it out immediately. "Your glossary defines approval as
X, but you seem to mean Y. Which is it?" Silence here lets two people name
two things the same word.

### Sharpen fuzzy language

When the user uses vague or overloaded terms, propose a precise canonical
term. "You are saying account. Do you mean the customer or the user? Those
are different things."

### Cross-reference with code

When the user states how something works, check whether the code agrees. If
it contradicts, surface it: "Your code cancels entire runs, but you just
said partial cancellation is possible. Which is right?" Three fill states
exist (what the user said, what the code does, what the docs claim) and they
are checked against each other, not trusted in any order.

### Update inline

A resolved term gets written to `GLOSSARY.md` as soon as it is resolved, not
batched at the end. Batched glossary work is written from memory, and memory
is where the ambiguity came from.

A glossary defines terms. It is not a spec, a scratch pad, or a bin for
implementation decisions.

### Offer ADRs sparingly

An ADR gets offered when all three are true:

1. **Hard to reverse**: the cost of changing your mind later is meaningful.
2. **Surprising without context**: a future reader will wonder why this was
   chosen.
3. **Debated**: there was a real alternative, and the reasons survived one
   serious objection.

An ADR writes down the decision, the context, and the alternatives
considered. It does not write down the implementation. Decisions with no
date are suspicions, not decisions.

## Operating rules

- Invent edge-case scenarios that force precision about boundaries between
  concepts. The interesting terms are the ones that break under a concrete
  case.
- Never manufacture ambiguity to look thorough. If the term is already
  precise and the code agrees, move on.
- Grilling pairs naturally with this skill: the grilling finds the
  undecided branch, this skill records the decision. But the two are
  independent, and neither is required by the other.