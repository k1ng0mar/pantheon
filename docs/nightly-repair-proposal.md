# Nightly repair-loop design flaw - PROPOSAL ONLY

**Status:** Draft. No code changed. This needs Umar's eyes before anything changes.

## The flaw

`validate_with_fix_loop` in `crates/pantheon-nightly/src/fixloop.rs` treats an
eval rejection as a *claim* problem, not a *draft* problem. When the eval gate
rejects a skill/persona proposal, the "repair" is `try_prune_failing_tag`
(fixloop.rs:235-241): it deletes the failing eval's tag from
`proposal.eval_tags` and re-runs the gate. The draft body - the actual thing
that broke the eval - is never touched.

Two facts make this worse than a no-op:

1. **Vacuous pass.** `gate()` (crates/pantheon-nightly/src/gate.rs:115-119)
   returns `EvalVerdict::Pass("no evals tagged")` when the tag list is empty.
   Prune every failing tag and the broken draft passes with *no* evals having
   run against it. A proposal can be `Validated` and queued for approval while
   every eval that tested it failed.

2. **The narrowed claim is a fiction.** Pruning pretends the draft's relevance
   to the failing eval was mislabeled. In the common case the relevance was
   correctly labeled and the draft is genuinely bad - the eval caught exactly
   what it was supposed to catch. The repair path converts a real failure into
   a shrunken claim and calls it fixed. The audit log even narrates this as
   normal: `"pruned failing eval tag '{pruned}' (narrowed claim); retrying
   gate"` (fixloop.rs:137-142).

Contrast with the replay side of the same loop: `try_llm_sharpen`
(fixloop.rs:247-269) *revises the draft body* and re-runs both gates. The eval
side has no equivalent - its only move is to move the goalposts.

## Concrete scenario

1. Nightly proposes a skill draft tagged `[persona-tone, tool-policy]`.
2. `tool-policy` eval fails: the draft grants a tool it shouldn't.
3. Fix loop prunes `tool-policy` from the tags (attempt 1), re-runs gate on
   `[persona-tone]`, which passes.
4. Proposal is `Validated`, queued for approval, carrying the tool-policy
   violation. The approval queue shows a green validation history.

## Proposed fix design

Replace tag-pruning with **draft repair** for eval rejections, mirroring the
replay path:

- **Remove `try_prune_failing_tag`** as a repair step. Eval tags are the
  proposal's claimed relevance; they should be immutable once the proposal
  enters the loop. If the relevance was truly mislabeled, that's a proposer
  bug to fix at proposal time, not a loop-time repair.
- **On eval reject, repair the draft, not the tags.** Use the same Reflection
  slot used by `try_llm_sharpen`: feed the eval failure detail into
  `refine_proposal` and re-run the *full* eval tag set against the revised
  draft. Eval detail comes from `EvalRunner::run_eval`'s `EvalOutcome::Fail`
  string, which `gate()` already formats into the reject reason
  (gate.rs:126-129).
- **Keep the bound.** Each revision consumes one of the `max_fix_attempts`
  (default 3, fixloop.rs:31). Exhaustion escalates to `NeedsAttention` in
  `nightly-escalated.json`, unchanged.
- **Close the vacuous-pass hole.** If a proposal ever reaches `gate()` with
  zero eval tags (e.g. a kind that legitimately skips eval-gating), that
  should be an explicit, audited decision at the caller - not a silent `Pass`
  inside `gate()`. Minimal option: return a distinct verdict
  (`EvalVerdict::Skipped`/reason "no evals tagged") and have the fix loop
  treat it as *not validated* unless the proposal kind is allowlisted to skip.
  The current comment ("a vacuous pass is safer than a block", gate.rs:118)
  is wrong in this context: for a proposal destined for approval, an
  untested pass is the dangerous direction.
- **Audit shape stays the same.** `FixAttempt { phase: "eval", detail: "sharpened
  draft via Reflection slot; re-running gate" }` reuses the existing event
  type, so downstream consumers (report.rs, escalation records) need no
  changes.

## Risks of unattended use as-is

1. **Bad proposals auto-validate.** Any draft that fails every eval but keeps
   passing a shrinking tag set lands in the approval queue looking validated.
   Unattended approval (or a tired operator clicking through) merges broken
   skills/personas.
