# Pantheon Desktop — plan

Status: proposal + scaffold (2026-10-24). The desktop shell lives in a sibling
project (`~/projects/pantheon-desktop`, Tauri 2) so the Pantheon workspace keeps
its std-only dependency discipline; see "Placement" below.

## 1. What this is based on

Four shipped agent desktops were reverse-engineered in October 2026 (full
findings in the ZCode-workspace report `REVERSE-ENGINEERING-REPORT.md`;
pandacode is public source). What matters for Pantheon Desktop:

| Product | Stack | Core lesson for us |
|---|---|---|
| **ZCode** (Z.ai) 3.16.3 | Electron + custom React, in-process agent CLI | The desktop is a **shell over an agent host** that owns a versioned NDJSON wire protocol (V4): conversation subscribe/resync, rowsRange, attachments chunking, workflow-run events, off-peak control. Renderer never talks to the world directly. |
| **AutoClaw** (ZhipuAI) 1.18.1 | Electron + patched OpenClaw gateway | Credentials never reach agent processes: a loopback **Model Broker** rewrites `body.model` and injects auth; subprocesses get a `envPassthrough` allowlist. Also: forced-yolo trust boundaries are product risk, not a feature; request signing with an embedded MD5 secret is weak. |
| **MiMo Code** (Xiaomi) | Electron + OpenCode-derived engine, SQLite event store | Event-sourced storage (`session/message/part/event/permission`) is enough to run a whole product; feature gates split domestic/overseas builds; a local token-authed HTTP API is how first-party tooling attaches. |
| **PandaCode** 0.3.37 | **Tauri 2 + Rust + vanilla TS**, OS webview | A lean desktop can drive *many* harness CLIs with bounded output, deadlines, process-group cleanup, warm-agent pooling, steering, named connections with a secrets store, and an E2E-encrypted relay (Cloudflare DO + x25519/chacha20poly1305) for phone control. No bundled Chromium, no React, no resident Node. |

Common failure modes observed across the closed-source three that Pantheon
already designs against: policy-in-prompt instead of policy-in-runtime (all
three ship "yolo" with at most a UI toggle), prompt-parsed approvals, telemetry
SDKs with opaque beacons (ARMS, DataRangers/APMPlus), and update channels
pointing at first-party proxies that rewrite model traffic.

## 2. Decision

**Tauri 2 shell around `pantheon-dashboard`, not a new Electron app and not a
pandacode fork.**

- Pantheon's authority model (capability gate, SQLite ledger, UiDocument v1
  approval parking) lives in the runtime. A desktop app must *display and
  submit* to that authority, never re-implement it. The dashboard SPA already
  renders runs, approvals, scheduler, memory and stats against the ledger with
  per-instance token auth; the shell's job is to make that a first-class app.
- Electron (zcode/AutoClaw/MiMo) costs ~200 MB of bundled Chromium per app and
  a resident Node; pandacode demonstrates the Tauri path works for exactly this
  product shape. Tauri also keeps the Rust toolchain we already target.
- Forking pandacode directly would import its architecture: task/session state
  owned by the Tauri backend in its own SQLite. Pantheon requires the opposite
  split — the runtime owns state, the desktop is a client. Reuse patterns and
  (with attribution, per its license) the relay protocol instead.

## 3. Architecture

```
┌──────────────────────────── pantheon-desktop (Tauri 2) ─────────────┐
│  main.rs                                                            │
│   ├─ supervisor: spawn `pantheon serve dashboard [--port N]`,       │
│   │   capture stdout line "pantheon dashboard on http://…/?token=…" │
│   │   process-group kill on exit; restart on crash (bounded)        │
│   ├─ webview window → External(http://127.0.0.1:<port>)             │
│   ├─ tray: show/hide, runs status (later: /api/stats), quit         │
│   ├─ global shortcut: quick overlay window (pandacode pattern)      │
│   ├─ single-instance + deep links (pantheon://run/<id>)             │
│   └─ updater (tauri-plugin-updater, later)                          │
└──────────────────────────────┬──────────────────────────────────────┘
                               │ HTTP + WS, X-Pantheon-Token
                    ┌──────────▼───────────┐
                    │ pantheon-dashboard   │  (unchanged, std-only)
                    │  SPA + /api/*        │
                    └──────────┬───────────┘
                               │ in-process
                    ┌──────────▼───────────┐
                    │ pantheon runtime     │  ledger, capability gate,
                    │                      │  providers, scheduler, memory
                    └──────────────────────┘
```

