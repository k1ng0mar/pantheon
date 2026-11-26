---
name: convening-marketing
origin: bundled
description: "Use when the user wants a marketing plan, marketing strategy, go-to-market plan, competitive analysis, consumer/audience research, positioning, messaging, pricing, or channel strategy for a product, service, or business. Convenes a simulated full marketing department of agent personas: Intelligence researchers study the consumer base, competition, and market; Strategists build STP, positioning, messaging, and pricing; Execution and Measurement build the channel plan, 90-day calendar, and KPIs; the Studio writes the actual marketing copy. Delivers a department round-table plus a complete marketing plan."
---

# Convening the Marketing Department

Run a full simulated marketing department to produce an evidence-based marketing plan: consumer base study, competitive analysis, positioning and messaging, channel strategy, a 90-day plan, and KPIs.

## Prerequisite: know the product

If the user's product/service/company is not clearly described in the request or conversation, the CMO must ask the user for it (what it is, who it serves, price if any, stage) before convening. Never invent a product. If only partial details exist, research public information about the company and fill gaps with `[ADD: ...]` markers.

## Flags

- Default: full run, all 4 Intelligence agents, full strategy, execution, and studio suites.
- `--quick`: 3 Intelligence agents (Consumer Researcher, Competitive Analyst, Market Analyst), condensed strategy, top 3 channels, reduced copy pack.
- `--focus X`: restrict the run to one track (e.g. `--focus positioning`, `--focus channels`, `--focus pricing`, `--focus consumer`, `--focus copy`); the CMO states which tiers are skipped.
- State the sub-agent call count for the chosen configuration before starting.

## Operating principles

- **Evidence first.** Claims about consumers, competitors, and markets must trace to the harness's search/fetch tools. If no web access exists, say so and produce only a framework-level plan clearly marked as such.
- **Never fabricate.** Unknown facts get `[UNVERIFIED]` or `[ADD: ...]`.
- **Count the audience.** Consumer research must estimate the realistic reachable size of each segment and cite adoption/purchase behavior (what the audience does), not just what they say. Borrowed from the R&D board's hardest lesson.
- **Blind independence.** Intelligence agents do not see each other's findings before reporting; Strategists receive only the aggregated intelligence brief, not each other's drafts; Execution and Studio work only from the approved strategy. Nobody just goes along with the room.
- **Defensible, not vibes.** Positioning must name real alternatives and real customer pain points found in research; messaging must be anchored to evidence.
- **Separation of powers.** The CMO only orchestrates: briefs, aggregates, mediates, and decides. It never researches and never rewrites an agent's findings.

## Working directory

Write run artifacts to `.marketing/` in the working folder (create it; never touch other files):

- `.marketing/BRIEFS.md`: delegation briefs (objective, sources, output format, boundaries)
- `.marketing/intelligence/<agent>.md`, `.marketing/strategy/<agent>.md`, `.marketing/execution/<agent>.md`, `.marketing/studio/<agent>.md`: raw agent findings
- `MARKETING_PLAN.md`: the final deliverable

If the harness cannot write files, keep everything in clearly labeled conversation turns.

Every file named in this skill lives under `references/` in this skill's directory. Read it from there when told to load one.

## Execution modes (pick the best the harness supports)

1. **Native subagents.** If the harness can spawn subagents, the CMO delegates each agent as a parallel subagent per tier, enforcing blind independence by never passing one agent's output into another's brief.
2. **Sequential simulation fallback.** Otherwise run each persona as a separate labeled pass, completing its full search loop before the next. Never merge personas into one voice.
3. Scale effort: a full run suits a real product/company; for a solo, low-budget, or local business, propose a leaner formation and say what was cut.

## Workflow

### Phase 0: Brief (CMO)

- Restate: product, market, stage, budget signals, and the user's goal (launch, grow, reposition, analyze competition...).
- Choose agents from `references/personas.json`, assign territories, write briefs. Identify which data must come from the user (sales, current customers) and ask for it once, up front, rather than inventing it.

### Phase 1: Intelligence (research)

- Load `references/personas.json`. Dispatch Intelligence: Consumer Researcher (who buys, segments, size counted from evidence, behavior and JTBD), Competitive Analyst (teardowns: pricing, messaging, channels, weaknesses), Market Analyst (trends, seasonality, category dynamics), and Product Expert (features vs. category, USPs, gaps).
- Each must actually search: reviews, forums, social, competitor sites, pricing pages, category reports; prefer dedicated connector tools where available.
- Each returns a compact brief with source links. The CMO aggregates into an intelligence brief: it deduplicates, flags contradictions, and states what is still unknown.

### Phase 2: Strategy

- Load `references/frameworks.md`. Strategists work from the aggregated intelligence brief only: STP (segments scored by size/reachability/value, one primary target), positioning (following the framework in `references/frameworks.md`), messaging per segment, pricing direction, and channel selection ranked by fit and estimated cost.

### Phase 3: Execution and Measurement

- Load `references/personas.json` execution tier. Build the 90-day plan from the approved strategy only: channel-by-channel tactics, content angles tied to audience evidence, a launch/always-on calendar, and the KPI system from `references/frameworks.md` with realistic starting targets.

### Phase 3.5: The Studio (copywriting)

- Load `references/personas.json` studio tier. Writers produce the actual copy, from the approved messaging matrix only: tagline finalists, ad variants (per the chosen channels), social post sequences, email sequences (launch + nurture), landing page hero + section copy, and content angles/headlines for long-form. Each piece of copy must be traceable to a message pillar and, where possible, echo real customer language found in research. The Editor selects, cuts, and demands rewrites; only Editor-approved copy enters the plan.
- Scope control: write samples for the primary segment and top channels only (a full copy deck for every channel of a 90-day plan is a later production job, not a planning deliverable).

### Phase 4: The Round Table (dialogue)

- The CMO reveals all work and moderates a department round-table: Intelligence presents what the market actually looks like, Strategy defends choices, Execution reports constraints, the Editor reports what was cut, and genuine disagreements are argued to a decision. Reference evidence by name and link.

### Phase 5: The Marketing Plan

- Load `references/plan-format.md` and write `MARKETING_PLAN.md`: round-table dialogue first, then the structured plan. Deliver both.

## Output compactness

No raw page contents, search dumps, or tool transcripts in user-visible output. Condense to headline + evidence + link.
