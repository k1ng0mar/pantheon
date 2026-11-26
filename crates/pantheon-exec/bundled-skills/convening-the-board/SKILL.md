---
name: convening-the-board
origin: bundled
description: "Use when the user asks for researched ideas, opportunities, or decisions that warrant a full multi-agent investigation, such as \"which startup should I build\", \"what product should I make\", \"what niche has unmet demand\", or any open-ended question needing broad internet research followed by validation and feasibility analysis. Convenes a simulated R&D board of agent personas: Scouts fan out across the web, social media, and forums to gather evidence; Validators independently stress-test each candidate idea; Realists judge feasibility on the ground; a lab designs a failing test and builds a throwaway artifact. Delivers a boardroom dialogue, a final report, and a decision record."
---

# Convening the Board

Run a research board with a prototyping lab to answer open-ended opportunity questions using real internet evidence. The board investigates, disconfirms, and builds a throwaway artifact before it reports. Deliver a boardroom dialogue, a final report, and a decision record that a later run can pick up.

## Who is on the team

- Chairperson: orchestrates only. Owns the brief, the evidence ledger, and the decision record. Never researches and never argues a side.
- Scouts (5): find raw signals across the web.
- Validators (4 per idea): stress-test one idea each.
- Realists (3 per finalist): ground the idea in buildability and risk.
- Lab (3 per finalist): design a failing test, build a throwaway artifact, write the pre-mortem.

## Flags

- Default: full run, all 5 scouts, 3-6 ideas into validation, lab on for up to 3 finalists.
- `--quick`: 3 scouts (Community Miner, Gap Hunter, Market Analyst), max 3 ideas, no lab, no intake.
- `--ideas N`: cap the number of candidate ideas carried into validation.
- `--no-lab`: skip prototyping and pre-mortem.
- `--no-intake`: skip founder questions and use only the context already given.
- State the pass count before starting: 5 scouts plus N validators plus 3 per realist survivor plus 3 per lab finalist. Announce each phase as it starts and ends.

## Operating principles

- **Evidence first.** Every claim an agent makes must trace to something found via the harness's search/fetch tools. If the harness has no web search or fetch capability, say so and stop instead of inventing findings.
- **Never fabricate.** If a fact is needed and not found, mark it `[UNVERIFIED]` or leave `[ADD: ...]` rather than inventing it.
- **Blind independence.** Scouts do not see each other's findings before reporting. Validators do not see each other's verdicts, and Realists do not see each other's judgments, until the Chairperson aggregates. Nobody just goes along with the room.
- **Start wide, then narrow**: scouts open with short, broad queries, survey the landscape, then drill into specifics.
- **Compress**: each agent returns its top findings only, with source links, never raw dumps.
- **Separation of powers.** The Chairperson only orchestrates: it plans, delegates, aggregates, and moderates. It never researches, never argues a side, and never edits an agent's verdict.

## Anti-groupthink rules

- A persona receives only the user brief and its own assignment. It never sees another persona's output or the running score.
- Every verdict and rating carries one argument against its own conclusion. Send a verdict without one back for revision.
- Record at least one dissent per phase. If the room is unanimous, the Chairperson asks one role to argue the other side and logs the result, or states plainly why unanimity is credible here.
- The Chairperson does not summarize opinions until every pass is in.

## Working directory

Write the run's artifacts to `.board/` inside the working folder (create it; never touch other files):

- `.board/BRIEFS.md`: one delegation brief per agent (objective, sources, output format, boundaries)
- `.board/FOUNDER_BRIEF.md`: the constraints the user actually stated
- `.board/EVIDENCE.md`: claim, source, confidence, reporter (see `references/decision-record.md`)
- `.board/DECISIONS.md`: one block per candidate, appended as decisions are made
- `.board/scouts/<persona>.md`, `.board/validators/<idea-slug>.md`, `.board/realists/<idea-slug>.md`: each agent's raw returned findings
- `.board/lab/<idea-slug>/`: the experiment spec, prototype or build spec, and pre-mortem
- `BOARD.md`: the final deliverable (dialogue + report)

If the harness cannot write files, keep everything in conversation turns instead, clearly labeled.

Every file named in this skill (`.json` and `.md` roles and formats) lives under `references/` in this skill's directory. Read it from there when told to load one.

## Execution modes (pick the best the harness supports)

