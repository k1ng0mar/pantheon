# Pantheon TUI ↔ Runtime Integration Audit

**Date:** 2026-11-08
**Scope:** `crates/pantheon-tui` (61,245 LoC) against `crates/pantheon-runtime`,
`crates/pantheon-api`, `crates/pantheon-storage`, `crates/pantheon-providers`
**Method:** implementation reading + empirical probes (temporary tests, since removed)
**Working tree:** clean at time of audit (`c35d376`)
**Baseline:** `cargo test -p pantheon-tui` → 312 passed, 0 failed

No product code was modified. Probes used for evidence were deleted; the tree is
byte-identical to `c35d376`.

---

## 0. Evidence key

| Tag | Meaning |
| --- | --- |
| **[CONFIRMED]** | Reproduced by running code (probe test output quoted) |
| **[CODE]** | Established by reading the implementation; no runtime repro |
| **[SUSPECTED]** | Consistent with the code but depends on conditions I could not stage |
| **[IMPROVEMENT]** | Not a defect; a design/UX recommendation |

---

## 1. Architecture map

```
terminal.rs (argv dispatch)
   └─ session::run_tui_session_with()          session.rs:6881
        ├─ Session::new(...)                   pantheon-runtime
        ├─ session.on_event  ──┐               ModelEvent stream (per-worker-thread)
        ├─ supervisor.register_observer ─┐     runtime Event stream (all runs)
        │                                ▼
        │                    mpsc::channel::<TuiEvent>()   session.rs:6805
        │                                ▼
        └─ tui_loop()  ── try_recv drain ─► TuiState::handle_model_event   session.rs:1585
                                          ► TuiState::handle_runtime_event session.rs:2386
                                          ► TuiState::handle_background_event session.rs:2298
```

**Key routing invariant** (`session.rs:2000` `event_run_id`): every `Event`
carries a `run_id`; anything whose `run_id != state.session_id` is diverted to
`handle_background_event`, which may only touch *that* run's tab dot, parked
decision maps, or `/btw` step list. Model events are routed by a
**thread-local** `STREAM_RUN` stamp (`session.rs:6800`), set once at the top of
each worker thread, and read inside the `on_event` closure.

That design is sound and is the main reason background tabs cannot corrupt the
visible transcript. It is also the origin of Finding **F-01** below, because the
same `Session` object backs every worker thread.

### 1.1 Feature inventory (TUI surface → runtime capability)

| Surface | Entry point | Runtime capability | Status |
| --- | --- | --- | --- |
| Slash commands (75 registered) | `COMMANDS` `session.rs:56` | dispatch in `handle_slash_inner` `session.rs:11311` | See §3 |
| Command palette | `state.palette`, `filtered()` `session.rs:207` | reads `COMMANDS` | OK |
| Streaming text / reasoning | `ModelEvent::TextDelta` / `ReasoningDelta` | provider chain | OK |
| Provider fallback / retry cards | `handle_model_event` `session.rs:1585` | `ModelEvent::{Attempt,AttemptFailed,RetryAttempt,Fallback,Exhausted}` | OK |
| Interrupt (double Esc) | `press_esc` `session.rs:2807` | `cancel_current_run` `runtime/session.rs:4155` | **F-01** |
| Steering `/steer` | `do_steer` `session.rs:10943` | `steer`/`drain_steers` `runtime/session.rs:4187` | OK (with F-11) |
| Approval cards | `pending_approval`, `settle_approval_card` `session.rs:1111` | `Supervisor::grant/deny` `runtime/lib.rs:814,880` | OK |
| Permission modes | `cycle_permission_mode` `session.rs:2246` | `Session::set_permission_mode` | OK |
| Plan/Build mode | `toggle_plan_mode` `session.rs:2202` | `Session::set_mode` | OK |
| Clarify (`ask_user`) | `pending_clarify`, `answer_clarify` `session.rs:8263` | `Supervisor::answer_input` | OK |
| UI protocol docs | `BlockKind::Ui`, `degrade_for` at draw `session.rs:6442` | `ui::degrade_for` + `render_ui_node` | OK |
| UI approval park | `decide_ui_approval` `session.rs:8582` | `Session::submit_ui_action` | OK |
| Todo list | `refresh_todos` `session.rs:3439` | `Session::todo_list` | OK |
| Subagents / swarm | `swarm_spawn`/`swarm_complete` `session.rs:467,503` | `Event::{AgentSpawned,AgentCompleted}` | OK |
| Background tasks `/btw` | `spawn_bg_task` `session.rs:7290` | `chat_turn_with_cancel` (own token) | OK |
| Sessions / tabs / fork / rewind / checkpoint | `switch_to_run` `session.rs:3632` | ledger replay, `fork_run`, `TurnRewound` | OK (see F-12) |
| Model browser `/models` | `apply_model_switch` `session.rs:10602` | `Session::switch_model` `runtime/session.rs:3185` | OK |
| Compression `/compress` | `session.rs:12508` | `Session::compress_now` `runtime/session.rs:3352` | OK |
| Initial prompt (`-r ID "msg"`) | `session.rs:7159` | `start_turn` | OK |
| Notifications (turn done) | `notify::emit_turn_notification` | — | OK |
| MCP health segment | `refresh_mcp_segment` `session.rs:5510` | `mcp_manager.health()` | OK |
| Tool registry `/tools reload` | `session.rs:12779` | `Session::build_tool_registry` | OK |
| **Computer use** | — | `Session::set_computer_config` `runtime/session.rs:3569` | **F-02: not wired** |
| **`/edit`, `/continue`, `/pin`, `/clear-context`** | `session.rs:10984–11047` | — | **F-03: unreachable** |
| **Compression progress** | — | `Event::ContextTrimmed` / `ContextCompressed` | **F-04: never rendered** |
| **`/status` diagnostics for computer-use** | `session.rs:11917` | `Session::tool_diagnostics` | **F-05: always empty** |

