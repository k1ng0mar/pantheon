# Computer Use for Pantheon — Research (2026-09-29)

All options verified against live sources on 2026-09-29 by four parallel research passes.
Pantheon is a Rust agent runtime on a personal machine (Linux/macOS/Windows).

## The layer split

Computer-use decomposes into three layers. Nearly every option below is exactly one of them —
confusing the layers is how projects end up wrapping a competing agent:

| Layer | Job | Examples |
|---|---|---|
| **Perception** | See the screen: screenshots, AX trees, element grounding | AX/UIA/AT-SPI APIs, OmniParser, UI-TARS grounding |
| **Reasoning** | Decide the next UI action | Anthropic computer toolset, OpenAI CUA model, UI-TARS |
| **Actuation** | Move mouse, type, click | Enigo, CUA Driver, xdotool/ydotool, portals |

---

## Perception — ranked

### 1. Native accessibility trees — ✅ viable (the primary)
Free, deterministic, exact element bounds, zero tokens. Always prefer over VLM guessing.

- **macOS:** `axuielement` 0.10 (github.com/doom-fish/axuielement-rs, updated days ago) — safe Rust over a Swift bridge: AXUIElement, AXObserver, attributes, actions. Needs Accessibility trust (TCC); permission changes require app relaunch. AX reads ~379µs/element — cache/scope walks.
- **Windows:** `uiautomation` 0.10.0 (leexgone/uiautomation-rs, updated ~24 days ago) — native IUIAutomation via COM: tree walkers, control patterns, events, plus optional `screenshot` and `input` (`send_keys`) features.
- **Linux:** `atspi` crate over D-Bus/zbus — enumerate apps, walk accessible trees, read roles/names/bounds. Coverage varies: GTK good, Qt needs `QT_ACCESSIBILITY=1`, Electron only with accessibility enabled. Used by `georgesriver/computer-use-linux`.
- Watch: **`xa11y`** (github.com/xa11y/xa11y) — young Rust workspace unifying all three OS a11y trees behind one `App`/`Element`/`Locator` API. Architecturally exactly right; unproven, monitor.

### 2. OmniParser v2.0 — ✅ viable with caveats (best screenshot parser)
Microsoft Research. Screenshot → structured UI elements (bboxes + captions). v2.0: 60% faster than v1, 39.5% ScreenSpot-Pro at release. Run as `omniparserserver` FastAPI sidecar: POST base64 PNG → elements; Pantheon does coord/DPI mapping in Rust (template: pi-nodriver-browser's `PI_NODRIVER_OMNIPARSER_URL` contract). Needs NVIDIA CUDA GPU (~1.5–2GB VRAM; hosted ~$0.011/call on Replicate). ⚠️ **License trap: `icon_detect` weights are AGPL-3.0** (inherited from YOLO, per Microsoft's own README). Defensible posture: local-only subprocess for a single-user agent; never bundle weights or ship as a network service. Pin the v2.0 checkpoint (YOLOv9-E weights still in an unmerged HF PR).
https://github.com/microsoft/OmniParser

### 3. UI-TARS grounding — ✅ viable with caveats (VLM fallback for unlabelled UIs)
`ByteDance-Seed/UI-TARS-1.5-7B` (Qwen2.5-VL based; 94.2% ScreenSpot-v2, 49.6% ScreenSpot-Pro per published tables). Run vLLM as OpenAI-compatible sidecar, call over HTTP from Rust; never embed weights. 7B needs ~16GB VRAM; CPU-only infeasible; Apple Silicon story weak. GGUF/quantized path officially downgraded by maintainers. ⚠️ Recheck the exact checkpoint's license before bundling (one audit flags "research license" language on early 1.5 releases). UI-TARS-2 (23B-active/230B MoE) exists but weights aren't public — teacher-only.
https://github.com/bytedance/UI-TARS · https://huggingface.co/ByteDance-Seed/UI-TARS-1.5-7B

### 4. OculiX (SikuliX successor) — ✅ viable with caveats (pixel-only fallback)
SikuliX was archived 2026-03-03; stewardship passed to Julien Mer as **OculiX** (oculix.org), v3.0.3, MIT, active. OpenCV template matching + OCR + actuation; ships an **MCP server module** (the clean Pantheon integration point: stdio/SSE). Niche: Citrix/RDP/VNC/Canvas where a11y trees don't reach. Template matching is inherently brittle (theme/DPI rot); JVM weight is wrong for the hot loop — narrow fallback only. Single-maintainer succession risk.
https://github.com/oculix-org/oculix

---

## Reasoning — ranked

