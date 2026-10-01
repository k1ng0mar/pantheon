# Setup wizard flow

How the TUI setup wizard (`crates/pantheon-tui/src/setup_wizard.rs`) walks
the user to a config file. Source of truth is the code — this doc mirrors
`run_setup_flow` and `setup_graph::sections` as of the `chloe/tui-redesign`
rework (branch unpushed, nothing committed).

The wizard never writes config itself. It collects answers, then calls
`setup::run_setup(data_dir, answers, assume_defaults=true)` — the same path
`pantheon setup --yes` uses. A cancelled provider answer (`None`) therefore
resolves to the recommended default silently rather than aborting the commit.

## Screen 1: Setup / "Choose your setup"

Three modes; Quick is gone. Cancelling the first screen (Esc) resolves to
Recommended — the least presumptuous default.

| Option | Description | What happens |
|---|---|---|
| **Recommended** | "pick your provider and model; everything else gets recommended defaults" | Provider picker → model picker → recommended provider for each tool group, no picker screens, no Tools screen, no STT/TTS |
| **Full Setup** | "everything, skipping anything that does not apply" | Provider picker → model picker → Tools multi-select (all 15 pre-ticked) → provider screens only for the enabled tool groups |
| **Blank Slate** | "create the runtime without configuring an agent" | Provisions the runtime, writes a minimal config, stops — no provider, model, or tools questions |

## Screen 2: provider picker ("Model" / "Choose a model provider")

Same in Recommended and Full. Providers sorted recommended-first
(Nous Research leads, via the catalog marker with a defensive pin in the
wizard); rows carry "recommended" tags and blurbs. A **Custom** row takes a
base URL for an OpenAI-shaped endpoint and registers it through the same
path `pantheon provider add` uses, so it is a real catalog entry.

## Screen 3: model picker ("Model" / provider label)

For OpenAI-compatible providers the screen live-fetches `{base}/models`
(`model_catalog::fetch_live_models`) and merges it with the curated
catalog entries (`merge_model_rows`) — curated wins on metadata: a live
id that already has a curated entry is skipped, so the catalog's context
limits and pricing survive the merge. Rows show per-model pricing as
`in $X/M · out $Y/M` (context limit first: `70k · in $0.20/M · out
$0.60/M`); zero-priced models are tagged `free`; no tag appears where no
pricing exists — prices are never invented.

Key handling: the API key is collected after the model screen, so the
fetch is tried keyless first (no auth header is sent). A 401/403 falls
back to curated entries with a "full list after API key" note in the
subtitle; other failures show `live list unavailable (…)` instead.
Non-OpenAI providers use the curated list only.

Manual model-name entry remains the last resort when a provider has
neither curated nor fetchable models. Cancelling here writes nothing —
the flow returns without committing.

## Recommended: the "straight to keys" path

After provider+model, `run_recommended` resolves each group with
`complete_recommended`: no picker screen, just the kind-driven follow-ups.
The done summary then prints each configured id (or `skipped` /
"recommended default" for cancelled follow-ups) and `pantheon: voice: off`.

The recommended toolset is every group except Voice:

| Tool group | Recommended provider | What the setup screen asks |
|---|---|---|
| Web Search | TinyFish | nothing — keyless |
| Browser | GSD | detect `gsd-browser`; when missing: dependency notes, then install-or-skip |
| Memory | Pantheon Native | nothing — keyless |
| Computer Use | CUA driver | detect `cua-driver`; when missing: dependency notes, then install-or-skip |
| Terminal, Files, Skills, Tasks, Delegation, Ask User, Vault, Vision, Video Analysis | built-in | nothing — no provider screen exists |
| Voice (STT + TTS) | off | skipped entirely — not asked |

### Computer use in Recommended: the explicit decision

The CUA driver **is** included in Recommended (it is the documented
recommended driver), resolved through detect/install-or-skip. On a headless
machine the install is skipped gracefully: the answer records
`skipped=true`, the config still records the choice, and `pantheon doctor`
reports the gap instead of pretending the capability works. Rationale:
install-heavy but skip-safe beats silently omitting a flagship capability —
the user learns the option exists and gets a truthful health check.

### Skipping a provider screen

Every provider screen (browser, web search, STT, TTS, memory, computer
use) carries an explicit **Skip** row at the bottom. Skip is not Esc:

- **Skip** records the tool as intentionally unconfigured — the tool group
  is dropped from the enabled set, no provider section is written, and the
  runtime never registers it. The done summary lists the skipped tools.
- **Esc** keeps its prior meaning: fall back to the recommended default
  downstream (the shared setup path resolves it when the answer is `None`).

In Recommended mode there is no picker, so the same skip is offered in
the follow-ups: `complete_recommended` asks a "Set up <name> now?"
confirm before running the kind-driven questions. Keyless providers have
nothing to ask and complete directly. STT/TTS skips are granular per
backend — skipping STT does not skip TTS.

> Status note (2026-09-29): the Skip row and the "Set up <name> now?"
> confirm have landed (`SKIP_VALUE` / `skip_item` in `setup_providers`,
> `apply_pick` / `apply_voice_picks` map Skip to group removal) — the
> paragraph above describes current behavior, verified against the code.

## Full Setup

