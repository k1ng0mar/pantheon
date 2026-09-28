# Pantheon UI State Machine

> Internal implementation spec for TUI work, not end-user documentation.
> Slash commands and verbs listed here describe design targets unless marked
> shipped; the authoritative surface is `docs/reference/terminal.md`.
> Shipped today: ASCII splash (embedded logo art + styled wordmark, wide/narrow
> variants at 64 cols), session tabs across the top (Ctrl+Tab cycle, Alt+1–9
> jump, ● marks a background session with a running turn), live status bar
> (context %, per-turn in/out, tok/s, model, turn count, session prefix,
> cost; unknown renders as —), reasoning view (◇ Thinking blocks, collapsed
> summary by default), turn timeline (Ctrl+O or F2, read-only; ↑↓ navigate,
> Enter jumps to a turn), double-Esc interrupt + double-Esc rewind
> (first Esc arms, second confirms; rewind is durable via `TurnRewound` and
> replay honors it), fullscreen editor (Ctrl+E opens $EDITOR on the input;
> blocked while an approval is pending), the §06 RUNTIME list, the
> history/models overlays, the permission card, PgUp/PgDn transcript scroll.
> Everything else in this file (palette, Ctrl+K/R/P/L hotkeys, side
> inspector, modes, compaction progress UI) is a target, not a claim.

The interaction/state matrix. Each state maps to: what triggers it, what the
screen shows, and what transitions out. Implementation order follows the
families — SESSION and AGENT first (they are the shell), then TOOLS, then the
rest.

Visual primitives shared by every family:

- Box: `╭─ {icon} TITLE ─...╮` / `╰───╯` (rounded, icon in title)
- Status glyphs: ● running · ◐ partial/waiting · ✓ done · ⚠ warn · × fail
- Event icons: ◈ pantheon · ◇ reasoning · ◉ web · ⚙ tool · ✎ file change → agent
- Every block carries: icon, title, timestamp or duration, status line
- Expandable blocks collapse to one summary line when complete

---

## 01 SESSION

| State | Trigger | Screen | Transitions to |
|---|---|---|---|
| BOOTING | process start | ASCII splash: embedded temple logo (60-col art ≥64 cols, 44-col art below; styled PANTHEON wordmark beneath), init steps ticking (config, providers, tools, memory) | READY on success; ERROR screen on failure |
| NEW | first `pantheon` run, no session | splash, empty transcript, welcome line, input focused | INPUT on first keystroke |
| RESTORED | session exists on disk | transcript replayed from ledger, banner "resumed run {id}" | READY |
| RESUME | `/resume` or `pantheon --resume [id]` | session picker if ambiguous, then RESTORED | READY |
| INPUT | user typing | input box focused, cursor visible | RUNNING on Enter |
| RUNNING | turn in flight | status bar ● working, live token counter ticking | WAITING/COMPLETE/ERROR |
| INTERRUPT ARMED | first Esc mid-run | status bar ⚠ "esc to interrupt" | INTERRUPTED on second Esc; RUNNING on any other key |
| REWIND ARMED | first Esc while idle (a rewindable turn exists) | status bar ⚠ "esc again: rewind?" | REWIND OFFER on second Esc; INPUT on any other key |
| REWIND OFFER | second Esc while idle | status bar takes over: `↩ rewind turn N (preview…)? [y] yes [n] no` | [y] → TurnRewound emitted, transcript rolls back, INPUT; [n] → INPUT |
| COMPLETE | turn done, final text rendered | status bar ✓ ready, summary line | INPUT |
| WAITING | agent asks clarification | question block with numbered options, [1][2] key hints | INPUT on selection |
| COMPACTING | context threshold crossed (78%) | compaction progress block: preserving list ticking, discard count | READY with "compacted Nk → Mk" block |
| NEARLY FULL | ctx > 78% | header ctx turns ⚠ amber | COMPACTING or manual /compress |
| FULL | ctx >= 100% | hard stop, forced compaction prompt | COMPACTING |
| INTERRUPTED | second Esc within the arm window, mid-run | cooperative cancel: intent recorded durably, status bar ⚠ "interrupted", in-flight provider request runs to its next boundary (see Open follow-ups) | READY on turn settle |
| CRASHED | panic/provider hang | recovery screen: last good checkpoint, resume/restart options | RESTORED or NEW |
| SHUTDOWN | /exit, Ctrl+D | graceful: flush ledger, "session closed; resumable" line | process exit |

## 02 AGENT

| State | Trigger | Screen | Notes |
|---|---|---|---|
| THINKING | ReasoningDelta stream | ◇ Thinking block grows; spinner; collapses to `◇ Thought · 8s · 1.2k tok` on completion | collapsed form is the default for history |
| ANSWERING | TextDelta stream | ◈ block grows with cursor at stream head | markdown rendered; code blocks get syntax border |
| WAITING_USER | turn ends with question | question block + option hints | distinct from COMPLETE |
| AGENT_ERROR | model attempt fails | `× Model request failed · provider timeout · attempt 2/3` | retryable shows countdown |

## 03 TOOLS

Universal tool-card, specialized per tool family. Card fields: name, args,
status, duration, output size, provenance/trust badge.