Phase 1 keeps HTTP as the only seam. Phase 2 can move hot paths (event stream,
approvals) to Tauri IPC without touching the runtime — the dashboard's
`App`/mount structure (`DashboardMount`) is already library-shaped.

## 4. Phases

- **P0 (scaffolded now):** shell supervises `pantheon serve dashboard`,
  parses the tokened URL, opens the webview, tray with Show/Quit, single
  instance, clean child teardown. Windows/macOS/Linux profiles.
- **P1 native chrome:** global-shortcut overlay window (new task / running
  tasks), notification bridge (ledger events → OS notifications), dock/tray
  badge, auto-start, updater channel (`cdn` pattern: static file feed, signed).
- **P2 deeper integration:** token handed to webview via IPC instead of URL
  query; ledger event stream as Tauri events; native approvals surface
  (UiDocument approval cards → OS notification with Approve/Deny, still
  validated server-side — the revision-pinned submission is the guard).
- **P3 remote + mobile:** adopt/reimplement pandacode's relay design
  (GitHub-OAuth device pairing, per-account Durable Object, E2E-encrypted
  frames — no task content at rest) so `pantheon-mobile` (Flutter/NYX) and the
  desktop pair against the same endpoint. pandacode's relay is MIT-with-
  attribution; lifting the protocol is allowed with the attribution line.
- **P4 differentiation:** things none of the four have — provenance-aware
  memory browsing, nightly self-improvement diffs as first-class UI,
  capability-gate replay from the ledger (rewind = new branch, never rewrite).

## 5. pandacode → Pantheon reuse map

| pandacode | Where it lands |
|---|---|
| `providers.rs` harness adapters (spawn, bounded output 1 MiB/256 KiB, deadlines 8–15 s, process-group cleanup, warm pool `epoch/take/prewarm`) | `pantheon-providers` already owns provider/streaming; the bounded-spawn helpers and warm-pool pattern are worth porting into `pantheon-exec` |
| `connections.rs` named connections + secrets store (secret names only cross the bridge) | matches `pantheon-secrets` broker; adopt the "file and webview only see names" rule in dashboard settings UI |
| approval bar (one line per request, grouped approve-all) | dashboard SPA; approvals stay server-validated via UiDocument revision pins |
| task board + message queueing/steering | `pantheon-runtime` queue/steer already exists; board is dashboard UI work |
| `relay/` E2E device pairing | P3 (above) |
| `windows_job.rs` (Windows job objects for child cleanup) | needed when Pantheon ships Windows builds; port to `pantheon-exec` |

## 6. Placement

Sibling project, not `crates/pantheon-desktop-tauri`: Tauri pulls tokio and a
winit/wry stack into the build graph, which the workspace's std-only rule
exists to avoid. The shell never links Pantheon crates — it talks HTTP. If a
later phase wants in-repo, add `exclude = ["desktop"]` to the root workspace.

## 7. Non-goals / risks