The Tools screen replaces the old Permissions screen: one multi-select over
the 15 tool groups (session search stays on and is not listed; Esc keeps
every group on; a confirmed empty selection turns everything off). Policy
stays the coder preset in all modes — there is no permissions screen.

Provider screens are tool-gated and use the full picker (`pick_provider`):
Browser (browser group on), Web search (web search on), STT then TTS
(voice on), Memory (memory on), Computer use (computer use on), Extensions
(plugins group on — see below). After that, the fallback provider/model
screens run when the Fallback section applies.

### Screen: Extensions (Full only)

Gated on `answers.extensions_enabled`, which the wizard derives from the
Tools screen (`ToolGroup::Plugins`). The screen has two halves:

**MCP servers.** A loop: "Add an MCP server" collects a server name
(becomes `[mcp.servers.<name>]`), a transport (`stdio` / `sse` / `http`
streamable), then either command+args (stdio) or the endpoint URL
(sse/http), then optional env vars as comma-separated `KEY=value` pairs.
"Done" continues with the servers added so far; Esc keeps what was added
and continues (no recommended default exists to fall back to).

Validation is shape-only, never a live handshake:

- the config's own `McpServerEntry::problems()` (so the wizard can never
  disagree with config validation),
- a stdio command must resolve on PATH (or be an executable path),
- an sse/http URL must be an `http(s)` URL,
- server names are restricted to letters, digits, `-`, `_` (they become
  TOML table keys).

A failed check prints the problems and the server is not recorded — fix
it or pick Skip. Recorded servers are verified later: `pantheon doctor`
runs config validation over `[mcp.servers.<name>]`, and the MCP manager
reports connection failures at session start. The wizard never opens a
connection during setup.

**Secrets.** A value written as `env:NAME` (e.g.
`GITHUB_TOKEN=env:GITHUB_TOKEN`) is preserved verbatim in the config;
the MCP manager resolves it from the operator's environment at spawn
time, so the secret itself never lands in `config.toml`. The wizard
never asks for secret values — only the `env:` reference.

**Bundled plugins.** The screen prints the bundled-plugin status from
`<data_dir>/extensions/bundled/` (dirs containing a `plugin.yaml`).
Bundled plugins load without approval and there is no enable/disable
switch for them in `pantheon-extensions` — removing the directory is
the disable path — so the screen reports what is there and moves on.
Nothing ships there today, and the screen says "no bundled plugins
installed" instead of inventing a registry.

**Skip.** Same semantics as every other provider screen: Skip is an
explicit "leave extensions unconfigured" — the Plugins group comes off
the enabled set (recorded in the done summary), no `[mcp]` section is
written, and any servers added earlier on this screen are discarded.
Esc is not Skip: Esc keeps the servers added so far.

**Catalog seam.** The bundled MCP catalog (`pantheon-mcp::bundled`,
sibling workstream) feeds the same pipeline: a catalog row becomes
`(name, recipe.to_config_entry(true))` — secrets already as `env:NAME`
placeholders — and lands in the same `[mcp.servers.<name>]` tables with
the same `enabled` flags the dashboard/app toggles use. The config
document is the only enablement state; the wizard keeps no parallel
record. Catalog recipes are curated, so they bypass the hand-typed
flow's PATH gate and rely on `pantheon doctor` for launch verification,
same as everything else recorded here.

The section graph (`setup_graph::sections`) also enumerates
Reasoning/Workspace/Execution/DockerNetwork/Gateways/Extensions/Review/
Provision/Done for Full, but the wizard's Full path today only shows
Tools → provider screens → fallback; the rest have no screens behind them
yet (e.g. workspace and execution come from the `data_dir` argument, not
questions). The step indicator counts the resolved branch, never a
hardcoded total.

## What commit() writes per mode

`commit` → `run_setup` → `<data_dir>/config.toml`, plus
`memory-backend.toml` (the selection file the runtime instantiates from) and
the model API key into `<data_dir>/.env` (env var name only in the TOML —
the wizard never handles secrets).

- **Blank Slate:** minimal config — provider/model/policy/tools all `None`;
  no `[model]`, no `[tools]`, no provider sections. Server section only.
- **Recommended:** `[model]` (provider, model), policy = coder preset,
  `[tools]` with `voice = false` only — only deviations from the all-on
  default are written — `[websearch]` (TinyFish, enabled), `[browser]`
  (GSD), `[computer_use]` (CUA driver). No `[memory]` (native is the
  runtime default), no `[stt]`/`[tts]`.
- **Full:** same shape; `[tools]` records only the disabled groups, so an
  all-on run writes no `[tools]` at all; `[stt]`/`[tts]` only when Voice is
  on; `[memory]` only when the backend is non-native; `[mcp.servers.<name>]`
  tables for each MCP server added on the Extensions screen (nothing when
  none were added or the screen was skipped — skipping also writes
  `[tools] plugins = false`).

## Out of the wizard: auxiliary models

Auxiliary-model pins are intentionally **not** asked in the wizard. They
are managed anytime with `pantheon model`; the wizard's done summary
(`commit`) prints that hint so the user knows where to go.

## Invariant

Every visible screen writes something the runtime reads; no decorative
screens. Each section in the graph exists only when its runtime consumer
exists, and any screen that appears writes a config value (or an explicit
skip record `doctor` can report) — offering a choice the runtime cannot
honor is forbidden by design.