| Tool family | Icon | Specialization |
|---|---|---|
| shell.exec | ⚙ | shows command line `$ ...`; exit code; duration; output tail |
| python.exec | ⚙ | stdout streaming into card |
| filesystem.read/write/delete | ⚙ | path + range for read; +− counts for write; confirmation for delete |
| git.* | ⚙ | diff preview for commit; ref for checkout |
| web.search | ◉ | queries as sub-lines, result counts, "sources added to context" |
| web.open/fetch | ◉ | URL, bytes, extract summary |
| mcp.* | ⚙ | server name prefixed: `mcp.github.create_issue` |
| plugin/external | ⚙ | trust provenance badge from capability gate |

States per card: PENDING (◐) → RUNNING (●, animated) → DONE (✓, collapse) / FAILED (×, expand with error).

## 04 CHANGES (file editing)

Pipeline: read → plan → propose → approval? → write → verify → checkpoint.

| State | Screen |
|---|---|
| PROPOSED | `✎ Proposed changes` card: path, +N −M, [Enter] apply · [d] diff · [e] edit · [Esc] reject |
| DIFF | full unified diff inline, syntax highlighted, scrollable |
| APPLIED | `✓ written · checkpoint {hash}` one-liner |
| VERIFIED | `✓ tests passed` appended to the APPLIED line |
| ROLLED BACK | `↺ restored checkpoint {hash}` |
| TESTS FAILED | `× tests failed after edit` + offer rollback |

## 05 ORCHESTRATION (swarm / delegation)

| State | Screen |
|---|---|
| SPAWNING | `◈ Swarm · spawning 5 agents...` with per-agent tick-in |
| RUNNING | swarm card: per-agent rows `● researcher-01 searching 00:32`; footer `4 active · 1 complete` |
| MSG | `researcher-01 → researcher-04 "..."` one-liner in transcript, expandable |
| PARTIAL FAIL | `⚠ 4/5 agents completed · researcher-03 failed · synthesis continues` |
| SYNTHESIS | synthesis agent card highlighted |
| INSPECT | Ctrl+O on swarm card → full-screen agent inspector (per-agent transcript) |
| DEADLOCK | `⚠ swarm stalled · no progress 60s` + [k] kill · [Esc] keep waiting |

## 06 RUNTIME

Shipped: `/models` (provider → model browser, filter, Enter switches the
live default and saves it to `[model]`), `/model [provider id]` (show or
switch without the browser), `/reasoning [off|minimal|low|medium|high|xhigh|max]` (effort
for chat turns, saved to `[model].reasoning`; maps to `reasoning_effort`
on the OpenAI wire and a thinking budget on the Anthropic wire),
`/remember KEY TEXT` (agent-memory write
through the policy gate), `/skills [filter]`, `/settings` (config readout),
`/gateway` (service + outbox state), `/doctor` (full system preflight),
`/sessions` (runs holding a live lease), `/new`, `/compress` (compress now,
may spend one compression-model call), `/export [markdown|json]` (writes
`exports/<run>.md|json`).

Removed from the surface: `/memory` (`/remember` covers it), `/policy`,
`/cost` (header shows tokens), `/tools`, `/debug`, `/provenance`, `/events`
(`runs`/`audit`/`logs` cover them).

| State | Screen |
|---|---|
| MODEL SWITCH | `model changed: haiku → opus · reason: complexity` |
| FALLBACK | `⚠ opus unavailable → routing to gpt-5.6` |
| MEMORY RECALL | `◇ Memory · recalled 7` with score list, provenance line |
| MEMORY WRITE | one-liner `✓ memory stored: {topic}` |
| CONFLICT | `⚠ memory conflict` two entries side by side, resolution prompt |
| SANDBOX | tool card footer `sandbox: docker/pantheon-4f21` |
| ESCALATION | `⚠ agent requested access outside sandbox` → permission flow |
| RESOURCES | /status panel: model, tokens in/out, context window; local: device, VRAM, tok/s |
| ROUTER VIEW | /models browser: providers → models, search across both, ctx window per model |

## 07 CONTROL

| State | Screen |
|---|---|
| PERMISSION REQUIRED | full-width `⚠ Permission required` card: command, consequence estimate, [Enter] allow · [Esc] deny · [d] details |
| HALTED | "oh shit" screen: double-border block, whole-viewport takeover, [inspect] [allow once] [deny] |
| COST GUARD | `⚠ estimated cost ~$2.40 · large swarm` confirm before spend |
| STEER | inject instruction mid-run → queued, shown as `→ steering: "..."` |
| PAUSE/RESUME | `◐ paused · turn {n}` in status bar |

## 08 RECOVERY

| State | Screen |
|---|---|
| TOOL FAIL | `× Tool failed` card: code, cause, retry countdown, [r] retry · [s] skip · [Esc] stop |
| RATE LIMITED | HTTP 429 card with backoff countdown |
| PARTIAL | per-run partial-failure summary (see ORCHESTRATION) |
| CHECKPOINT | `✓ checkpoint {hash}` one-liner; /undo → checkpoint card with [r] restore; double-Esc rewind → `TurnRewound` event (durable, audit-kept; replay skips the rewound turn) |
| CRASH RECOVERY | on boot after crash: last checkpoint, transcript tail, resume prompt |
| SECRET DETECTED | `× secret detected in output · redacted · rotate advised` — never renders the secret |

