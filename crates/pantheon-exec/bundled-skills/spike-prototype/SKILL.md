---
name: spike-prototype
description: "Use to answer one technical question fast with throwaway code: can this library do X, which approach is viable, what breaks. Timeboxed, never merged as-is."
origin: bundled
---

# Spike prototype

A spike answers a question, not a ticket. "Can we stream this?", "which of
these three approaches survives contact with the real API?", "what is the
actual latency?" You build the smallest thing that produces the answer,
write the answer down, and then the code dies or gets rewritten properly.
Spikes that quietly become production code are how systems rot.

## Purpose

Resolve one technical unknown quickly with a timeboxed throwaway
experiment, and record the answer.

## Workflow

1. State the question in one sentence, plus what a "yes" and a "no" look
   like. If you cannot state the question, you are not ready to spike.
2. Set a timebox: 1-2 hours default, a day at most. When it expires, you
   stop and write up what you learned, even if the answer is "incomplete."
3. Build the smallest thing that answers the question. Hardcode, skip
   error handling, copy-paste. The spike owes nothing to quality; it owes
   everything to speed and honesty.
4. Write the answer: what you tried, what happened (with numbers where
   relevant), and the recommendation.
5. Dispose of the code explicitly: delete it, or promote it with a
   written plan for the rewrite it needs. "Keep it for now" is not a
   disposition.

## Output contract

- The question, the timebox, what was built (roughly).
- The answer: findings, numbers, recommendation.
- Disposition of the code: deleted or promoted-with-plan.

## Operating rules

1. One question per spike. Two questions is two spikes.
2. A spike is never merged as-is. If the approach works, the production
   version is a rewrite informed by the spike, not the spike with the
   TODOs removed.
3. Do not build scaffolding for the spike (configs, CI, docs). If the
   spike needs scaffolding, the question is too big.
4. Record negative results. "Approach B fails because the API rate-limits
   at 10 req/s" is the whole value of the spike.
5. Timebox is a hard stop, not a suggestion. An expired spike with a
   written partial answer beats a week-long "almost done" prototype.