---

## 2. Confirmed defects

### F-01 — CRITICAL: an interrupt permanently poisons the run's next turn

**Severity:** Critical (data-loss class: the user cannot continue the session)
**Status:** **[CONFIRMED]** — reproduced with a probe test

**Paths:**
- `crates/pantheon-runtime/src/session.rs:4155` `cancel_current_run`
- `crates/pantheon-runtime/src/session.rs:4320` `cancel_reason`
- `crates/pantheon-runtime/src/lib.rs:1223` `cancel_run_intent`
- `crates/pantheon-storage/src/core_group.rs:1590` `reopen_run`
- `crates/pantheon-tui/src/session.rs:7261` `start_turn` → `session.reset_cancel()`

**Mechanism.** `cancel_current_run` sets cancellation on **two independent
channels**:

1. the in-process `AtomicBool` token (`self.cancel.store(true, …)`), and
2. a durable per-run row flag via `cancel_run_intent` → `ledger.set_cancel_intent(run, true)`.

The drive loop checks **both** at every turn boundary:

```rust
fn cancel_reason(&self, run_id: &str, cancel: Option<&AtomicBool>) -> Option<&'static str> {
    if cancel.is_some_and(|c| c.load(SeqCst)) { return Some("interrupted by user"); }
    if self.supervisor.cancel_intent(run_id).unwrap_or(false) { return Some("cancel requested"); }
    None
}
```

The TUI's `start_turn` clears **only channel 1**:

```rust
session.reset_cancel();     // clears the AtomicBool, not the ledger flag
```

The ledger flag is documented as *"cleared by `reopen_run`"* — and
**`reopen_run` only clears it when the row status is `completed`, `failed`, or
`canceled`** (`core_group.rs:1597`, `WHERE … status IN ('completed','failed','canceled')`).

But `cancel_current_run` does not set the status; it appends `RunCanceled` and
sets `cancel_intent=1`. Therefore, for any run that is still `running`:

- the flag stays `1`,
- `reopen_run` refuses to clear it (status not terminal),
- **every subsequent turn on that run aborts at its first boundary with
  "cancel requested".**

**Reproduction** (probe, run against `Session::new` + `start_run`):

```
leftover cancel_intent after interrupt = Ok(true)
assertion `left == right` failed: after reset_cancel the next turn must not start canceled
  left: Some("cancel requested")
  right: None
```

**User-visible impact.** Double-Esc while a turn runs is the documented,
advertised interrupt (`KEY_HINTS`, `session.rs:145`). After using it once, that
session answers *"interrupted"* to every further prompt until the user opens a
new tab, and there is no UI affordance that explains why. Chronic-hit-rate bug:
interrupt is a first-class keybinding.

**Why the tests miss it.** `session::slash_dispatch_tests::undo_while_running_says_wait_not_no_finished_turn`
and `runtime_session_cancel.rs` exercise `cancel_reason` with a foreign
supervisor, and the manual interrupt path ends the process. No test performs
*interrupt → clear-token → new turn*.

**Recommended fix (pick one, in order of preference):**

1. **Best:** make `reset_cancel` also clear the durable flag —
   `Supervisor::clear_cancel_intent(run_id)` calling
   `ledger.set_cancel_intent(run_id, false)`, or
2. have `reopen_run` clear `cancel_intent` for a *running* row too (it is the
   documented "the run is continued" hook; today its `WHERE` clause silently
   excludes the only case that matters), **and**
3. add a regression test: *start run → `cancel_current_run` → `reset_cancel` →
   `assert_eq!(cancel_reason(run, None), None)`*.

Note (2) alone is not sufficient if any code path starts a turn without calling
`reopen_run`; (1) is the tight fix because it sits next to the flag's setter.

---

### F-02 — HIGH: `[computer_use]` config is never applied to the session

**Severity:** High (configured capability silently absent; misleading startup)
**Status:** **[CODE]** — verified by full call-graph search

**Paths:**
- `crates/pantheon-runtime/src/session.rs:3569` `set_computer_config` (the only setter)
- `crates/pantheon-runtime/src/session.rs:5230` (the only consumer, in `build_tool_registry`)
- `crates/pantheon-api/src/config.rs:1072` `ComputerUseSection`
- `crates/pantheon-tui/src/session.rs:6995` — where browser/websearch/stt *are* wired

**Mechanism.** `run_tui_session_with` configures every tool surface from
`config.toml`:

```rust
session.set_browser_config(…);       // [browser]
session.set_websearch_config(…);     // [websearch]
session.set_stt_section(…);          // [stt]
session.set_mcp_config(…);           // [mcp]
```

There is **no** `set_computer_config` call. A repo-wide search returns only:

- the definition,
- the registry consumer,
- `config_group.rs:8167` (the `pantheon mcp` *CLI* verb, which independently
  re-derives the spec from config), and
- two tests in `eval/tests/runtime_tool_gating.rs`.