- No bundled model proxy in the shell: model traffic stays inside the runtime
  under the capability gate (AutoClaw's broker exists because its gateway
  cannot be trusted with tokens; Pantheon's threat model already fixes that).
- No telemetry in the shell beyond opt-in crash reports; updater feed is
  static-file based (zcode's localhost-updater dev stub is what shipping on a
  real feed looks like — decide the feed domain before P1).
- Webview parity: WebView2/WKGTK/WebKit differences are the Tauri tax; the
  dashboard SPA already ships a service worker and must be tested on all three
  engines (pandacode's plain-TS approach shows the ceiling is fine).

## 8. Further prior art (2026-10-24): DeepSeek Harness, Claude Cowork, ChatGPT Work

### DeepSeek Harness (`dsh`, deepseek-ai/deepseek-harness, v0.2.1-rc.3, MIT)

TypeScript pnpm monorepo (Node ≥ 24.9), 53 capability packages + Electron
desktop (`apps/desktop` shell + `apps/desktop-host`) + `apps/web` SPA + `dsh`
CLI. Two ideas worth stealing outright:

- **Cordis, "everything is a plugin"** (vendored): explicit DI, scoped
  services, lifecycle-managed cleanup, config-driven loading. Pantheon's
  `pantheon-extensions` is the same seam with less machinery; dsh proves you
  can run an entire product (model adapter, tool registry, agent loop, GUI
  host) as plugins over one kernel. Guard-failure mode it fixes: lifecycle
  cleanup as a framework guarantee, not per-module discipline.
- **Capability package families with READMEs as contracts** (`guard/`,
  `spill/`, `ptc-runtime/`, `goal/`): each family documents what it provides
  and who owns each part. Directly adoptable:
  - `guard/` — loop-hygiene: repeat-tool-call reminder + per-tool-call timeout
    policy. Cheap, high-value Pantheon extension plugins.
  - `spill/` — oversized tool results spill to storage, context gets a
    locator with retrieval guidance (`maxInlineTokens`); failure keeps the
    original inline. Pantheon's ledger is already the storage; this is a
    context-policy layer on top.
  - `ptc-runtime/` — programmatic tool calling: model writes programs against
    host-provided async functions, fresh process under sandbox policy,
    opt-in persistent confined REPL per state owner. Pairs with
    `pantheon-exec::sandbox` levels.
  - `goal/` — one durable completion objective per session, survives
    restart/resume/fork, `/goal` human command, opt-in continuation rounds.
    Complements the scheduler (records completion state, does not schedule).
- **Validation for our stack split:** dsh also ships an HTTP host + SPA web
  GUI (`host/`: SPA dist server, workspace-picker seams, plugin inventory)
  — the same shape we chose (shell → HTTP → dashboard). Their desktop is
  Electron; ours being Tauri is the one place we deliberately diverge.

### Claude Desktop / Cowork (Anthropic; vendor architecture doc)

Cowork = Claude Agent SDK with no terminal; desktop hosts three apps (Chat /
Cowork / Claude Code). Launched Jan 2026 (macOS), Feb 2026 (Windows), GA Apr.

- **Two-environment local split:** agent loop runs natively; **code execution
  runs in a hypervisor-isolated Linux VM** (Virtualization.framework /
  Hyper-V). Pantheon's sandbox ladder (`pantheon-exec`) should grow a VM level
  for exactly this "agent trusted with files, code not trusted with the host"
  split.
- **Egress as a first-class gate:** cloud sandboxes get a mandatory,
  non-bypassable proxy with allow-listed destinations; private/link-local/
  cloud-metadata addresses denied by default; session-scoped credentials that
  expire within hours; connector tokens never enter the sandbox. This is the
  runtime-level version of what Pantheon's capability gate does for tools —
  apply the same model to network egress (`pantheon-gateway`).
- **Grants can be non-persistent:** admin toggle disables "always allow" so
  every permission-gated call needs fresh approval. Make "no persistent
  grants" a first-class Pantheon profile.
- **Desktop app as the local bridge for cloud sessions:** cloud work reaches
  local files only through the desktop app's brokered, permission-checked
  connection. Same shape as our P3 relay plan — the shell is the on-device
  authority proxy, and offline shell = unreachable files.
- **Observability + blind spots:** Compliance API / OpenTelemetry for admins;
  EDR can't see inside the local VM. If Pantheon grows a VM sandbox level,
  ship an equivalent audit stream so the VM isn't a visibility hole.

### ChatGPT Work / merged ChatGPT desktop (OpenAI, July 9 2026)

Cloud-first agent ("ships files, not just chat") + merged macOS/Windows
desktop. One actionable lesson: **migration as onboarding** — it imports
settings/skills/projects from Claude Code, Claude Cowork, and Cursor.
Pantheon already has `pantheon-migration` (Hermes/OpenClaw/OMP); add Cowork
and ChatGPT Work export formats to its target list when their formats are
documented/observable.

### Amended decisions

- P1 adds: loop-hygiene guards + tool-result spilling as Pantheon extension
  plugins (cheap wins from dsh).
- P2 sandbox ladder gains a VM-backed level (Cowork's local split is the
  reference implementation shape).
- `pantheon-gateway` egress policy should adopt allow-list + session-scoped
  credentials, not just tool-level gating.

## 9. Decision change (2026-10-24): PandaCode is the base, not a reference

The Tauri-shell-wrapping-the-dashboard approach (P0, preserved at
`~/projects/pantheon-desktop-shell-p0`) was a browser-in-a-box. The product
goal is PandaCode's workspace — sidebar of agents/projects, task board,
worktrees, per-turn diffs, display switcher (workspace/browser/terminal),
steering, approvals bar — with **Pantheon as the harness it runs**.

What exists now in `~/projects/pantheon-desktop` (fork of pandacode v0.3.37,
full history, PandaCode attribution license honored):

- `pantheon` registered as a first-class ACP harness: `pantheon serve acp`
  (verified: initialize handshake answers protocolVersion 1, loadSession,
  embeddedContext). One ACP session = one Pantheon run; approvals arrive as
  `session/request_permission`; modes map to Pantheon's Build/Plan.
- Pantheon is the default provider for new tasks, first in the picker list,
  and in the executable-override allowlist. Discovery: PATH, then
  `~/projects/pantheon/target/release/pantheon`, then `~/.local/bin/pantheon`.
- Upstream harnesses stay dormant (discovery-gated) — stripping them is a
  deliberate later pass, not silent breakage.
- Identity retargeted (`app.pantheon.desktop`), `cargo check` + `tsc` clean.

Next steps in priority order:
1. First-run experience: Pantheon-first composer defaults, provider settings
   page for the pantheon harness (binary path override already works).
2. Wire Pantheon-specific surfaces where PandaCode ones don't fit: the
   capability gate's approval semantics already map to `request_permission`;
   UiDocument v1 render_ui events will need an ACP session/update mapping
   (new upstream capability or a Pantheon ACP extension) — design before P2.
3. Then re-apply the plan's P1+ phases on top of this codebase (notifications,
   updater, relay for pantheon-mobile), replacing this document's §3
   architecture diagram's shell internals with the fork's structure.

## 10. The steal-list (what we take from each product)

Per-source, each item names where it lands in Pantheon. Ordering = priority
within the source; items already done are omitted.

### Codex (openai/codex; app-server protocol via the fork)
1. **Turn-scoped lifecycle + clean interrupt** — `turn/interrupt`, explicit
   turn states, model catalog with preserved display names. Lands in the ACP
   layer / providers of the fork, and as semantics for `pantheon run`.
2. **Sandbox ladder as per-task policy** — read-only / workspace-write /
   danger-full-access chosen per task with per-call approval overrides, not
   one global mode. Lands in `pantheon-exec` policy profiles + fork composer.
3. **Goals as durable objectives** (same idea as dsh `goal/`) — one
   completion objective per session surviving restart/fork, with explicit
   continuation rounds. Lands in `pantheon-runtime` alongside the scheduler.

### Claude Desktop / Cowork
1. **VM-backed code-execution sandbox** — agent loop native, code execution
   in a hypervisor-isolated VM (Virtualization.framework / Hyper-V). Next rung
   of the `pantheon-exec` sandbox ladder above OS sandboxes.
2. **Egress as a runtime gate** — mandatory non-bypassable proxy,
   allow-listed destinations, private/link-local/metadata denied by default,
   session-scoped short-lived credentials, connector tokens never enter the
   sandbox. Lands in `pantheon-gateway` + `pantheon-secrets` broker.
3. **Non-persistent grants profile** — a config where "always allow" is
   disabled and every gated call re-prompts. Lands in `pantheon-agent`
   permission profiles.
4. **Desktop app as the on-device bridge** for remote/cloud/mobile sessions —
   the shell brokers file access for P3 relay sessions; offline shell means
   unreachable files. Shapes the fork's P3 role.
5. **Admin observability** — audit event stream + OpenTelemetry export for
   org deployments; avoid Cowork's EDR-blind-VM mistake by shipping the
   audit stream from inside any VM sandbox we add.

### ZCode
1. **Resync-after-gap robustness** — sequence-checked event frames plus an
   authoritative snapshot resync (`v4/conversation/resync`, `rowsRange`) when
   a client detects a gap. Lands in the dashboard WS/event-stream protocol
   and the fork's task views.
2. **Workflow runs as events** — `workflowRun{Events,Runs,Workspace,NodeResult}`
   surfaces multi-step runs as first-class subscribable state. Lands in
   `pantheon-runtime::swarm` projections + dashboard board.
3. **Off-peak compute control** — a client-usable lifecycle (`offPeakControl`,
   `offPeak/lifecycle`) for deferrable work. Lands in `pantheon-scheduler` +
   dashboard.
4. **Remote agents over SSH/WSL** — deploy agent bundles to a remote host
   with hardened tar extraction, run under a managed runtime root. Lands in
   `pantheon-exec` remote targets.
5. **TTFT/local telemetry facts as events** — client-measured latency facts
   ride the event bus instead of a hidden beacon. Lands in the event ledger.

### MiMo
1. **Local query classifier** — small ONNX model routes each request (which
   harness/model/tool path) before spending tokens. Lands in
   `pantheon-providers` as an optional pre-router; onnxruntime optional.
2. **Migration as onboarding** — `claude_import` imports rival tools' session
   history (they import Claude Code/Cowork/Cursor). Extend
   `pantheon-migration` targets: Claude Code, Cowork, ChatGPT Work.
3. **Feature-gate profiles** — domestic/overseas build surfaces as config
   profiles (compliance-driven gating). Lands in the dashboard/fork settings.
4. **Vendored office stack** — hash-pinned LibreOffice + document pipeline
   for deliverables. Lands in `pantheon-tools`/document plugin.
5. **Transcript evidence store for self-improvement** — evolve-transcript DB
   backing their evolve-seed. Lands in `pantheon-nightly` as its evidence
   input (ledger already has the events).

### AutoClaw
1. **Model Broker credential isolation** — harness/subprocesses never hold
   tokens: a loopback broker rewrites model + auth per request, children get
   an explicit `envPassthrough` allowlist. Lands in `pantheon-secrets` broker
   as the enforcement path for provider credentials.
2. **Evolution governance (Hermes)** — proposal → human approval →
   HMAC-verified journal, fail-closed write guards, recursion exclusions.
   The governance shape for `pantheon-nightly` self-improvement.
3. **IM channel breadth** — WeChat/WeCom/QQ/DingTalk/Lark channels. Roadmap
   items for `pantheon-gateway`.
4. **Skill hot-update + usage ledger** — skills update without restart and
   their usage is auditable. Lands in `pantheon-extensions`.
5. **Anti-patterns (deliberately NOT taken)** — runtime-patching a vendored
   engine instead of shipping a clean fork; forced-yolo with a documented
   `workspaceOnly` bypass; MD5 static-key request signing with an embedded
   app secret.

### Done today (from this list)
- **UiDocument over ACP** (ZCode-adjacent robustness + Cowork-style
  permission mapping): `pantheon-tui/src/acp.rs` now renders `UiContent`/
  `UiUpdated` as markdown fallback + `_pantheon/uiDocument` extension
  notification, and parked UI approval cards surface as standard
  `session/request_permission`, resolved through the runtime's
  revision-pinned `submit_ui_action`. Standard ACP clients (Zed, the fork)
  work with zero client changes.

## 11. MonoCode (hardbeat920/monocode, v0.16.0) — the UI reference

Tauri 2 + React 19 + CodeMirror 6 + Tailwind 4; sessions as tabs, split
panes via title-bar drag (`TitleBarPaneDrop`), a `quick-composer` global
launcher, checkpoints in the backend, Jira/Linear/GitLab/Azure DevOps
integrations, and harness support incl. Hermes Agent and Devin. Two steal
groups:

### UI system (the part that makes it look wonderful)
1. **One-hue theme token system.** The entire palette derives from a single
   `--theme-hue/--theme-saturation` pair plus lightness scales
   (`hsl(var(--theme-hue) ...)`), with every surface, stroke, and selection
   computed via `color-mix(in srgb, var(--color-content) N%, transparent)`.
   Coherence is structural, not per-component taste. Port the token sheet
   into the fork's CSS (PandaCode's vanilla-TS UI can consume the same
   custom properties unchanged) and expose hue/saturation in settings the
   way `features/settings/model/appearance.ts` does.
