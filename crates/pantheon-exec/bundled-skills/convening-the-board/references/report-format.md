# Final Report Format

Deliver the boardroom dialogue first, then this report. Keep the whole package readable: no raw tool output, every factual claim carries a source link.

## 1. Boardroom dialogue (first, verbatim style transcript)

- Chaired by the Chairperson; every persona speaks in its own named voice.
- Scouts present candidate ideas with evidence; Validators challenge; Realists deliver hard truths; the lab names the cheapest test, shows what runs, and reads the failure story.
- Include genuine disagreement and the resolution (or non-resolution) of each.
- Keep it lively but evidence anchored: when a persona cites something found in research, link it. Pull the link from `.board/EVIDENCE.md` so no claim in the dialogue is unsourced.

## 2. Final report

### Executive summary

The research question, and the board's answer in 3-5 sentences.

### Scoreboard

A compact table of all candidate ideas with:

- Validator scores (demand, necessity, scalability, differentiation, willingness to pay, audience adoptability)
- Validator verdict (Pursue / Pursue with changes / Kill)
- Adoption Auditor rating (Adoptable / Adoptable if / Not adoptable)
- Realist verdict (Workable / Workable if / Not realistic)
- Experiment verdict (Test first / Kill now / Ship test)
- Prototype verdict (Buildable / Buildable if / Not buildable)

### The finalists (1-3 ideas)

For each surviving idea:

- **The idea** in one sentence
- **Who it is for** (the specific person from the Customer Advocate)
- **Why now** (demand evidence, linked)
- **Competition and white space** (linked)
- **What it takes and the honest first 90 days** (condensed, linked where applicable)
- **Top risks**
- **Board's recommendation**

### The lab

For each finalist, one compact block:

- The riskiest assumption and the cheapest disconfirming test
- Pass threshold, kill threshold, timebox, cash cost
- What was actually built, with the real/mocked split, or a pointer to the build spec
- The pre-mortem in three lines, plus the earliest warning signs
- Kill criteria and who checks them

State plainly when the lab produced a spec instead of running software.

### The decision record

Copy the final blocks from `.board/DECISIONS.md`: decision, reason, kill criteria, review trigger. This is what makes the run reusable next time.

### The graveyard

Killed ideas with a one-line cause of death each, so the reasoning is reusable. Include lab kills, not just validator kills.

### Sources

Generated from `.board/EVIDENCE.md`, grouped by idea. One row per claim with its source and confidence. Do not retype links from memory.