### 1. Anthropic Computer Use — ✅ viable (best-documented; design template)
**GA since 2026-08-19, no beta header.** Messages API with `{"type": "computer_toolset_20260801"}`. 17 actions (screenshot, zoom, clicks, drag, mouse_move, scroll, type, key, hold_key, wait…). New dispatch shape: the action is the **name of the `tool_use` block** (`{"name":"left_click","toolset_name":"computer",…}`); every `tool_result` must echo `"toolset_name":"computer"`. Batched turns: execute in order, stop at first failure. Coordinates in screenshot pixel space (watch Retina 2x scaling); ~1,000–1,800 tokens/screenshot. Available on Fable 5/5.1, Mythos 5/5.1, Opus 5/5.5, Sonnet 5, Opus 4.8. Cost: standard model pricing (Sonnet 5: $2/$10 per 1M in/out, permanent). **Pantheon's internal action schema should be modeled on this protocol** — it's the best-specified, whichever model drives it. Also GA: `browser_toolset_20260801` (a11y-tree browser control) — evaluate separately for web-only tasks.
https://docs.anthropic.com/en/docs/build-with-claude/computer-use

### 2. OpenAI computer tool (GA) — ✅ viable with caveats
`computer-use-preview` is **legacy/deprecated**; GA shape is a minimal `computer` tool via the **Responses API only** (not Chat Completions), driven by computer-capable models (e.g. gpt-5.4). Loop: `computer_call` with `actions[]` → harness executes → screenshot as `computer_call_output`; stateful via PreviousResponseID. Two wire shapes coexist (singular `action` vs `actions[]`) — normalize both. `pending_safety_checks`/`acknowledged_safety_checks` approval plumbing is first-class — Pantheon needs an approvals surface. Pricing: preview was $1.50/$6.00 per 1M; GA = driving model's rate. **Operator (consumer product) is dead/subsumed** — don't design against it. Tier-gating may still apply; prompt-injection susceptibility explicitly warned.
https://developers.openai.com/api/docs/guides/tools-computer-use/

### 3. UI-TARS-1.5-7B as reasoner — ✅ viable with caveats (the open/local option)
Screenshot in → thought + UI action out, end-to-end VLM. Same deployment/license/compute caveats as above. Reusable `COMPUTER_USE`/`GROUNDING` prompt templates in the repo. No API-key vendor lock; GPU required.

---

## Actuation — ranked

### 1. Enigo 0.6.1 — ✅ viable (the default)
`enigo` on crates.io (~1.76M downloads, MIT). Windows (SendInput, DPI-aware), macOS (CGEvent), Linux **X11** solid. ⚠️ Wayland + libei are experimental/feature-gated ("because of bugs"); libei only on GNOME 46+. Coordinate trap: `Abs` is physical pixels on Windows/X11 but **logical points on macOS** — divide by backingScaleFactor on Retina or clicks land wrong. No release in ~10 months.
github.com/enigo-rs/enigo

### 2. Wayland probe chain — ✅ viable with caveats (the Linux reality)
No single input path covers Wayland. 2026 pattern (computer-use-linux, murmly): probe per session —
- **ashpd RemoteDesktop portal** (the sanctioned path): `ashpd` crate, `NotifyKeyboardKeysym`/`NotifyPointerMotionAbsolute`/etc. + Screenshot/ScreenCast in one session. Consent dialog first use; restore tokens after. Uneven backend support: GNOME/KDE implement RemoteDesktop; **portal-wlr and portal-hyprland do NOT**. Systemd-unit/app-id trap for daemons. The right long game.
- **wtype**: Wayland-native virtual-keyboard protocol; wlroots compositors only (Sway/Hyprland); **fails silently on GNOME/KWin**.
- **ydotool**: `/dev/uinput` kernel-level — works on ALL compositors + X11; needs `ydotoold` daemon + input-group/udev perms; raw evdev keycodes only (carry a keysym→keycode map).
- **xdotool**: X11-only; on Wayland exits 0 but events reach only XWayland clients — **silent no-op trap**. X11/XWayland fallback only.
Rule: probe each backend once per session, mark failures unusable, never trust exit codes.

### 3. CUA Driver (trycua) — ✅ viable (closest to a ready-made cross-platform driver)
MIT, ~18–21k stars, very active. `cua-driver`: click/type/scroll/drag/hotkeys, a11y-tree inspection, background screenshot capture, window listing — **no model inside** (the "body for an agent brain"). Killer feature: **no-foreground contract** — drives target windows in the background (CoreGraphics/AX on macOS, UIA on Windows, AT-SPI/X11-XWayland on Linux) without warping the user's cursor or stealing focus. Runs as **MCP stdio server**, daemon, or one-shot CLI — trivial from Rust, no bindings. Platforms: macOS + Windows GA; **Linux pre-release** (Wayland raw-input limitations). Use `cua-driver` only, not their Python agent SDK. Pin the binary version (fast-moving).
https://cua.ai/docs · github.com/trycua/cua

### 4. Clipboard path — ✅ viable (text-injection complement)
`arboard` (maintained, incl. rustdesk fork): set_text + Ctrl+V via enigo/ydotool. Wayland: `wayland-data-control` feature w/ X11 fallback; doesn't persist after process exit on Wayland; clobbers user clipboard (save/restore).