Consequence: the session keeps `ComputerToolConfig::default()`
(`enabled: true, driver: "cua-driver", binary: None`) and **ignores the operator's
`[computer_use].driver` and `[computer_use].binary` entirely.**

This produces two concrete failures:

1. **`[computer_use] binary = "/opt/tapp-mcp"` is ignored.** The runtime resolves
   `cua-driver` from `PATH` instead, fails to find it, and registers nothing —
   while `pantheon mcp` (the CLI verb) reports the tapp driver as configured.
   Two code paths, two different answers, exactly what the config comment at
   `config.rs:1074` promises will not happen ("`/status` reads it").
2. **`[tools] computer_use = true` is silently inert** for users who set a
   driver other than `cua-driver`.

**Recommended fix.** Add, next to the other `set_*_config` calls in
`run_tui_session_with`:

```rust
session.set_computer_config(config::resolve_computer_section(
    file_cfg.as_ref().and_then(|c| c.computer_use.as_ref()),
));
```

with a shared `resolve_computer_section` used by both the TUI and the gateway, so
the CLI verb and the runtime cannot drift again. Add an eval test asserting that
`[computer_use] binary = "<tmp>"` reaches `tool_diagnostics` after
`build_tool_registry`.

---

### F-03 — HIGH: four registered commands can never succeed

**Severity:** High (dead controls: the UI advertises work it cannot do)
**Status:** **[CONFIRMED]** — `active_run` is `None` in every state those
commands accept

**Paths:**
- `crates/pantheon-tui/src/session.rs:10984` `do_edit`
- `crates/pantheon-tui/src/session.rs:11023` `do_continue`
- `crates/pantheon-tui/src/session.rs:10965` `do_pin`
- `crates/pantheon-tui/src/session.rs:11047` `do_clear_context`
- `crates/pantheon-tui/src/session.rs:2996` `on_turn_complete` (sets `active_run = None`)

**Mechanism.** All four begin with the same two guards:

```rust
if !state.ready { state.add_status("/edit: wait for the turn to finish"); return; }
let Some(run_id) = state.active_run.clone() else {
    state.add_status("/edit: no active run"); return;
};
```

`active_run` is set only in `start_turn` (`session.rs:7281`) and
`resume_parked_turn`, and it is set to `None` in **every** terminal path:
`on_turn_complete`, `reset_ephemeral`, the `TuiEvent::Error` arm, the
`TuiEvent::Canceled` arm, approval-deny, and clarify-cancel.

So `!state.ready` and `active_run.is_some()` are mutually exclusive by
construction:

- idle (`ready == true`) ⇒ `active_run == None` ⇒ *"no active run"*
- running (`ready == false`) ⇒ *"wait for the turn to finish"*

**Probe output:**

```
after turn complete active_run = None
do_edit guard would say: no active run
```

Every one of the four commands therefore fails 100% of the time, and each
failure message blames a condition the other guard forbids.

**User-visible impact.** `/help` and the `/` palette list `/edit`, `/continue`,
`/pin` and `/clear-context` (the last is not in the registry, but is dispatched).
Each is a dead control. `/continue` in particular is a natural thing to try after
an interrupt — precisely the state where F-01 also bites.

**Recommended fix.** These commands are *about a finished run*, so they should
resolve the run from `state.session_id` (the selected session), not from
`active_run` — mirroring `do_rewind`, which already does:

```rust
let run_id = state.session_id.clone();   // do_rewind, session.rs:8726
```

`/continue` additionally needs the `/goal`-style "queue or start" treatment and
must not run while `!ready`. `/pin` needs a real message id source (see F-09).
Failing that, delete the four commands rather than ship controls that cannot
work.

---

### F-04 — MEDIUM: context compression has no UI at all

**Severity:** Medium (silent state transition the operator cannot see or predict)
**Status:** **[CODE]**

**Paths:**
- `crates/pantheon-api/src/types_group.rs:406` `Event::ContextTrimmed`
- `crates/pantheon-api/src/types_group.rs:418` `Event::ContextCompressed`
- `crates/pantheon-tui/src/session.rs:2386` `handle_runtime_event` — no arm
- `crates/pantheon-tui/src/session.rs:2000` `event_run_id` — both are listed, so
  they are routed and then dropped by the `_ => {}` catch-all

**Mechanism.** `event_run_id` enumerates all 54 event variants including
`ContextTrimmed` and `ContextCompressed`; `handle_runtime_event` has no arm for
either, so they reach `_ => {}` (`session.rs:2731`) and render nothing.

The runtime emits `ContextTrimmed { estimated, window, dropped_rows,
compacted_rows }` and `ContextCompressed { model, exchanges, rows, chars_before,
chars_after, summary }` on every overflow-triggered fit. The docs
(`types_group.rs:406–441`) say these exist so operators know *"why earlier turns
are absent from the prompt"*.

The asymmetry is stark: a **provider error** gets a full inline card
(`BlockKind::ProviderError`); **losing half the conversation from the prompt**
gets nothing. `/compress` is manual and does report — but the automatic path,
which is the one that actually fires mid-session, is invisible.

**Recommended fix.** Add two arms:

- `ContextTrimmed` → `BlockKind::Status(format!("context trimmed: {} rows dropped, {} compacted ({} of {} est. tokens)", …))`
- `ContextCompressed` → a `BlockKind::Status` line naming the aux model, the
  exchange count, and the size delta.