2. **Eval suite silently loses coverage.** Each prune shrinks the set of evals
   future similar proposals must pass - the loop learns to dodge evals rather
   than satisfy them. Over many nights the effective eval bar decays even
   though `eval/` still contains the tests.
3. **Audit misleads.** The `FixAttempt` event frames pruning as a successful
   repair ("narrowed claim"), so a post-hoc review sees "fixed and validated"
   rather than "failed an eval and the eval was dropped."
4. **Escalation never fires for this failure mode.** `Escalated` only happens
   when pruning can't name a tag (fixloop.rs:144-157) - i.e. only when the
   reject reason doesn't parse, not when the draft is broken. The safety valve
   doesn't cover the main hazard.

## Open questions for Umar

- Should eval-tag relevance ever be *renegotiable* in the loop (e.g. the LLM
  judge explicitly re-labels tags as part of sharpening), or is immutability
  the right invariant?
- Is `gate()`'s vacuous pass load-bearing for memory-lesson proposals
  (gate.rs:116-118)? If so, the fix needs a kind-aware skip, not a blanket
  removal.

---

# Addendum: nightly repair of broken MCP servers, scheduled tasks, and tools

**Status:** Design accepted by Umar 2026-09-29 (scope expansion of the nightly
repair work). Implementation: workstream 3, uncommitted.

## Mandate

Extend the nightly loop to detect and repair three new target classes
broken MCP servers, broken scheduled tasks, broken tools. Same safety
contract as the proposal fix loop: bounded repair attempts, then
disable-with-escalation. Never silent, never a retry loop, never repaired
into a worse state.

Two cross-cutting directives from Umar:

1. **Repair diagnosis uses the new `repair` aux model, not the Reflection
   slot.** `AuxiliaryKind::Repair` + `[repair]` config section (env prefix
   `REPAIR`, vault key `PANTHEON_REPAIR_API_KEY`) is owned by workstream 2.
   The repair loop resolves it the same way the fix loop resolves slots
   (`resolve_repair`, mirroring `resolve_refiner`). When no repair model is
   configured, diagnosis degrades gracefully: deterministic repairs still
   run, only the LLM diagnosis step is skipped - the pass never fails for
   an unconfigured slot.
2. **No second enabled flag.** Nightly is OFF by default (`[nightly]
   enabled`, workstream 1). The repair phase runs only as part of an
   enabled pass - it hooks into the same check by living inside `run_pass`.

## What "broken" means

- **MCP server:** `failures >= mcp_max_failures` (default 5) consecutive
  failures with status `failed`/`backoff` (the manager already keeps a
  consecutive-failure counter, reset on success). `Disabled` is operator
  intent and `Unapproved` is waiting on a human - neither is ever a repair
  target.
- **Scheduled job:** `consecutive_failures >= schedule_max_failures`
  (default 3) from the new `RunHistory` (pantheon-scheduler; no durable
  per-job run history existed - the gateway tick loop dropped completion
  receivers), and the job is not paused. Separately config-shaped: a cron
  expression that fails `CronSchedule::validate` - such a job silently
  never fires (`due()` returns false), which is breakage, not quiescence.
  (A missing template is *not* breakage: `resolve_task` falls back to the
  stored task snapshot by design.)
- **Tool:** over the pass's ledger scan window, `calls >= tool_min_calls`
  (default 3) and `errors == calls` - every invocation errored, errors
  attributed via failed turns exactly like the existing `RepeatedFailure`
  signal - OR the nightly smoke probe fails. Probing is opt-in per tool
  (`tool_probe_allowlist`, default empty): the pass never executes an
  arbitrary tool unprompted, and probes are empty-args invocations behind
  the capability policy.

## Repair ladders (all bounded, all audited)

- **MCP:** retry the connection (the flapping-server second chance) →
  repair-model diagnosis (advisory) → re-resolve config (env vars and
  paths may have changed since configure) + retry → **disable** the
  server and escalate. Disabling stops reconnect attempts and keeps the
  spec; the escalation tells the operator to mirror the disable in config.
- **Scheduled job:** read the failure. Config-shaped (bad cron) →
  deterministic normalization (6-field with zero seconds → 5-field,
  `@daily`/`@hourly`/`@weekly`/`@monthly`/`@yearly` macros, Quartz `?` →
  `*`); if the normalized expression validates, apply it, else **pause**
  and escalate. Non-config run failures → **pause** + escalate directly
  a failing cron spamming errors nightly is worse than a paused one. No
  retry-the-job ladder: re-firing a failing job is not the nightly's job.