2. **WebKit-known-quirks discipline.** They document and work around
   Tailwind's `:is(:where(.group))` focus-invalidation bug in WebKit —
   exactly the OS-webview tax from §7. Keep a quirks ledger in the fork.
3. **Tabs-as-sessions + split panes with drop targets**, `GlassBackdrop`
   for window chrome, compact `--leading-label` typography discipline
   (descenders in 10px tab labels).
4. **Streamdown** for streamed markdown/code/mermaid — candidate to replace
   ad-hoc markdown rendering in the fork's conversation view.

### Product ideas
5. **`/operator`** — one slash command that lets the running agent open
   sessions and manage worktrees/notes through a narrow, audited surface.
   Maps cleanly onto Pantheon's capability gate: it IS a tool projection.
6. **Checkpoints** (backend `checkpoint.rs`) per session — pairs with the
   ledger: checkpoint = worktree/Git state anchor, ledger = decision state.
7. **Monos** — cross-project agents with memory + schedules. Independent
   confirmation of Pantheon's scheduler+memory direction; steal the framing
   (first-class entity in the sidebar rail) for Pantheon runs.

### Done (steal-list execution, 2026-10-24)
- **MonoCode one-hue tokens** (`pantheon-desktop/src/pantheon-theme.ts`):
  hue/saturation dial computes every existing palette variable — including
  the `rgb(var(--accent-rgb)/α)` triplet, which pure CSS cannot derive —
  with WCAG auto-contrast for `--on-accent`, light/dark lightness levels
  mirrored from the original sheet, persisted in localStorage, applied
  before first paint. Default: Pantheon purple (hue 262). The CSS itself is
  untouched; reskinning is data, not a rewrite. Settings dial UI is the
  follow-up; a sweep of the few hardcoded brand hexes (e.g. `.brand-name`)
  is the second.
