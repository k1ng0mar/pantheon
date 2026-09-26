# Group A review (core, agent, providers, runtime) — Nyx hand review

Baseline: 298 tests, 15 evals, zero warnings. Line refs approximate to
current HEAD (e5179a5+).

## pantheon-core

**[low] events.rs — ModelDelta vs ToolOutput asymmetry**
ModelDelta is provider-plane only and never persisted as a full message, but
ToolOutput IS persisted. A contributor reading the enum cannot tell which
variants are ledger-canonical. A doc comment on the enum saying "variants
that project to transcript messages: AssistantMessage, ToolMessage" would
save real confusion. Fix: doc-only.

**[low] catalog.rs — provider lookup by exact string**
`provider(name)` does exact matching. A user typo ("openai-compat") silently
falls into "unknown provider" paths downstream with a generic error. Fix:
provider() could return a did-you-mean list of catalog names in the error.

**[low] capability.rs — Policy has no serde derive**
Policies are built in code only. Config presets map to constructors, fine for
now, but a config file cannot express a custom policy. This is a deliberate
scope cut, not a bug; document it in ARCHITECTURE.md under capability system.

Keep as-is: structured PantheonError (code/layer/retryable/cause/remediation/
evidence) is the right shape and consistently used; Event enum single-source
of truth holds; catalog metadata (cost, context limits) is clean.

## pantheon-agent

**[medium] engine.rs:131 — ModelRequested hardcodes model "default"**
The legacy loop emits `model: "default"` in ModelRequested because ModelTurn
has no model identity. The runtime path (session.rs) emits its own through
LedgerModelSink with the real chain, so no ledger corruption, but the legacy
path is a second, misleading event stream. Fix options: give ModelTurn a
`model_name()` method, or mark the legacy loop as test-only and move it out
of the lib path. Recommend the latter: session.rs is the canonical path, two
loops is standing confusion cost.

**[medium] engine.rs:158 — budget check happens before gate**
In the legacy loop, a tool call that would be denied still consumes budget
consideration order matters: the deny path returns Denied, fine, but a
run that alternates allowed/denied calls can exhaust budget on calls that
never executed. Philosophically, denials should not count against the tool
budget. The session.rs path counts only executed calls (fixed earlier), so
the legacy loop is inconsistent with the canonical one. Fold into the
"retire legacy loop" fix.

**[low] engine.rs — Delegate returns immediately after spawn result**
Delegated work returns `Delegated { agent }` and the loop stops. The
delegate's output lands in the transcript but the parent never continues from
it. This matches the spawner not being wired in v1, but the comment says
"denied" while the code returns Ok. Minor doc drift.

Keep as-is: Budget is runtime-owned (never agent-chosen); the gate emits
structured denials; loop events cover every transition.

## pantheon-providers

**[medium] chain.rs — fallback attempts emit Attempt but failure details go
only to the sink**
The chain retries on retryable errors with the next fallback, but the cause
of each fallback is not persisted as its own ledger event (only Attempt
metadata). After a run with two fallbacks, `pantheon runs <id>` shows which models were
tried but not WHY each earlier one failed. Fix: emit ModelFallback with the
error code (an event variant may already exist — verify) so the offline
`pantheon runs <id>` answers "why fallback?".

**[low] mock.rs — match semantics "*" only**
MockTransport matches "*" or exact strings. A fixture needing prefix matching
cannot be written. This limited the eval fixtures to exact-message scripts.
Fix: treat entries ending in "*" as prefix match, or document the limitation
in eval/README.

**[low] http.rs — no timeout on the blocking transport**
HttpTransport::default() has no explicit request timeout. A hanging provider
blocks the session thread indefinitely; the watchdog probes ledger status
(which still works) but the turn itself never completes. Fix: ureq agent
with a timeout (PANTHEON_HTTP_TIMEOUT_MS, default 120s).

Keep as-is: fallback logic living outside the agent loop is the right call;
normalized ModelEvents for both streaming and non-streaming; the chain is
the single fallback authority.

## pantheon-runtime

**[low] lib.rs — lease_id format includes a random run id**
`lease_{pid}_{new_run_id()}` — the run-id generator is reused for lease ids,
which works but couples two concerns. A dedicated counter would be clearer.
Cosmetic.

**[low] session.rs — watchdog probes ledger status as liveness**
The probe checks `ledger_status(run_id)` which can succeed even when the
provider is hanging (the ledger isn't involved in the model call). The probe
therefore verifies process liveness, not turn progress. Document this
limitation on TurnWatchdog: it kills wedged processes, not slow providers.
A provider-timeout fix (http.rs above) covers the actual hang case.

**[info] session.rs — two loops confirmed**
Canonical drive() loop and legacy AgentLoop::run both live in the build. The
legacy one is exercised by pantheon-agent's own tests only. Recommend:
keep as test harness for the engine crate, add a module doc saying exactly
that, and have session.rs stop referencing it beyond the adapter.

Keep as-is: durable operations state machine, lease CAS, deny-settle,
watchdog pause semantics, pipeline gate flow. All recently reviewed and
correct.
