---
name: doc-to-action-items
description: "Read a document (contract, brief, email thread, spec) and extract obligations, deadlines, and tasks. Trigger when the user shares a document and asks what they need to do or what's due."
origin: bundled
---

# Document to action items

Contracts, briefs, and long email threads bury the same three things:
what must be done, by when, and by whom. This skill reads the document with
the filesystem/read tools and pulls those out. It is a workflow skill:
the reading is real, the judgment is the agent's.

## Workflow

1. Read the whole document. For PDFs use the `pdf` skill's extractor;
   for Word use `docx-extract`. Do not work from a summary someone else
   wrote.
2. Extract, in order of how binding they are:
  - **Obligations**: things the document says must happen ("shall",
     "must", "agrees to"). Quote the clause.
  - **Deadlines**: every date mentioned, what it attaches to, and
     whether it is a hard date or a target.
  - **Tasks for the user**: obligations where the user (or their side)
     is the actor.
3. For each item: cite the section or page it came from. An action item
   without a source line is a guess.

## Output contract

1. Obligations table: obligation | who | source (section/page) | binding
   language quoted.
2. Deadlines in chronological order: date | what | hard or target.
3. The user's tasks: what, by when, and what blocks it.
4. Explicitly list what the document does NOT say - the gaps the user
   should confirm (missing dates, unnamed owners, undefined terms).

## Operating rules

- This is not legal advice. Say that if the document is a contract and
  the stakes are real.
- Never soften binding language. "Shall deliver by Friday" is not
  "aims to deliver".
- If the document contradicts itself, quote both passages and flag the
  conflict instead of resolving it silently.
- Dates: state the year. "March 15" in a 2024 contract is not March 15,
  2026.