- **Codex-style per-task mode mapping for Pantheon**: the fork's composer
  approval modes now map explicitly — Standard/Accept edits/Full access →
  `session/set_mode: build`, Plan mode → `session/set_mode: plan` — with
  Pantheon's runtime capability gate remaining the actual policy (its
  `request_permission` calls surface in the fork's approval bar unchanged).

### Done (steal-list execution II, 2026-10-24, fork commit 0c2d346)
- **Layered ownership settled:** `pantheon-theme.ts` owns the eight neutral
  variables (the hue-tinted surface family); `palette.ts` owns accents and
  gained a `pantheon` mode fed by the dial (`pantheonAccentSource()`), and
  is now the default accent (previously wallpaper→classic-yellow fallback).
  `palette.write()` no longer removes neutral names, so accent switches
  can't erase the tint. Settings → Accent gains the Pantheon choice plus a
  live Hue / Surface-tint dial (slider pair, in-place swatch updates so
  drags don't drop). `theme.ts` applies tokens before first paint.
- **Hex sweep:** sidebar glow, workspace-header washes (4), and brand-name
  colors now derive from `--accent-rgb`/`--muted` instead of hardcoded
  yellow. Remaining known hardcode: quick-overlay's `#ffdb40` (separate
  surface, later pass).

### Queued designs (next focused passes)
1. **MonoCode-style session tabs** (the "tab session thing"): the fork
   currently shows one task at a time in the conversation column with
   sidebar-driven switching. Add a tab strip over that column: one tab per
   open task (title, provider mark, running indicator, pending-approval
   badge), click = existing `selectSession`, ✕/middle-click = archive,
   overflow scroll, and (phase 2) drag-to-split reusing the pane system.
   State: an explicit open-task list (UI store, LRU-capped) distinct from
   the session history. Size: ~300–400 lines TS+CSS, no backend changes.