### 5. OpenAdapt — ✅ viable with caveats (governed execution, not free-roam driving)
Pivoted 2026: no longer "record demos, train models." Now **verified last-mile execution** — human records a demo → compiled into a deterministic local program → governed runtime executes with declared-effect verification (`VERIFIED`, `HALTED_BEFORE_EFFECT`, `RECONCILIATION_REQUIRED`); healthy runs make zero model calls. Their README: *"Computer-use agents are the user of OpenAdapt. They are not the executor inside it."* Substrate adapters: Playwright (browser), native desktop (UIA/AX/AT-SPI), RDP, Citrix/VDI. Rust integration: sidecar (`openadapt-agent serve --allow-run` or `openadapt flow` CLI), consume VERIFIED/halt receipts. Maps directly onto Pantheon's runtime-authority thesis and approvals model. MIT open core; cloud control plane commercial. Only compiled, human-admitted workflows — not general computer use.
https://github.com/OpenAdaptAI/OpenAdapt

### 6–8. The legacy/Node options — ⚠️/❌
- **PyAutoGUI — ❌ not viable.** Effectively unmaintained (0.9.54-era, 2024); Wayland issue open since **2016**; Xlib-only, black screenshots on Wayland. Everything it does is covered by native Rust crates with no Python tax.
- **RobotJS — ⚠️ revived but Node-only.** Surprise: shipped 0.8.0–0.9.1 (Jul–Aug 2026) with NAPI 3 rebuild and prebuilt x64/arm64 binaries. MIT. But Node-addon sidecar only, no Wayland — architecturally nonsense for a Rust runtime when enigo exists.
- **Nut.js — ⚠️ technically excellent, commercially awkward.** The 2024 subscription rug-pull stands: prebuilt npm packages pulled, **$20/mo Core / $75/mo Solo** (Solo adds the agent-relevant `nib` CLI: JSON protocol + a11y tree). Free only if you build from source. Node-only, no native Wayland.
- **rdev forks** (RustDesk's fork / `rdevin` / `handy-keys`): viable-with-caveats for global *listening* (hotkeys) + simulate; Linux listen is X11-only. Enigo is safer for pure injection.

---

## Not viable — competing agents / research demos (do not wrap)

| Option | Why not |
|---|---|
| **Microsoft UFO** (v1/v2/v3 Galaxy) | Research demo, full Python agent runtime (own loop, own sessions) — classic competing-agent inversion. UFO² is Windows-only (UIA/Win32/COM). Mine the papers for ideas (hybrid UIA+vision perception); integrate nothing. MIT. https://github.com/microsoft/UFO |
| **Agent TARS** (ByteDance) | Goal-driven full agent (browser+terminal+MCP+own loop); TS/npm; "Technical Preview." Study its event-stream protocol; don't depend on it. Apache 2.0. |

---

## Evaluation harness

**OSWorld — ✅ viable with caveats, as the test harness (not a component).** 369 tasks v1 (+Verified 361), **OSWorld 2.0** (Jun 2026): 108 long-horizon tasks, ~318 tool calls/task. Apache 2.0. Reusable: `desktop-env` observation/action contract (mirror in Rust), the task corpus + evaluators as Pantheon's computer-use regression suite (run Pantheon's perceive→reason→actuate loop in the Ubuntu qcow2), and the shared yardstick every vendor reports against. Heavy infra for full runs (VM fleet); `desktop-env` is Python — dev/test sidecar only, never in releases.
https://github.com/xlang-ai/osworld

---

## Recommended stack for Pantheon

| Layer | Choice | Integration |
|---|---|---|
| **Perception (primary)** | Native AX trees: `axuielement` (macOS) / `uiautomation` 0.10 (Windows) / `atspi` (Linux) | Rust crates, in-process |
| **Perception (fallback)** | OmniParser v2.0 sidecar for unlabelled/canvas UIs; OculiX MCP for pixel-only (Citrix/RDP) | Managed local subprocesses over HTTP/stdio; AGPL posture: local-only |
| **Reasoning** | Model-agnostic harness on Anthropic's GA toolset protocol; Anthropic first, OpenAI GA `computer` second, UI-TARS-1.5-7B local third | HTTPS from Rust |
| **Actuation** | Enigo (macOS/Windows/X11) + Wayland probe chain (ashpd portal → wtype → ydotool); arboard clipboard text path; CUA Driver as optional unified cross-platform driver | Rust crates + subprocess |
| **Governance** | OpenAdapt verified bundles for human-admitted workflows (receipts into the approvals model) | Sidecar |
| **Eval** | OSWorld task corpus as regression suite | Dev/test only |

Design rules: internal action schema mirrors Anthropic's 17-action toolset (batched, per-result toolset echo); never depend on a fixed model id (code against the `computer_call` shape); screenshots budgeted (~1–1.8k tokens each, ≤20 in context, prune for cache); consequential actions through Pantheon approvals (both vendors' safety-check plumbing expects it).

## Open questions for Umar
1. Primary platform? If Linux: which compositor (GNOME/KDE/Sway/Hyprland) — decides the Wayland actuation path and whether CUA Driver's pre-release Linux backend is worth testing.
2. Which model vendor account funds computer-use tokens — determines whether the Anthropic or OpenAI shape gets built first.
3. OmniParser AGPL posture: comfortable with local-only subprocess use, or avoid entirely?