Both are already filtered to the visible run by `event_run_id`, so no new
routing work is needed.

---

### F-05 — MEDIUM: `/status` diagnostics can never mention computer use

**Severity:** Medium (diagnostic channel that silently omits a whole capability)
**Status:** **[CODE]** — direct consequence of F-02

**Paths:**
- `crates/pantheon-tui/src/session.rs:11917` `let diags = session.tool_diagnostics();`
- `crates/pantheon-runtime/src/session.rs:3579` `tool_diagnostics()`
- `crates/pantheon-runtime/src/session.rs:5377` where `tool_diags` is stored

`tool_diagnostics()` returns `tool_diags`, whose contents are **overwritten** on
every `build_tool_registry` call (`runtime/session.rs:5377`). The TUI's `/status`
reads it without calling `build_tool_registry` first, so it reports whatever the
last registry build recorded — normally nothing, since the TUI calls
`build_tool_registry` only inside `/tools`. Combined with F-02, the
computer-use "unknown driver" and "binary not found" diagnostics
(`runtime/session.rs:5257–5268`) are both unreachable from the TUI.

**Recommended fix.** Have `/status` call `session.build_tool_registry()` (or a
cheap `refresh_diagnostics()`) before reading `tool_diagnostics()`, and fix F-02
so there is something to report.

---

## 2b. Verified-clean areas (negative results)

Listed so the next reviewer does not re-derive them.

**Command registry ↔ dispatch.** I extracted every
`cmd == "…"` / `starts_with("…")` / `strip_prefix("…")` literal from
`handle_slash_inner` (`session.rs:11311–13010`) and diffed it against the
`COMMANDS` table (`session.rs:56`). Result:

- Registry entries with **no** handler: **0**.
- Handlers **not** in `COMMANDS`: `/clear-context`, `/name`, `/secrets`.
  - `/name` is a deliberate alias of `/title` (`session.rs:11537`).
  - `/secrets` opens the secure form; `/clear-context` clears the prompt
    window. Both are undocumented in `/help`.
    → **[IMPROVEMENT]** document them or fold them into the registry; today
    `/help` is claimed to be "the single source of truth" (`session.rs:50`)
    and is not.
- `/collab` is registered and handled, but is a documented deprecated stub
  (`session.rs:11361`). → **[IMPROVEMENT]** remove it or mark it deprecated in
  its description text so the palette stops offering it.

**`degrade_for` is correctly wired.** `render_block` calls
`pantheon_api::ui::degrade_for(document, &tui_caps())` at draw time
(`session.rs:6442`) with a capability list that matches the components
`ui_render` actually implements (`render_group.rs:3999`). **[CODE]** verified
correct — a protocol-version mismatch degrades to a Markdown note rather than
rendering garbage.

**`/models` current-model marker.** `render_models` marks the active
configuration with a glowing `●` by comparing `"{provider}/{model}"` against
`state.model` (`session.rs:4428`). Correct.

**Provider error / fallback cards.** `handle_model_event`
(`session.rs:1585–1878`) handles all eight `ModelEvent` variants, keeps a
`card_index` so a failure card is updated in place rather than re-pushed, and
distinguishes a stacked-key rotation from a same-model retry via
`PendingProviderFailure::retrying`. The `provider/model` (not bare `model`)
comparison bugs called out in the comments are genuinely fixed. **[CODE]**

**Background-run isolation.** `TuiEvent::Runtime` events for a non-visible run
never reach the transcript: `handle_runtime_event` (`session.rs:2386`) diverts
on `event_run_id != session_id` before the match, and the background arm
(`session.rs:2298`) only mutates tab dots, the `approvals`/`clarifies` maps, the
`/btw` step list, and subagent records. The comments on
`on_turn_complete`/`on_background_turn_complete` (`session.rs:2996,3017`)
accurately describe the code. **[CODE]**

**Approval settle-on-failure.** `settle_approval_card` (`session.rs:1111`) puts
`pending_approval` back and reports the error when `grant`/`deny` fails, and
every caller checks `.is_ok()` before resuming. Covered by
`approval_card_tests` (`session.rs:1138`). **[CODE]**

**Theme discipline.** No raw `Color::` literal is used for the conversation view
outside `session/theme.rs`; `wordmark_color` interpolates deliberately.
**[CODE]** — grep for `Color::` in `session.rs` returns only the interpolator.

**Markdown GFM coverage.** `markdown.rs` (3,744 LoC) handles ATX/setext
headings, fenced code with language tags, tables with alignment, task lists
(`☐`/`☑`), reference links/definitions, autolinks (angle, bare URL, bare email),
entities, nested blockquotes with lazy continuation, nested lists, `~~strike~~`,
escaped pipes in tables, and tab expansion to 4-column stops. Pure-probe
results on a sample:

```
- a / "  - b" / "    - c"        → nested bullets preserved
1. a / "   1. b"                → nested ordered preserved
- [ ] a / - [x] b               → ☐ a / ☑ b
| a | b | / |---|---| / | 1 | 2 | → a  b / 1  2  (alignment row stripped)
~~gone~~                        → gone
https://example.com/x?a=1).     → trailing ")." handled
```

No GFM defect found. **[CONFIRMED by probe]**

---

## 3. Non-defect observations and risks

### R-01 — O(n) transcript re-render every frame
**Severity:** Medium (latency, not correctness) · **[CONFIRMED by measurement]**

