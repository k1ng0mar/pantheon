---
name: grilling
description: "Use when a plan, design, or decision has unexamined assumptions: interview the user in rounds until nothing is left silently assumed, finding facts yourself rather than asking."
origin: bundled
---

# Grilling

A plan with an unexamined assumption is not a plan, it is a hope. This skill
finds the assumptions before they become expensive, by interviewing the user
until every branch of the design tree has been visited.

Do not act on the plan afterward until the user confirms you reached shared
understanding. Grilling that ends in immediate implementation was wasted
motion.

## The design tree

Every decision branches into the decisions that hang off it. Map the plan as
a tree and work it in rounds.

The **frontier** is every decision whose prerequisites are already settled:
the questions you can ask right now without guessing at answers you have not
heard yet. Ask the whole frontier in one round, numbered, each with your
recommended answer. Then wait for the answers before the next round.

A question whose answer depends on another question still open in this round
belongs to a later round, not this one.

Each round of answers reshapes the tree: settled decisions push the frontier
outward and unblock questions that depended on them. Recompute and ask the
next round. The session is done when the frontier is empty.

## Rounds look like this

```
Q1 - Storage: where does run state live?

Options: sqlite file under data_dir, or a separate process.

Recommend: sqlite under data_dir. One file to back up, no extra
process to supervise, and the dashboard already reads it.

---

Q2 - Retention: how long do runs persist?

Recommend: 30 days, then prune. The competitor log uses the same
window.
```

Number the questions. Give a recommendation with reasoning, not just a
letter. A question with no recommendation pushes the work back onto the
user, which is the thing this skill exists to prevent.

## Facts are your job

When a question needs a fact from the environment, go get it. Do not ask the
user for anything you could look up yourself: the config file, the schema,
the existing implementation, the test that already covers this.

Do not block on the lookup either. A running exploration is an unsettled
prerequisite, so only the questions downstream of it wait for the result.
Ask the rest of the frontier now.

Decisions are the user's. Facts are yours.

## What to press on

- The failure path. What happens when this fails halfway, and who cleans up?
- The boundary. What happens at the edge, not the happy path.
- The assumption behind the estimate. Where does the number come from?
- The thing nobody has decided yet. Every plan has one; it is usually phrased
  as a fact.
- Reversibility. If this is wrong in six months, what does undoing it cost?

## Operating rules

- Press on the substance, not the person. "This will not scale" is useful;
  "you did not think about this" is not.
- A wrong answer gets corrected, not argued with. The point is a shared
  model, not winning.
- Silence is not agreement. If the user waves off a branch, ask what it costs
  to leave undecided.
- Stop when the frontier is empty. More rounds than the decision needs is
  interrogation, not rigor.
- Record what was decided and why. A decision with no recorded reason gets
  relitigated in a month.