---

## Interaction layer

- Slash commands: first-class palette with fuzzy search, autocomplete on args
  (`/model cla` → model list). `/models` = full browser, `/model` = quick switcher.
- Session tabs: opencode-style bar across the top, `[ 1 title ● │ 2 other ]`.
  Ctrl+Tab cycles, Alt+1–9 jumps (checked before the Char handler; Alt+1
  arrives as Char('1')+ALT). The active tab is bold cyan, inactive tabs dim,
  and a yellow ● marks sessions with a running turn — the "background
  session" indicator: you can switch away mid-run and the tab shows it is
  still working.
- Status bar: live telemetry, real or absent. Ready/working/interrupted word,
  model, context as `{used}k/{window}k` with a fill bar, per-turn in/out,
  tok/s, turn count (shows turn+1 while running), 6-char session prefix,
  cost when known. Anything the runtime did not expose renders as `—`.
  A pending rewind confirmation takes over the whole bar so the question is
  impossible to miss.
- Hotkeys: Esc arm/confirm (interrupt mid-run, rewind offer idle) ·
  Ctrl+O or F2 turn timeline (read-only; ↑↓ navigate, Enter jumps to a turn) ·
  Ctrl+E fullscreen editor (opens $EDITOR on the input; blocked while an
  approval is pending) · Ctrl+C interrupt · Ctrl+D exit · Ctrl+L clear ·
  Ctrl+Tab cycle session tabs · ? shortcut overlay
- Input editor: multiline, paste, @file refs with autocomplete, attachment chips,
  history (↑↓), undo. Ctrl+E hands the draft to $EDITOR and reads it back.
- Reasoning view: ◇ Thinking blocks stream open while the model reasons and
  collapse to `◇ Thought · {duration} · {tokens}` when done; collapsed is the
  default for history.
- Turn timeline: read-only vertical slice of the run's turns beside the
  transcript; selecting a turn jumps the transcript to its start.
- Transcript: PgUp/PgDn scroll, Ctrl+F search, Home/End jump,
  expand/collapse everything.
- Rewind: double-Esc while idle offers to rewind the last turn; confirming
  emits a durable `TurnRewound` event, the transcript rolls back to before
  the turn, and replay/resume honors it (raw history is kept for audit).
  See §08 CHECKPOINT.
- Side inspector: right pane (RUN: model/ctx/agents) when width
  ≥160 cols, auto-collapses below.
- Notifications: background events queue as `◈ 2 background events`, expandable,
  never hijack the transcript.
- Sessions: /sessions browser; session = conversation, run = execution inside it.
  Header shows session title after /name.
- Export: /export [markdown|json] saves the visible transcript to a file.
- Modes: normal/plan/auto/safe/readonly in header; SAFE MODE suppresses
  auto-approve paths.
- Capability adaptation: detect width/unicode/truecolor; ASCII fallback mode;
  reduced-animation mode.

## Weird cases (explicit designs required)

tool hang · network loss · mid-run model switch · rate limits · MCP crash ·
malformed tool output · prompt-injection in tool results · contradictory
memories · low-trust result used as instruction · swarm deadlock · recursive
spawning · runaway tokens/tools · terminal closed mid-run · power loss ·
process crash · corrupted checkpoint · invalid patch · failing tests · merge
conflict · sandbox timeout · permission denied · secret in output · mid-session
model change. Each maps to a RECOVERY or CONTROL state above; none may render
as a bare "error: something went wrong".


---

## Open follow-ups (tracked, not in current scope)

### Permission card: queued-batch approval ordering

`ApprovalRequested` carries only the opaque call id (`call_1_0`). The TUI
permission card displays the tool name and args captured from the immediately
preceding `ToolStarted` (`TuiState::last_tool`).

Edge case: if approval fires for a **queued** batch before any `ToolStarted`
for that batch has been observed, the card would show the previous tool's name
(an empty or stale one). Unlikely under the current serial approval flow, but
it is a real display-inaccuracy risk.

Proper fix (deliberately not done now, since it means changing the event
contract, not just the UI): include `tool` and `args` on `ApprovalRequested`
itself so the card is self-describing and no inference is needed.

### Interruption: cooperative-only cancel boundary

The cancel token is checked at turn entry and (via the `turn + 1` recursion)
after each tool batch. An **in-flight provider request cannot be aborted** —
the HTTP stream runs to completion and the cancel is observed on the next
boundary. A request hung at the network layer still needs the watchdog.

Acceptable for now: intent is recorded durably the instant the user confirms,
and the UI reports the interrupt immediately and honestly. Making the provider
call itself cancellable means an abortable-stream provider interface.

### Observer lock constraint

Observers run while the supervisor observer lock is held. See the
`register_observer` doc comment in `crates/pantheon-runtime/src/lib.rs`.
Observers must stay non-blocking and must not re-enter the Supervisor.