`render_transcript` (`session.rs:5941`) rebuilds **every** block's lines on
every draw — markdown parsing included — then wraps the card-style blocks in
`wrap_line`. Measured against a `TestBackend(120, 40)`, debug build:

| blocks | full frame | transcript only |
| --- | --- | --- |
| 20 | 10.7 ms | — |
| 100 | 38.0 ms | 17.2 ms |
| 400 | 120.8 ms | — |
| 1000 | 320.9 ms | 162.0 ms |

At 1,000 blocks a single frame in a *debug* build costs ~320 ms; release builds
are perhaps 5–10× faster (~32–64 ms), which is still a visibly janky scroll on
a long session, and typing latency scales with transcript length.

This is a design cost, not an accidental one — the doc comment at
`session.rs:5945` explains that per-frame rebuild is what keeps `block_starts`
(timeline jumps, click-to-toggle) exact and lets wrapping happen before scroll
maths. But it forecloses the usual fix (a ratatui `Paragraph::wrap`) and the
tick is 100 ms (`event::poll(Duration::from_millis(100))`, `session.rs:9238`),
so *every* streamed token re-parses the whole history.

**Recommendation:** cache rendered `Vec<Line>` per block, invalidated by block
identity + `term_width`; only the last (streaming) block changes per token.
That keeps `block_starts` exact and removes the quadratic term.

### R-02 — `/steer` can be silently absorbed by the turn it was meant to redirect
**Severity:** Low–Medium · **[CODE]**

`do_steer` (`session.rs:10943`) checks `state.ready` to decide "steer the live
turn" vs "send as a message". `state.ready` is flipped by `TuiEvent::TurnComplete`
on the UI thread. Between the worker's last `deliver_steers` and the UI thread
observing that completion, a `/steer` writes into `steer_inbox` with the turn
already winding down.
`TuiEvent::TurnComplete` does drain leftovers into `queued_message`
(`session.rs:9021`) — a good mitigation — but the user is told
*"steering the running turn..."*, and the text arrives as a **separate next
turn**, not as steering. **[SUSPECTED]** narrow race; message is never lost.

**Recommendation:** when folding drained steers into the queue, push a
`Status("steering arrived after the turn ended; sent as a follow-up")` so the
transition is visible.

### R-03 — `/help` is not the single source of truth it claims to be
**Severity:** Low (documentation drift) · **[CODE]**

See §2b: `/secrets` and `/clear-context` are real, handled commands absent from
`COMMANDS`, while `/collab` is advertised and deprecated.

### R-04 — `new_tab` does not reset `ready`/`active_run`
**Severity:** Low · **[CODE]**