2. **dsh loop-hygiene guards as bundled Pantheon extensions**: a
   `PreToolCall` gate extension that hashes (tool, args) per run and, on
   the third consecutive identical call, denies with a change-approach
   message (dsh's advisory reminder, expressed through Pantheon's existing
   deny-with-reason semantics); companion per-tool timeout policy. Hooks
   and gate classes already exist in `pantheon-extensions`; this is
   manifest + runner + tests in a dedicated pass.

### Done (steal-list execution III, 2026-10-25)
- **MonoCode session tabs** (fork commit `c137e77`): `src/session-tabs.ts`
  renders one tab per open task above the conversation column — provider
  mark, title, pulsing running dot (`status === "running"`), pending-approval
  badge fed by the live stream's approval map, ✕/middle-click/Delete closes
  the tab (never the task), Enter/Space activates. Open list is persisted,
  LRU-capped at 12, pruned when tasks vanish. Draft tasks get a tab only
  after their first prompt; closing the active tab falls back to the next
  open tab or the no-selection view.
- **dsh loop-hygiene guard, native** (runtime commit: `loop_guard.rs` +
  gate wiring in `RegRunner`): refuses the third consecutive identical
  (tool, args) call with a model-visible change-approach reason, shaped
  exactly like an extension deny ("[blocked by runtime policy] …"). Exact
  string comparison (no hash collisions), per-turn budget (a new user
  message resets it), counter resets after any differing call or denial.
  Unit tests included; the crate-wide test run is blocked only by the
  in-flight `correlation_id` storage refactor already staged in the tree.

## 12. Hermes Desktop (Nous Research, hermes-agent/apps/desktop) — the engineering discipline reference

Electron + React over the Python hermes-agent gateway; open source, and the
best *written* codebase of everything studied — `DESIGN.md` (611 lines) is a
real design constitution and `AGENTS.md` an engineering one. Steals, grouped:

### Architecture discipline (AGENTS.md)
1. **Three-party authority split**: Electron owns the machine; the renderer
   owns the experience; the backend owns the work. "Native power arrives
   through a deliberate capability, not a general escape hatch." For the
   fork: Tauri = machine, renderer = experience, **Pantheon runtime = work**
   — and it names the deviation we inherited from PandaCode (task truth in
   the shell's SQLite) as the seam to migrate in P2.
2. **"Decide state by authority"** — first question is *who is allowed to be
   right about it*; persisted state declares its scope in its own key
   (global/connection/profile/session/project/window). Adopt as a rule for
   every new localStorage/DB key in the fork and every ledger surface.
3. **"Server truth is cached, not owned"** — merge don't clobber; be
   optimistic then honest (visible rollback); generation counters against
   stale responses; isolate the foreground (background work updates caches
   quietly); coalesce noise, flush signal (turn-terminal transitions always
   arrive immediately); preserve reference identity on no-ops.
4. **The observable ladder** — every cross-boundary fallback (versions,
   discovery, capability detection, remote vs cloud) is an ordered ladder of
   validated candidates with written precedence, one resolver per policy,
   bounded retries ending in a real recovery affordance. Pantheon already
   needs this for provider/runtime discovery; adopt as the house pattern.
5. **"Guest content never opens anything by itself"** — external content
   (links, files, tool output previews) can never trigger navigation or
   actions without a user gesture. Cheap, important renderer rule.
6. **"Keep the waist narrow, grow at the edges"** — no universal plugin ABI
   until a second real consumer exists; agent-callable renderer capabilities
   belong to the session's client, not the host. Aligns with §2's decision
   and guards against over-building the fork's plugin story.

### Design system (DESIGN.md)
7. **Principles**: flat-not-boxed (no card-in-card; group with whitespace +
   one hairline); borderless elevation (one `--shadow` + currentColor
   hairline token for ALL floating surfaces); one primitive per concern;
   tokens-not-literals (raw hex in a component is a bug); style lives in the
   primitive (call sites pass variant/size, not className overrides);
   intent-before-automation (a tool result must not move focus or navigate);
   immediate-feedback (paint first, reconcile after, roll back visibly).
8. **IA taxonomy**: pages (durable destinations: chat, skills, messaging,
   artifacts) vs route overlays (short tasks: settings, command center —
   cards that return to the previous route) vs panes (working context whose
   state survives hiding). Map the fork's views onto this taxonomy.
9. **Chat is the home surface** — transcript and composer stay primary;
   tools/previews/files/review/terminal complement the conversation.
10. **Mechanics**: context-sensitive dialogs; panel titlebars as a shared
   primitive; a checklist section ("Before you add something") enforced in
   review; the doc itself is maintained with the code ("a stale name in this
   file is a bug").

### Harness opportunity
11. `acp_adapter/` — Hermes Agent speaks ACP. It can register in the fork's
    harness list the same way Pantheon did (`hermes-agent acp`), and
    MonoCode already treats it as first-class. Do this when the user wants
    Hermes sessions beside Pantheon runs.

### Done (steal-list execution IV — Hermes, 2026-10-25, fork commit 49f1169)
- **Compatibility verified first**: `hermes acp` (Nous Research hermes-agent
  v0.21.5) answered the fork's exact initialize frame — protocolVersion 1,
  loadSession, image prompts, session fork/list/resume, nous-credential auth.
  Boot ~15–20 s (Python), inside the fork's 65 s ACP discovery budget.
- **Hermes Agent registered as a harness** (the second through the proven
  checklist): `hermes acp` launch args, `~/.hermes/hermes-agent/.hermes/bin/
  hermes` discovery fallback, all backend match arms, full TS side. It now
  sits beside Pantheon in the picker; both run over the same ACP seam.
- **Guest-content rule enforced**: `src/external-links.ts` now makes
  relative/`file:`/unknown-scheme links from agent output inert (before:
  relative links could navigate the webview away from the SPA). User-clicked
  http(s) still opens in the OS browser.
- **Scope-in-key applied**: all persisted keys renamed `pandacode.*` →
  `pantheon.*` (25 source files + tests), with the migration chain extended
  (`kingcode. → pandacode. → pantheon.`, copy-if-absent, marker per chain).
  Scope rule documented; session-tabs was already `pantheon.open-tabs`.
- **House rules codified**: `AGENTS.md` gains the fork section — three
  authorities, state-by-authority, server-truth-cached-not-owned, guest
  content, observable ladder, harness checklist, token discipline.
- Suites: `tsc` clean, `npm test` 136/0 (README-link test adapted to the
  fork's attribution contract), `cargo check` clean.

### State→UI coverage audit (2026-10-25, user prompt: "imagine if agent asks for clarify and theres no ui")

Every task state must have visible UI. Audit of the fork (renderer + ACP):

| State | Source | UI | Verdict |
|---|---|---|---|
| Turn streaming / tool steps | stream events | transcript + live status | ✓ |
| Tool approval (capability gate) | ACP `session/request_permission` | approval bar, grouped | ✓ |
| UI approval card (`render_ui` park) | mapped to `request_permission` | approval bar | ✓ (89c6de0) |
| **Clarify / ask_user** | `LoopOutcome::AwaitingInput` | was: plain text, app looks idle | **FIXED (8362812/b29f87f)** — `_pantheon/input_request` renders the existing question form (options + Other free text); answer rides the next prompt per Pantheon's contract; `_pantheon/input_resolved` retires the form |
| Budget exhausted (`max_tokens`, `max_turn_requests`), refusal | ACP `stopReason` | was: **silent turn end** | **FIXED** — `acp_stop_notice` emits status for every non-end_turn stop (all ACP harnesses benefit) |
| Usage limit hit | `limitedAt/limitedUntil` | limit UI + resume-at-reset | ✓ (upstream) |
| Run error / provider failure | error events | live errors + notification | ✓ (auth-fix flow is manual provider settings) |
| Stopped / cancelled | stop events | status | ✓ |
| Completed / archived | taskStage | board + archive | ✓ |
| Queued / steered | queue_add + steer events | queue + steering UI | ✓ |
| Remote device offline | remoteDevice | remote UI | ✓ (upstream) |
| Cloud agent stages | cloud.stage | cloud UI | ✓ (upstream) |
| Draft (new task) | draft flag | new-task page | ✓ |
| Goal input (`goal_input`) | goal events | goal command center | ✓ (upstream, Pantheon goal states still to map) |

Remaining known-thin spots, queued: provider-auth-failure could deep-link to
the provider settings row; Pantheon goal/plan state surfaces (UiDocument
forms) ride the §8 mapping when built; a renderer-side state machine test
("every AgentEvent kind must mutate visible UI or be explicitly inert") is
the durable guard — add when the Playwright suite next runs.

### Audit gaps filled (2026-10-25, follow-up)
- **Auth failure** (was "manual provider settings"): auth-shaped errors
  (401/unauthorized/invalid key) now flip the task to "Sign-in required" —
  banner above the composer with **Open provider settings** (one click to
  `showSettings('providers')`), queue holds like a failure, flag clears when
  the next turn starts.
- **Durable guard**: `tests/agent-event-coverage.test.mjs` — parses the
  `AgentEvent` kind union and fails if any kind has no renderer handling
  unless listed in a documented INERT allowlist (stale allowlist entries for
  handled kinds also fail). The state-machine class of gap can no longer
  reopen silently.

### Placement decision change (2026-10-25): the desktop app lives in the runtime repo

The §6 sibling-project rule is superseded by user decision: the app now lives
at **`<repo>/desktop`** (own Tauri workspace; the std-only root workspace
`exclude`s it, and `desktop/src-tauri/Cargo.toml` carries an empty
`[workspace]` so cargo never merges them).

- **`pantheon desktop`** (runtime CLI verb): locates `desktop/` (env
  `PANTHEON_DESKTOP_DIR` → checkout layout → `~/.local/share/pantheon/desktop`),
  builds what is missing or stale (`npm install` → `vite build` →
  `cargo build --release`; staleness by mtime ladder), then launches the
  shell — which supervises `pantheon dashboard` itself. Flags: `--rebuild`,
  `--skip-build`, `--register`, `--no-register`.
- **App registration**: after a build, the verb writes
  `~/.local/share/applications/pantheon-desktop.desktop` + the hicolor icon,
  so Pantheon appears in app pickers. Packaging later = `tauri build`
  (`.deb`/AppImage) which registers system-wide; the verb registration is
  the no-root path.
- The historical fork checkout (`~/projects/pantheon-desktop`, with its
  upstream remote) stays as the upstream-sync source; `desktop/` in the
  runtime repo is canonical.

### Visual verification caught a real launch bug (2026-10-25)

The first "verified" launch was verified by *process*, not pixels. A
screenshot showed WebKit's "Could not connect to 127.0.0.1: Connection
refused" and the upstream panda icon. Root cause: `tauri build` enables the
`custom-protocol` feature (embeds `frontendDist`); a plain
`cargo build --release` does not — the release binary baked the devUrl and
the webview had nothing to load. Fixes (commit `44d1a97`): explicit
`[features] custom-protocol` in `desktop/src-tauri/Cargo.toml`, the build
verb passes `--features custom-protocol`, Pantheon favicon
(`public/icon.svg` was a base64 panda PNG), bundle icon list → `icon.png`.
Post-fix screenshot confirms: purple P mark, Pantheon titlebar/brand, full
sidebar, "Choose a project" start view. Rule: **UI work on this app ends
with a screenshot, never a process check.** Still upstream DNA by choice:
wallpaper art, panda bot avatars, `kingcode.sqlite3` data-dir filename.