- **Tool:** re-resolve the backing config (plugin path/manifest, MCP
  server env/paths) + probe again → **disable** + escalate.

Every attempt is audited with the existing `NightlyEvent::FixAttempt`
shape (`phase: "mcp"` / `"schedule"` / `"tool"`); escalations reuse
`NightlyEvent::Escalated` and `nightly-escalated.json` with `kind`
`"mcp-server"` / `"schedule"` / `"tool"`. The repair-model diagnosis is
advisory text recorded in the audit event and the escalation reason
repair *actions* stay deterministic.

## Architecture

- New `crates/pantheon-nightly/src/repair_targets.rs`: snapshots, three
  adapter traits (`McpRepairTarget`, `ScheduleRepairTarget`,
  `ToolRepairTarget`), the `RepairTargets` bundle, and
  `run_repair_phase`. The pass never touches `McpManager`/`TickDriver`/
  `ToolRegistry` directly - hosts implement the traits (production
  adapters are host wiring; eval tests use fakes). This keeps
  pantheon-nightly's dependency set unchanged.
- `NightlyDeps` gains `repair: Option<RepairTargets>` (`None` = phase
  skipped); `run_pass` takes `&mut NightlyDeps` so the phase can drive
  the adapters. `NightlyConfig` gains the four knobs above.
  `PassResult` gains `repairs: Vec<RepairReport>`; the markdown report
  gains a Repairs section.
- `NightlyLlm` (pantheon-api) gains a defaulted `diagnose_repair` method
- backward compatible, existing implementors (pantheon-providers)
  degrade to deterministic-only repair until implemented.
- `pantheon-scheduler::RunHistory`: durable per-job outcome log
  (`<data_dir>/schedule-run-history.json`), consecutive-failure counting
  (`Completed` resets; `TimedOut`/`Panicked` increment; `Replaced` is
  neutral). The gateway `SchedulerLoop` gains an opt-in outcome sink
  (default off, zero behavior change) so completions can be recorded.
- `ToolRegistry::remove` (pantheon-tools): the host-side primitive for
  tool disable.
- Boundedness: at most two mutating attempts per target (retry +
  re-resolve) before contain; targets processed in sorted order;
  adapters must bound each call (suggested ≤ 30s) - a repair phase that
  hangs the pass is a bug.

## What this addendum deliberately leaves out

- Attaching the adapter traits to a live host. The only `run_pass`
  host today is the TUI CLI (`pantheon-tui/src/nightly_cli.rs`), which
  has no live manager handles, so it passes `repair: None`. When the
  gateway (or another long-running host) runs the nightly pass, it
  must supply `RepairTargets` built from its own state:
- **MCP**: snapshot from `McpManager` health (`status` via
    `ServerStatus::as_str`, consecutive `failures`, `last_error`);
    `retry_connect` re-runs the manager's connect path;
    `re_resolve` re-reads the server spec (env/paths) then reconnects;
    `disable` marks the server `Disabled` and persists the spec so the
    next boot keeps it disabled. `Disabled` and `Unapproved` servers
    are never snapshots the pass acts on.
- **Schedule**: snapshot from `schedule.json` joined with
    `RunHistory::stats` (`consecutive_failures`); `repair_cron`
    writes the normalized expression to `schedule.json` and resets
    the job's history; `pause` sets the job paused in `schedule.json`.
    The gateway records outcomes via `SchedulerLoop::set_outcome_sink`
    feeding `RunHistory::record`.
- **Tools**: stats come from the pass's own scan (no host work);
    `re_resolve` reloads the plugin/tool manifest; `disable` calls
    `ToolRegistry::remove` and persists the disable so the next
    registry build skips the tool. Smoke `probe` is only ever called
    for names in `NightlyConfig::tool_probe_allowlist` (empty by
    default - probing an arbitrary tool executes it).
- Retrying failed job runs from the nightly pass - out of scope; the
  pass contains, it does not re-fire.
- LLM-suggested config edits applied automatically - the repair model
  diagnoses, it does not write config; all mutations are deterministic.