`new_tab` (`session.rs:3568`) resets transcript, meter, goal and modals but
leaves `ready`/`active_run` untouched — deliberate per its doc comment ("a turn
may still run for the previous run"). Consequence: after `+` while tab A is
running, tab B's first Enter is *queued* rather than sent, and drains when tab
A's turn completes (into whichever run is then selected). `drain_queued_message`
resolves `state.session_id` at drain time, so the message can land in a
different tab than the one the user typed it in.

**[SUSPECTED]** low-frequency; worth a status line at minimum.

### R-05 — per-frame clone in `refresh_todos`
**Severity:** Info · **[CODE]**
`refresh_todos` (`session.rs:3439`) clones the whole todo list and compares
`Vec<TodoCardItem>` every frame. Cheap today (lists are short), but it is the
same unconditional-per-frame pattern as R-01.

### R-06 — `activity_status` is state with no renderer
**Severity:** Info · **[CODE]**
`set_activity_status` (`session.rs:2258`) writes `activity_status` on every
`ToolStarted`, but the field's own doc says it "is NOT rendered as a status
line anywhere". Dead state maintained per tool call. Harmless; confusing.

---

## 4. Missing / inaccessible runtime capabilities

| Capability | Runtime API | TUI access | Note |
| --- | --- | --- | --- |
| Computer use (`[computer_use]`) | `set_computer_config` | **none** (F-02) | configured driver is silently ignored |
| Automatic compression feedback | `ContextTrimmed`, `ContextCompressed` events | **none** (F-04) | routed then dropped |
| Tool diagnostics | `Session::tool_diagnostics` | partial (F-05) | only fresh after `/tools` |
| `/edit`, `/continue`, `/pin`, `/clear-context` | — | **never succeeds** (F-03) | four dead controls |
| `todo_guidance_active()` | `Session::todo_guidance_active` | none | no UI shows whether todo guidance is engaged |
| `temporal_hint_for_run()` | `Session::temporal_hint_for_run` | none (config only) | the hint the model receives is never surfaced |
| `effective_namespace()` | `Session::effective_namespace` | indirect (`/agent` prints the profile's NS) | acceptable |
| `set_reasoning_budget()` | present | `/reasoning` uses it | OK |
| `BrowserActivity` event | emitted by browser tools | **no arm** in `handle_runtime_event` | browser narration has no TUI surface |
| `ScheduledTaskFailed`/`Recovered` | emitted by scheduler | **no arm** | only visible through `/schedule` |
| `MemoryProposed` | emitted on proposal | **no arm** | no live memory-proposal card |
| `ToolInterrupted` | emitted by recovery | **no arm** | an interrupted tool call renders as still-running after resume |
| `TurnParked` | emitted on park | **no arm** | approval/clarify cards cover the common case |
| `DecisionRequested/Made/Recorded` | decision-layer audit | **no arm** | intentional (audit traffic, not conversation) |

The last three are judgement calls rather than defects, with one exception:

**F-08 — MEDIUM: `ToolInterrupted` leaves a resumed session showing a tool call that never finishes.**

`Event::ToolInterrupted` (`types_group.rs:218`) exists precisely so a recovered
run can say *"this call's outcome is unconfirmed"*. `event_run_id` routes it to
the visible session (`session.rs:2025`) and `handle_runtime_event` has no arm, so
on resume `rebuild_transcript` produces a `TranscriptItem::ToolCall` with
`ok: None` and the card renders with a live spinner forever
(`BlockKind::ToolCall { ok: None, started: None, .. }`, `session.rs:3690`).

**Recommended fix:** add an arm that finds the card by `call_id` and sets
`ok: Some(false)` with `error: Some(detail)`, or add a distict
`ToolInterrupted` block kind.

---

## 5. Regression risk

### RR-1 — "Two channels for one concept" is the root cause of F-01
`cancel` is represented by an `AtomicBool` **and** a ledger row flag, and the two
have different clearing rules (`reset_cancel` vs `reopen_run`). This is the same
shape as the bugs the codebase's own comments record for model identity
(`provider/model` vs bare `model`, `session.rs:1626`) and approval scope
(`scope` vs `last_tool`). **Any future cancellation source must remember to clear
both.** Recommended structural fix: make `reset_cancel(run_id)` the single
clearing entry point and delete every other path that clears one channel only.

### RR-2 — Config surfaces are wired one call per capability
`run_tui_session_with` applies 9 config sections by hand
(`session.rs:6940–7030`). F-02 exists because one was missed and nothing
detected it. **Recommended:** a single `Session::apply_config(&Config)` in the
runtime, used by the TUI, the gateway, and `agui`, plus an eval test that
enumerates every `Option<…Section>` field of `Config` and asserts each reaches a
`set_*` call.

### RR-3 — `active_run` is overloaded
It is used as an interrupt target, an `/edit`/`/continue`/`/pin` selector, and a
liveness flag, and it is cleared on completion. Splitting it into
`interrupt_target: Option<String>` and `last_run: String` would remove F-03's
class of bug.

### RR-4 — Events routed but unhandled fail silently
`event_run_id` enumerates 54 variants; `handle_runtime_event` handles ~20. The
`_ => {}` catch-all means a new event variant compiles and is silently dropped.
**Recommended:** an exhaustive match with an explicit
`_ => {}`-with-comment arm per unhandled variant, or a debug assertion listing
deliberately-ignored variants, so adding an event forces a decision.

---

## 6. Prioritised recommendation list

**P0 — ship-blocking**

1. **F-01** Fix the stale `cancel_intent` flag. Add
   `Supervisor::clear_cancel_intent` and call it from `reset_cancel`; alternatively
   widen `reopen_run`'s `WHERE` clause. Ship the regression test
   *interrupt → clear → next turn is not canceled*.
   `runtime/session.rs:4155,4176,4320` · `runtime/lib.rs:1223` · `storage/core_group.rs:1590`

**P1 — high**

2. **F-02** Wire `[computer_use]` into the session in `run_tui_session_with`
   (and the gateway), via one shared resolver.
   `tui/session.rs:6995` · `runtime/session.rs:3569`
3. **F-03** Re-point `/edit`, `/continue`, `/pin`, `/clear-context` at
   `state.session_id`, or delete them.
   `tui/session.rs:10965–11080`

**P2 — medium**

4. **F-04** Render `ContextTrimmed` / `ContextCompressed` as inline status lines.
   `tui/session.rs:2386`
5. **F-08** Settle a `ToolInterrupted` card so a resumed session has no
   perpetually-spinning tool row. `tui/session.rs:2386`
6. **F-05** Refresh diagnostics in `/status`. `tui/session.rs:11917`
7. **R-01** Cache per-block rendered lines; stop re-parsing the whole transcript
   per streamed token. `tui/session.rs:5941`

**P3 — polish**

8. **R-02** Announce steering that arrived too late. `tui/session.rs:9021`
9. **R-03** Put `/secrets` and `/clear-context` in `COMMANDS`; drop or annotate
   `/collab`. `tui/session.rs:56`
10. **R-04** Status line on `new_tab` while a turn is live.
11. **R-06** Delete or document `activity_status`.
12. **RR-4** Force a decision on unhandled event variants.

---

## 7. What was *not* verified

Stated plainly so the report is not read as stronger than it is:

- **No live provider turn.** No API key/network turn was run, so streaming,
  fallback, and compression were exercised by code reading, not by a real
  session. F-01 and the render timings are the only findings backed by execution.
- **No real terminal.** Rendering was measured through `TestBackend`; Kitty/Sixel
  image placement (`config_group.rs:4426`), OSC-11 background detection, and
  bracketed-paste behaviour were not run.
- **No `/btw`, swarm, team, plugin, or MCP server was launched** — those paths are
  read-only verified.
- **F-07 was withdrawn**: `models_close` (`session.rs:1469`) deliberately leaves
  `providers` armed so Esc steps back one level, and `providers_close`
  (`session.rs:1508`) closes both. The behaviour matches the footer text
  ("Esc back to providers"). No defect.
- **Two probes were removed** after use
  (`audit_perf_probe`, `audit_cancel_probe`, and a markdown probe); `git status`
  is clean.

---

## 8. Appendix — file index

| Concern | File |
| --- | --- |
| Event loop, routing, slash dispatch, render | `crates/pantheon-tui/src/session.rs` |
| Composer, vim, sidebar views | `crates/pantheon-tui/src/session/{vim,editor,timeline,activity,bg}.rs` |
| Palette, overlays, tables | `crates/pantheon-tui/src/session.rs:3889–4760` |
| Theme palette | `crates/pantheon-tui/src/session/theme.rs` |
| Markdown / GFM | `crates/pantheon-tui/src/markdown.rs` |
| UI protocol renderers | `crates/pantheon-tui/src/render_group.rs` (`ui_render`) |
| Session lifecycle / cancel / budgets | `crates/pantheon-runtime/src/session.rs` |
| Supervisor (grant/deny/emit/observer) | `crates/pantheon-runtime/src/lib.rs` |
| Event enum | `crates/pantheon-api/src/types_group.rs:59` |
| Ledger (`reopen_run`, `set_cancel_intent`) | `crates/pantheon-storage/src/core_group.rs` |
| CLI verbs (`mcp`, `doctor`, …) | `crates/pantheon-tui/src/{config_group,heal_group,model_group}.rs` |

---

# Part 2 — Deep pass (2026-11-08, second session)

Scope: turn lifecycles (park/resume/interrupt), replay and restore fidelity,
cross-tab concurrency, queued-message routing, command execution paths.
Method identical to Part 1: read-verified against the working tree, probes
only where empirical proof was needed (none required this pass), no product
code modified by the audit.

## 9. Tree state changed during the audit

Between Part 1 and this pass the repository moved twice:

- `08d12de` — fix overlay underflow, verb registry, auth data dir
- `960db80` — **wire computer use, clear cancel flag, fix run guards**

`960db80` implements fixes for five Part-1 findings, matching the report's
recommendations:

| Finding | Fix in `960db80` | Verified |
| --- | --- | --- |
| F-01 cancel-intent poison | `Supervisor::clear_cancel_intent` (runtime/lib.rs) called from `start_turn` before spawning | [CONFIRMED] build + tests pass |
| F-02 `[computer_use]` ignored | `apply_config` now threads the section into `set_computer_config` | [CONFIRMED] |
| F-03 dead `/edit` `/continue` `/pin` `/clear-context` | all four fall back to `state.session_id` | [CONFIRMED] |
| F-04 compression invisible | `ContextTrimmed`/`ContextCompressed` render a Status block | [CONFIRMED] |
| F-08 `ToolInterrupted` spinner | card matched by `call_id` and replaced with "tool interrupted" | [CONFIRMED] |

Post-fix state: `cargo build -p pantheon-tui` clean; **312 TUI + 141 runtime
tests pass, 0 failures**. `/pin` was additionally checked end-to-end at the
storage layer (`pin_message` is a plain upsert keyed by run id — works between
turns). One intermediate revision of the `ToolInterrupted` arm referenced a
non-existent `b.call_id` field; it was corrected to a `BlockKind::ToolCall`
match before commit and the committed code compiles.

Residual from F-01's fix: the durable flag is now cleared on the *next*
`start_turn`, so `/reset`-mid-turn (which still writes `cancel_intent` via
`cancel_current_run`) can no longer poison the following turn. The window
between `/reset` and the next send keeps the flag set, but nothing reads it
before `start_turn` clears it. F-01 and its `/reset` surface are both closed
by the same change.

## 10. New findings (deep pass)

Evidence tags as in Part 1. Line numbers refer to HEAD `960db80`.

### D-01 — `interrupted` flag survives the interrupt and tab switches — MEDIUM [CONFIRMED]

`press_esc` sets `state.interrupted = true` (session.rs:2827). It is cleared
only in `begin_turn` (2900), `reset_ephemeral` (2974), and
`on_turn_complete` (3015). The `TuiEvent::Canceled` arm (9217–9245) clears
`interrupt_armed_at` but **not** `interrupted`; `switch_to_run` (3632) touches
neither. Consequences until the next turn starts:

- queued tool cards (`ok: None, started: None`) render as **Interrupted** (dim
  `■`) instead of **Queued** — including on a *different tab* after switching;
- the status line can show "interrupted" for a run that was never stopped.

Fix: reset `interrupted = false` in the `Canceled` arm (and in
`switch_to_run`, where the flag belongs to the previous tab anyway).

### D-03 — Queued message is global, not per-tab — HIGH [CONFIRMED]

`queued_message` is a single `Option<String>` on `TuiState` (session.rs:797),
set by `begin_turn` when the *any* busy state queues (2894) and drained by
`drain_queued_message`, which starts the turn on `state.session_id` **at drain
time**. `switch_to_run` does not re-key or clear it. Concrete failure:

1. Tab A is running; the user types M in A → queued ("queued for after this
   turn").
2. The user switches to tab B and sends N; B is idle, so N starts a turn on B.
3. B completes → `TurnComplete{B}` matches the visible session → drain fires →
   **M starts a turn on B**, executing in B's ledger/workspace context.

The message the user typed into A is silently delivered to B. Fix: key the
queue by run id (`HashMap<String, String>` or per-tab state) and drain only
the queue belonging to the run that just completed.

### D-04 — `attention`/`attention_note` are write-only — MEDIUM [CONFIRMED]

`on_turn_complete(false)` sets `attention = true` + note (3023–3034),
`on_background_turn_complete` ditto, the background error/cancel arms set the
note (9177, 9219). A repo-wide search finds **zero readers** — the tab dot is
driven by `tab.busy`/`tab.approval` (lease + parked decisions), and
`render_group.rs` never references `attention`. The field's own doc comment
(session.rs:822) claims "the tab UI renders it as a badge"; that badge does
not exist. The only user-visible background-completion signal is the desktop
notification/bell. Fix: render the badge (e.g. a hollow dot for tabs with
`attention`) or delete the field and fix the doc.

### D-07 — `switch_to_run` leaves the previous tab's turn state armed — HIGH [CONFIRMED]

`switch_to_run` (3632) clears transient modals, tokens, drafts, rebuilds
blocks, hydrates decisions — but does **not** touch `ready`, `active_run`,
`status_line`, `interrupted`, or `queued_message`, all of which belong to the
tab just left. Switching from a running tab A to idle tab B leaves the UI in
A's state while showing B's transcript:

- `begin_turn` sees `!ready` and queues B's messages into the global slot
  (feeding D-03);
- **Esc in tab B cancels tab A's turn**: `press_esc` arms on `!ready`, and
  `interrupt_target()` (3001) returns `active_run` = A's run id;
- the status line and context meter show A's state on B.

Switching *into* a busy tab is self-healing (its own `TurnComplete` arrives
later and resets state), but switching *away* from one is not. Fix: derive
per-tab turn state (or recompute `ready`/`active_run`/`status_line` for the
target tab on switch — `has_active_lease` already provides the busy truth).

### D-10 — Turn worker has no panic guard — MEDIUM [CODE]

`spawn_turn` (7227) and `spawn_bg_task` (7310) run `chat`/`chat_turn` on a
bare `std::thread::spawn` with no `catch_unwind`. A panic inside the turn
kills the thread before any `TuiEvent` is sent: the tab stays `ready=false`,
`active_run=Some`, status "working" — forever. Esc cannot help (the `Canceled`
event never arrives; the interrupt only arms). The ledger-side lease expires
and `settle_stuck_run` repairs storage, but the TUI state stays stuck until
`/reset`. Fix: wrap the worker body in `catch_unwind` and send
`TuiEvent::Error` on unwind so the UI un-sticks with an honest message.

### D-02 — `/edit` does not hide the rewound turn (view/ledger divergence) — LOW [CONFIRMED]

`do_edit` (10984→ shifted) emits `TurnRewound`, appends the new user message,
and starts a turn — but does **not** truncate the live view, unlike the
shared rewind core `rewind_to_turn` (8765) used by double-Esc and `/restore`.
The edited turn and its replacement are both visible until the next tab
switch rebuilds the transcript from the ledger (which drops the rewound
turn). Investigated as a potential `/restore` numbering hazard and **cleared**:
`plan_restore` + `user_message_block` converge because the view's user blocks
are always a superset of the ledger's effective turns in order, so truncating
at the Nth view block never keeps content the ledger dropped. Residual
defects: inconsistent UX between the two "rewind and resubmit" paths, and the
draft restored by a later `/restore` is the *old* edited text, not the
replacement. Fix: route `do_edit` through `rewind_to_turn`'s view truncation.

### D-09 — Steer markers vanish on rebuild — LOW [CODE]

A mid-turn steer renders live as a Status block ("steered: …",
session.rs:2649) from `SteeringProvided`, and the model context keeps it
(`rebuild_messages` has a `SteeringProvided` arm). But `rebuild_transcript`
(runtime/session.rs:8730) has **no** `SteeringProvided` arm, so a reopened or
re-switched tab loses the steer marker from the visible transcript while the
model still sees it. Fix: map `SteeringProvided` to a Status item in
`rebuild_transcript`.

### Withdrawn during this pass

- **D-05 (thinking-block merge across turns)**: `ReasoningDelta` appends to a
  trailing Thinking block regardless of turn boundaries, which *would* merge
  an interrupted turn's reasoning into the next turn's block — but every
  terminal path (Canceled, Error, park) pushes a Status/Approval block that
  breaks the append. Not reachable through normal flows; noted as a fragility
  only.
- **User-prompt loss on rebuild (investigated, not a bug)**: the ledger stores
  the raw prompt twice — `Event::UserMessage` (retry bookkeeping) *and*
  `Event::AssistantMessage` carrying `Message::user` (the transcript row).
  `rebuild_messages`/`rebuild_transcript` intentionally skip the former; the
  TUI's `Role::User` mapping is alive.

## 11. Updated priorities

| P | Item | Note |
| --- | --- | --- |
| P0 | D-03 cross-tab queue injection | silent wrong-session execution |
| P0 | D-07 switch leaves A's turn state on B | Esc in B cancels A |
| P1 | D-10 panic guard on turn workers | stuck "working" until /reset |
| P1 | D-01 `interrupted` flag reset | wrong card states cross-tab |
| P2 | D-04 attention badge | dead state + unimplemented documented feature |
| P2 | D-02 `/edit` view truncation | UX consistency + draft quirk |
| P3 | D-09 steer marker on rebuild | replay fidelity, cosmetic |

Part-1 priorities unchanged except F-01/F-02/F-03/F-04/F-08 move to
**fixed (960db80)**; regression tests for the five fixes are still
recommended before the fixes are considered locked in.
