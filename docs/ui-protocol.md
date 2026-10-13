# Structured UI Protocol (v1)

Agent-native rich UI for Pantheon. The model emits typed UI documents
through the `render_ui` tool; clients render them natively. Plain-text
responses remain first-class: every client degrades unknown content to
readable text.

## Wire format

- `UiDocument { version: 1, id, root: UiNode }`
- `UiNode { id, component }` — `id` is slug-shaped, stable within the
  document. Actions and revisions address nodes by id.
- `Component` is internally tagged (`{"type": "table", ...}`).
- Bounds: max 128 nodes, max depth 8, bounded strings/rows/fields.
  Finite chart values; form schemas validated at publish.

## Component catalogue

markdown, table, chart (line/bar/pie), metrics, list, code, refs,
form, task_list, progress, details, diff, approval, layout
(stack/columns/tabs), custom (controlled, mandatory fallback).

## Actions

Declared on components, never executable by the client directly:

- `Local` — client-local only (tabs, expand). Never reaches the runtime.
- `Submit { tool }` — form submit; params are the validated form values.
- `Tool { tool, fixed_params }` — runs a registry tool; submission
  params merge over `fixed_params`.
- `Approval { decision }` — approval-card button; parks the turn.

Submissions (`UiActionSubmission`) bind `ui_id` + `revision` + `node_id` +
`action_id` + `params` + `nonce`. The runtime validates against the
exact published document at the pinned revision: unknown ids, bad
params, superseded revisions, and nonce replays are rejected. The
revision pin is a TOCTOU guard — a submission against a revision the
client did not display is refused rather than executing a possibly
redefined action under the old display. The document is the action
registry — no parallel state. Submissions are serialized under a
session lock so concurrent same-nonce submissions execute at most once.

## Lifecycle

1. Model calls `render_ui { document, ui_id? }`. The runtime intercepts
   it before capability gating (like `ask_user`).
2. Valid documents become `Event::UiContent` (new) or `Event::UiUpdated`
   (revision, full replacement). Invalid ones settle with a model-visible
   tool error. Unknown component tags degrade to markdown fallbacks.
3. A document containing an approval card parks the turn
   (`LoopOutcome::AwaitingUiAction`); the parked `render_ui` call is
   settled exactly once, by the operator's decision. The park is
   resolved only by an approval decision for that document — form/tool
   submissions on the same document do not clear it.
4. Clients submit actions via `Session::submit_ui_action`:
   - `Approval` — requires the matching park; records the decision and
     resumes the turn with it as the `render_ui` tool result.
   - `Submit`/`Tool` — resolves the tool name against the registry and
     executes through the normal capability gate (`execute_gated`).
     Approval-gated tools are refused fail-closed: UI actions cannot
     park for approval (that would need a second approval lifecycle
     parallel to the tool-approval flow). The denial message tells the
     model to use an approval card or bind an allowed tool.
     Every attempt emits `ToolStarted`/`ToolOutput`/`ToolMessage`/
     `ToolCompleted` with a synthetic `uia-…` call id.
5. Secret form values are redacted (`[redacted]`) in recorded
   `ToolStarted` args and never enter the ledger. The tool itself still
   receives the real values.

## Client rendering

- **TUI** (`pantheon-tui/src/ui_render.rs`): native ratatui rendering,
  width-aware. Forms fill via the composer (`F`), secrets masked.
  Approvals: `a`/`y` approve, `d`/`n` deny.
- **Dashboard**: `renderUi()` in `assets/app.js` renders the document
  subset (table, chart, metrics, form, tasks, progress, diff,
  approval). All strings escaped; no raw HTML. Forms render real
  inputs (text/number/boolean/select/secret), approval cards render
  working approve/deny buttons; submissions go to
  `POST /api/runs/:id/ui-action`, which delegates to
  `Session::submit_ui_action` — the dashboard performs no action
  validation of its own.
- **Gateway**: `UiFrameKind::Component` carries the document JSON;
  text channels get a summary line via `format_text`.
- **Capability negotiation**: `tui_caps()` declares the TUI's
  component set and max version; newer protocol versions degrade to
  explanatory markdown.

## Persistence

UI rides the ledger (`UiContent`, `UiUpdated`, `UiActionRequested`,
`UiActionSubmitted`), so ordering, replay, and resume come free.
`rebuild_transcript` applies forward-only revisions. Action params are
omitted from `UiActionSubmitted` — secrets never touch the ledger.