1. **Native subagents.** If the harness can spawn subagents (e.g. a Task/agent-spawning tool), the Chairperson delegates one subagent per scout in parallel, then one per idea validator, then the realists, then the lab. Enforce blind independence by not passing other agents' outputs into a subagent's brief.
2. **Sequential simulation fallback.** If no subagent mechanism exists, run each persona as a separate, clearly labeled pass in this conversation. Complete one persona's full search loop (search, read, refine, search again) before moving to the next. Never merge personas into one omniscient voice; the independence rule still applies.
3. Scale effort to the question. A full board run is for genuinely open, high-stakes queries; for narrower questions, run fewer personas and say which ones you skipped.

## Workflow

### Phase 0: Convene (Chairperson)

- Restate the user's question as a research objective.
- Read `.board/DECISIONS.md` if it exists. Do not re-litigate a kill unless its review trigger fired.
- Select scout personas from `references/personas.json` and write delegation briefs into `.board/BRIEFS.md`.

### Phase 0.5: Founder intake

- Ask at most five questions, only about facts that change the shortlist: budget, time to first revenue, skills already in hand, distribution channels reachable today, and what a win looks like.
- With `--no-intake`, or when the user already answered, write what you have and mark gaps `[UNKNOWN]`.
- Write `.board/FOUNDER_BRIEF.md`. Every later phase treats it as a hard constraint. No persona invents a constraint that is not in it.

### Phase 1: Scouts (idea generation)

- Load `references/personas.json` and dispatch each scout persona as an agent or search pass. Each scout performs many searches across its territory: blogs, general web search, Reddit, X/Twitter, Hacker News, Quora, Product Hunt, Indie Hackers, YouTube, niche forums, app store reviews, and public community mentions. Prefer the harness's dedicated connector tools (Reddit, X, social platforms) when present; otherwise use web search with site-scoped queries like `site:reddit.com`.
- Each scout returns: 3-5 candidate ideas/directions, the evidence for each (demand signals, complaints, gaps), and source links.
- After each scout reports, the Chairperson copies every sourced claim into `.board/EVIDENCE.md` as claim, source, confidence, reporter. No judgment yet.
- The Chairperson deduplicates, merges overlaps, and selects the strongest distinct candidates for validation (respecting `--ideas N`).

### Phase 2: Validators (stress-testing)

- Load `references/validators.md`. Assign one Validator per selected idea, blind to the other validators' work, including The Adoption Auditor's audience questions: does the audience exist in reachable numbers, and would they actually adopt (has this market ever paid for/adopted a comparable tool, or only complained)? Each validator must also resolve: is the demand real or anecdote, is it necessary, how big is it, how scalable, who competes, how crowded, would people pay?
- Each validator returns a verdict: PURSUE / PURSUE WITH CHANGES / KILL, with scores and the evidence that most influenced the verdict.
- The Chairperson writes each verdict into `.board/DECISIONS.md` as it lands.

### Phase 3: Realists (feasibility)

- Load `references/realists.md`. For ideas that survived validation, the realist bench asks: would this actually work? What would it take (skills, capital, time, distribution)? Who exactly is it for? What is the honest first 90 days?
- Realists run blind to each other too, and may kill an idea the validators approved. Their job is to ground everything.
- Cap the survivors at 3 before the lab.

### Phase 3.5: The lab (disconfirmation and build)

- Load `references/lab.md`. For each survivor, run the Experiment Designer, the Prototype Engineer, and the Premortem Lead, blind to one another.
- The experiment must have numeric pass and kill thresholds. The prototype must be runnable, or must say plainly that it is a spec. The pre-mortem must name the earliest warning signs.
- A lab role may kill a finalist. Record every lab verdict in `.board/DECISIONS.md` and in `.board/lab/<idea-slug>/`.

### Phase 4: The Boardroom (dialogue)

- Only after all verdicts are in does the Chairperson reveal them to the whole board and moderate the transcript: the scouts present, the validators challenge, the realists deliver hard truths, the experiment designer names the cheapest test, the prototype engineer shows what actually runs, the pre-mortem lead reads the failure story. Each speaks in its own voice and disagrees where it genuinely would.
- Reference real evidence found during research by name and link. Use `.board/EVIDENCE.md` so no claim in the dialogue is unsourced.

### Phase 5: Final report

- Load `references/report-format.md` and write the final deliverable to `BOARD.md`: dialogue, structured report, experiment specs, pre-mortems, and the decision record. Deliver it to the user.
- Point the user to `.board/` for the underlying artifacts.

## Output compactness

Never paste raw page contents, search dumps, or tool transcripts into the user-visible result. Condense findings to headline + evidence + link before showing them.
