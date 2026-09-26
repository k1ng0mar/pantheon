# Gap map: setup redesign spec vs. repo

Three inputs, reconciled:

- **SPEC** — what the setup redesign and slash-command sections say Pantheon becomes
- **REPO** — `~/projects/pantheon` at `6fa3dd5`, 19 crates, ~59.5k LOC
- **EVAL** — `eval/run.py` + `eval/cases.json`, 17 cases, 3 of which invoke `setup`

Classification per spec item: **EXISTS** / **PARTIAL** / **MISSING** / **DEAD** /
**PLACEHOLDER** / **SPEC GAP**.

Evidence below is file:symbol, read from source on 2026-09-26. Not inferred from docs.

---

## 0. The load-bearing finding

There is no shared component layer, and there are three unrelated implementations of
"let the user pick something":

| Implementation | Location | Technique |
|---|---|---|
| `model_cli::pick` | `crates/pantheon-cli/src/model_cli.rs:88` | raw mode + `println!` + manual clear |
| `pick_model` | `crates/pantheon-cli/src/main.rs:423` | alternate screen + `read_line`, filter-by-typing |
| `pick_run` | `crates/pantheon-cli/src/session_cli.rs:83` | alternate screen + `read_line`, filter-by-typing |
| TUI overlays | `crates/pantheon-cli/src/tui.rs:435` (`render_history`) | ratatui, type-to-filter |
| setup prompts | `crates/pantheon-cli/src/setup_cli.rs:15` (`prompt`/`confirm`) | plain stdin lines |

`pick_model` and `model_cli::pick` are two model pickers in the same binary. `pick_run`
and the TUI history overlay are two run pickers. The spec's rule — one component, many
surfaces — is violated at every one of these.

**Classification: MISSING.** This blocks sections 1 through 20 of the spec, both slash
command sections, and any future `/models` browser.

---

## 1. Entry (Quick / Full / Blank Slate)

**MISSING.** `setup_entry.rs:6` → `run_setup`, which runs a fixed 5-step prompt sequence
(`setup_cli.rs:60`) with no mode selection. Quick/Full/Blank Slate require a section graph
before any screen can be conditionally skipped.

## 2. Profile (New / Built-in, SOUL.md + AGENT.md)

**PARTIAL, and the existing half is a PLACEHOLDER.**

- `config_doc::AgentIdentity` (`:177`) has `display_name`, `soul_file`, `memory_namespace`, `policy`.
- Production readers: **none.** The only consumer outside config is
  `doctor_cli.rs:100`, which prints the namespace. Confirmed by grep for `soul_file`
  across all crates — hits only in `config_doc.rs` and its tests.
- `Session::new` sets `system_prompt: String::new()` (`session.rs:433`) and nothing in
  the workspace ever assigns it. `assemble_turn` (`:1610`) only emits a system row
  `if !system_prompt.is_empty()`. So even a hand-edited `system_prompt` field would be
  silently dropped — there is no setter, and the struct is constructed only in `new`.
- `memory_namespace: "nyx".into()` is hardcoded (`session.rs:435`). `[agents.*].memory_namespace`
  reaches nothing.
- No built-in profiles exist. `crates/pantheon-exec/bundled-skills/` holds one entry
  (`design-references`), which is a skill, not an agent profile.

**Verdict:** the config table is a schema for a subsystem that was never written. The
spec is right that this is a real gap; the table is currently decoration.

## 3. Model provider browser

**PARTIAL.** Catalog coverage is better than a first pass suggests — 40 providers
including `nous`, `novita`, `openrouter`, `fireworks`, `lmstudio`, `local`, `router`,
`openai`, `anthropic`. All the spec's named model providers exist.

Two real limitations:

- **Only 19 curated model rows exist across all 40 providers.** `nous`, `novita`,
  `lmstudio` and most others ship `models: []`. The provider→model list the spec's
  screen 5 renders is empty for them; only the live `GET {base}/models` path
  (`model_cli::fetch_models:340`) fills them, and that needs a working key first.
- `main.rs:423 pick_model` and `model_cli::pick` are two pickers. Neither is reachable
  from the TUI.

**Judgement on catalog gaps:** none of the 40 providers need new work. They are all
OpenAI- or Anthropic-shaped endpoints already handled by `openai.rs` / `anthropic.rs`.
What is missing is *curated model rows* for the empty providers, which is a data task,
not an integration task. **Not an integration.**

## 4. Authentication

**PARTIAL.** Key prompt, comma-stacking, `.env` write at mode 0600 all exist
(`model_cli.rs`, `dotenv.rs`). Two spec behaviors absent:

- **No key validation step.** The spec shows `✓ Key accepted`. There is no probe;
  `fetch_models` doubles as an implicit one, but it is framed as model listing.
- **No OAuth at all.** Grep for `oauth|device_code|authorization_url` across all
  crates: two hits, both in unrelated test files (`pantheon-migrate/src/index_tests.rs`,
  `pantheon-storage/src/search_tests.rs`). Zero implementation.

**Judgement:** OAuth is a genuine new subsystem, not an integration. It plugs into the
existing `SecretsBroker` / `.env` boundary (a callback-driven device flow writes the
resulting token through `dotenv::persist_dotenv_value`). It is core to the target (the
spec lists it as a provider class), so: **stage behind an `AuthFlow` trait, implement the
OAuth adapter after the component layer exists.** Do not list OAuth providers in setup
until that adapter is real — otherwise it is fake configuration.

## 5. Default model

**EXISTS**, on a non-shared picker. `model_cli.rs:924` filters and selects from the
catalog or a live fetch. Consolidating onto the shared component is step 2 of the plan.

## 6. Reasoning effort

**MISSING.** Traced end to end and it does not exist at any hop:

- No `reasoning_effort` / `ReasoningEffort` identifier anywhere in `pantheon-core` or
  `pantheon-providers`.
- `ModelMeta.reasoning` (`catalog.rs:97`) is a bool. Its **only** consumers are
  `crates/pantheon-core/src/catalog_tests.rs:17,22,29`. Nothing in production reads it.
- `openai::build_body` (`openai.rs:62`) emits `{"model", "messages"}` plus tools. No
  effort, no `reasoning_effort`, no `thinking` block config.
- The streaming parser *reads* `reasoning_content` / `reasoning` / `thinking`
  (`openai.rs:148,278`) so responses are handled, but nothing requests it.

**Consequence:** the spec's screen 6 would write a config field that changes nothing.
12 catalog models are flagged `reasoning: true` and would be offered a selector that
does nothing. This is the clearest instance of the fake-configuration hazard.

**Required path:** config field → `DefaultModel` → `ProviderChain::attempt`
(`chain.rs:57`) → `build_body`. Effort also has to be *per-provider shaped*
(OpenAI uses `reasoning_effort`, Anthropic uses a `thinking.budget_tokens`), and a
value the model rejects is a hard 400, not a warning. The six levels in the spec are a
target, not a fixed set.

## 7. Workspace

**MISSING.** No workspace concept in config or runtime. The session implicitly uses
`std::env::current_dir()` in three places:

- `session.rs:742` — skill discovery project root
- `session.rs:783` — plugin discovery project root
- `model_cli.rs` — provider registry project root

There is no scoping: `read_file` takes an absolute path with no workspace containment
(`builtins.rs`, `Capability::FilesystemRead` only). "Where should your agent work" cannot
be answered truthfully, and the `Reader`/`Developer` distinction does not currently
constrain *where* writes may land.

## 8. Execution environment (Local / Docker, network policy)

**PARTIAL, with the container path MISSING.**

- `ExecutionBoundary::Container` exists (`sandbox/level.rs:23`) and `SandboxLevel::High`
  maps to it. `SandboxProfile.network: bool` exists (`level.rs:59`).
- `run_sandboxed` (`sandbox/runner.rs:369`) never builds a container. It builds a
  `bwrap`/namespace wrapper, probes `wrapper_initializes`, and on failure falls back to a
  direct spawn with rlimits. There is no `docker`/`podman` invocation anywhere in the crate.
- On this host the bwrap probe fails (`setting up uid map: Permission denied`), so
  `shell` runs unisolated today with a visible prefix in the tool result.

**Judgement:** the abstraction is already correct — `ExecutionBoundary` is the seam, and
a `Container` backend is one new `ExecutionBoundary` implementation reading
`profile.network`. That is a clear path into an existing abstraction, so it is core
architecture, not speculative. **Implement.** Note that `profile.network` is currently
consulted only by the bwrap wrapper, so the "Block egress" choice must reach the backend
that actually enforces it.

## 9. Agent permissions

**PARTIAL, closest section to the spec.** `PolicyPreset` (`config_schema.rs:61`) has
exactly three: `reader`, `coder`, `coder_memory`. The spec's four are `Reader`,
`Developer`, `Developer + Memory`, `Advanced policy`. First three map 1:1 onto
`reader`/`coder`/`coder_memory`. `Advanced policy →` has no surface at all.

The presets resolve correctly through `to_policy()` (`:97`) and every entry point uses
`policy_for_config` (`:110`), so the spec's "separate from the execution backend" is
already true: `Policy` and `SandboxLevel` are independent.

## 10. Gateways

**PARTIAL.** Telegram and Discord are real, with allowlist enforcement and a token
preflight (`gateway_cli.rs:196`).

- **Slack does not exist.** Grep for `slack` across crates: 5 files, all incidental
  (`channel.rs` doc text, `context.rs`, `carry.rs`, `session_tests.rs`). No adapter.
- No picker. `cmd_gateway` (`:171`) takes `run|start|stop|restart|status` and
  `preflight_or_exit` checks *both* env vars, so there is no "configure only what you
  selected" step.
- No token validation.

**Judgement:** the `Channel` seam (`gateway/channel.rs`) is exactly where Slack plugs
in — a third adapter behind an existing trait. But the spec lists Slack as one option
among several and it is the only one with no code. **Do not list Slack in setup until
the adapter exists.** Telegram and Discord are truthful today.

## 11. Tool groups — the gating layer

**MISSING. This is the largest single item in the spec.**

Current state, verified:

- `Session::chat_turn` (`session.rs:716-775`) calls `register_builtins_with`,
  `register_safewrite`, `register_skill_tools`, `register_session_search`,
  `register_memory_tools`, and registers every discovered plugin — **unconditionally**.
- `Config.tools` (`config_doc.rs:279`) is `Option<toml::Value>` and its own doc comment
  says: *"The keys are inert: tool registration is unconditional and nothing reads
  them."*
- ~30 tools register with no group concept anywhere: `shell`, `read_file`, `write_file`,
  `list_dir`, `preview_file`, the 9 safewrite tools, 5 memory tools, 2 skill tools,
  `session_search`, 5 vault tools, plus plugin tools.
- The catalog the spec's screen 11 lists (Web, Terminal, Files, Memory, Planning, Image
  Generation, TTS, Vision, MCP) names **five groups that have zero tools**:
  Web, Image Generation, TTS, Vision, MCP.

So the spec's tool screen would render checkboxes for capabilities that cannot be
enabled because there is nothing to enable. Selecting "Web" and answering "configure
provider for Web" would configure a provider for a tool that does not exist.

**What building it requires:**

1. A group taxonomy mapped to `Capability` (which already exists and is the right
   granularity: `FilesystemRead/Write`, `ShellExecute`, `NetworkOutbound`, `Browser`,
   `MemoryRead/Write`, `AgentSpawn`).
2. A `ToolGroups` selection carried into `Session`, filtered at the
   `register_*` call sites in `session.rs`.
3. `Config.tools` becomes a typed `Option<ToolGroups>` instead of inert TOML. Back-compat:
   keep parsing the old shape, ignore unknown keys.
4. **Group availability must be honest.** A group with no registered tools must render
   as unavailable in setup, or be omitted — not as a checkbox that writes a setting.

Step 4 is the part that makes this not fake configuration. `Foundation ≠ optional`
applies here directly.

## 12. Browser provider

**MISSING, entirely.** Grep for `web_search|browse|duckduckgo|tavily|firecrawl` across
all crates: **zero hits.** No browser tool, no `Capability::Browser` consumer (the
variant exists in `capability.rs:11` and is used only for sandbox-level mapping),
no Playwright/Chromium/headless dependency, no HTTP fetch tool.

The spec lists eight browser backends (Local, Lightpanda, Camofox, Browser Use,
Browserbase, Firecrawl, Skip). None exist. `NetworkOutbound` has no tool that uses it.

**Judgement:** a `ToolProvider` trait is the obvious seam and the spec already names the
component. But this is a new tool family plus eight vendor integrations, and nothing in
the eval or the rest of the spec depends on it. **Stage behind the `ToolProvider` trait
once groups exist; do not build the eight integrations now.** Setup must show Browser as
unavailable until at least the local provider is real.

## 13. Web search provider

**MISSING, entirely.** Same grep as above: zero hits. Spec lists DuckDuckGo, Brave, Exa,
Firecrawl, Tavily, SearXNG.

**Judgement:** six vendors, six API shapes, and no consumer tool. Same call as browser:
trait now, integrations later, unavailable in setup until real.

## 14. TTS

**PARTIAL, and the failure mode is exactly the one to avoid.** The *provider plane* is
built: `providers/voice.rs` has `TtsRequest`, `speech_payload`, a backend registry
(`command` | `openai`), and `Config.tts: Option<VoiceSection>` (`config_doc.rs:270`).

**But there is no TTS tool.** Grep for `tts|synthesiz|speech` in `pantheon-exec`: zero
hits. `register_*` in the session never registers one.

So today: setup could configure TTS successfully, the config would validate, `doctor`
would not complain, and the agent could still never speak. That is a working example of
the fake-configuration trap already latent in the repo, and a direct argument for
building the tool before the setup screen.

## 15. Memory backend

**PARTIAL, with one PLACEHOLDER that matters.**

What is real: `BackendRegistry::with_plugins` (`backend.rs:276`) registers `native`
(SQLite + FTS5), `http`, and a plugin bridge for `galaxymem`, `mnemosyne`, `honcho`,
`hindsight` (`PLUGIN_BACKENDS:152`). `pantheon memory backend select` works.

**The gap:** `Session::new` hardcodes
`MemoryStore::open(&data_dir.join("memory.db"))` (`session.rs:407`). `open_selected` —
the function that honors the user's selection — is called **only** from the four
`pantheon memory` CLI verbs in `main.rs:800,833,856,879`. The agent loop never sees it.

So `pantheon memory backend select galaxymem` succeeds, `doctor` passes, and the
session keeps writing to the native SQLite store. The selection is cosmetic for the
agent. **This is a PLACEHOLDER and the single most misleading thing in the memory
section.**

`mem0` is not in `PLUGIN_BACKENDS` and has no code. GalaxyMem, by contrast, is a
registered bridge and is genuinely selectable once the session honors it.

## 16. Skills / MCP

**PARTIAL.**

- **Skills: EXITS end to end.** `discover_skills_ext` (`skills.rs:364`) scans
  pantheon + project + Hermes/OpenClaw/.agents/.claude roots plus
  `PANTHEON_SKILLS_DIR`, seeds bundled skills, and registers `skills_list` +
  `skill_read` gated on `FilesystemRead`. Real, tested, reachable from a chat turn.
- **MCP: projection only.** `pantheon-mcp` exports `capability_from_token` and
  `project`. No client, no server launch, no tool discovery or invocation. Confirmed:
  no `ClientSession`/launch path in the crate. The spec's "never force MCP during first
  boot" is the right call and the honest one.

## 17. Fallback

**EXISTS.** `fallback_cli.rs` (add/list/remove by index), `[model].fallbacks`, honored by
`ProviderChain` (`chain.rs`), and `ModelEvent::Fallback` is rendered by the TUI as a
routing line (`tui.rs:213`). The spec's fallback screen reuses existing components with
no new architecture.

## 18-20. Review / Provisioning / Finished

**MISSING.** No section-graph, no review summary, no provisioner with retry/skip/abort,
no finished screen. The provisioner's failure policy is the substantive part: a per-step
result enum with `Retry | Skip | Abort`, where `Skip` records a durable degraded-capability
fact rather than silently continuing.

## 21. Dynamic progress

**MISSING.** No progress indicator exists in either surface. Straightforward once the
section graph from item 1 exists — the count must be computed from the resolved branch,
never a constant.

---

# Slash commands and recommendations

## Typing `/` opens a palette

**MISSING.** Both surfaces parse the whole line on Enter and dispatch
(`tui.rs:1092`, `session_cli.rs:644`). Nothing renders while typing.

## Command divergence (the consolidation target)

The TUI and the REPL implement **different, overlapping** command sets:

| Command | TUI | REPL |
|---|---|---|
| `/help` `/exit` `/quit` `/clear` `/history` `/name` `/resume` `/runs` `/status` | yes | yes |
| `/cost` | yes | **no** |
| `/new` | **no** | yes |
| `/memory` `/remember` `/policy` `/model` | **no** | yes |

Neither is a superset. Two hand-maintained `handle_slash` (`tui.rs:1154`) and `command`
(`session_cli.rs:174`) implementations, ~200 and ~250 lines, answering the same intent
differently (`/runs` in the TUI prints glyphs, in the REPL a numbered table).

**Classification: DEAD (duplicated).** One registry, one implementation, both surfaces
dispatch through it. This is the single highest-value cleanup in the whole audit and it
is a prerequisite for the palette.

## `/history` overlay

**PARTIAL, and it is the one working precedent.** `tui.rs:435` implements
type-to-filter, Up/Down, Enter-to-resume over `filtered_history` (`tui.rs:159`). It is
also the only real instance of the spec's `/resume` argument-autocomplete behavior.

## Runtime-state recommendations

**MISSING.** No recommendation layer. The *data* it needs already exists and is
maintained: `doctor_cli::run_system_doctor` produces typed `Check { section, status,
detail, fix }` rows (`doctor_cli.rs:13`); `Config::validate()` returns per-field problems
(`config_doc.rs`); `ledger_list_runs`; `discover_skills_ext`; `catalog` provider state;
`BackendRegistry::list()`.

The spec's key constraint — deterministic, cheap, no LLM per keystroke — is satisfiable
with zero new infrastructure. This is the cheapest high-value item in the audit.

## Keyboard behavior

**PARTIAL.** Existing: double-Esc interrupt with a 1.5s arm window (`press_esc`,
`tui.rs:339`), `y`/`n` on the permission card, `q` to quit. Missing: Tab (accept
suggestion), Ctrl+Space, Ctrl+K, Ctrl+L, Ctrl+D.

Note that Tab must not be captured when the suggestion popup is closed, or it becomes
untypeable in the composer.

---

# Cross-cutting findings

## Catalog display lies in the TUI

`run_tui_session` hardcodes the model as `"opus-4.1"` and the context window as
`200_000` (`tui.rs:874-877`). The header and status bar render those values
unconditionally, so a session on `local/llama3.2` displays `opus-4.1` and a context
budget it does not have. `run_tui_session` never reads the configured model policy.

**PLACEHOLDER.** The TUI was built before the model became configurable and was never
reconnected. `/cost` in the TUI inherits the same fiction.

## `fit_to_window` / `compress_oldest` are orphans

Already recorded in `ARCHITECTURE.md:467`, restating because it interacts with this
spec: the spec's `/models` browser is supposed to show per-model context windows, and
`ModelMeta.context_limit` has no production consumer. A user can be shown a context
budget that is never enforced.

## `Config.tools` as inert TOML

`config_doc.rs:279`. Retained deliberately for back-compat parsing, and the comment is
honest about it. Under the spec this becomes a typed field. Keeping the comment accurate
is a small, separate correctness item: the current text is accurate, but it will become
false the moment anyone assumes the table works.

## `pantheon-otel`

No consumer, no OpenTelemetry dependency, no exporter (`ARCHITECTURE.md:470`). Not
mentioned in the spec. Flagged as out of scope for this work, not as a target-state
item.

---

# Dependency order

Steps 1-11 as given, with one correction that the ordering forces.

```
L0  ui/ component layer            Select, MultiSelect, TextInput, Keymap, Dialog
                                    (ratatui when TTY, line fallback when not)
L1  migrate model_cli::pick        onto L0; delete pick_model and pick_run
L2  command registry               one registry; TUI + REPL dispatch through it.
                                    Deletes ~450 duplicated lines.
L3  config schema (new fields)     workspace, execution boundary, network policy,
                                    reasoning effort, tool groups, agent identity.
                                    Typed, with old-shape back-compat.
L4  runtime consumers              one per field, same commit as L3:
                                    - effort -> DefaultModel -> chain.rs -> build_body
                                    - workspace -> cwd + path containment
                                    - tool groups -> registration filter in session.rs
                                    - agent identity -> system_prompt + namespace
                                    - execution -> Container boundary impl
                                    - memory -> session uses open_selected
L5  setup orchestrator             section graph over L0 components, conditional
                                    branches, dynamic count, provisioner.
                                    Each screen enabled only when its L4 consumer
                                    is live.
L6  capability/tool-group gating   taxonomy -> Capability, filter at register sites
L7  provider integrations          tool providers (web, browser, TTS tool) behind
                                    ToolProvider; auth flows behind AuthFlow
L8  slash palette                  / completion, categories, Ctrl+K
L9  runtime-state recommendations  over doctor checks + config validation
```

### Why L3 and L4 cannot be separated

The plan's step 3 is "build the setup orchestrator" and step 4 is "implement the runtime
architecture required by setup." Run in that order, step 3 writes fields that step 4 has
not yet made functional — which is precisely the fake-configuration failure the audit
exists to prevent. Reasoning effort, tool groups, workspace, and execution backend all
have this shape.

**Recommendation:** build the orchestrator structurally, but gate each screen on its
consumer landing in the same commit. Reasoning effort is the cheapest and should go
first (config → `DefaultModel` → `build_body`, plus per-provider shaping) because it is
the clearest demonstration that the rule is being honored. Tool groups are the most
expensive and should follow the capability taxonomy.

---

# Per-integration verdicts

For each missing integration: is it core architecture, does an abstraction exist, is
there a clear path, does anything require it, and when.

| Integration | Core? | Abstraction exists? | Path | Required by | Verdict |
|---|---|---|---|---|---|
| Component layer | yes | no | build it | every spec screen | **now** |
| Command registry | yes | no | build it | palette, both surfaces | **now** |
| Reasoning effort | yes | `DefaultModel` → `build_body` | direct, per-provider shaping | setup screen 6 | **now** |
| Tool groups | yes | `Capability` | taxonomy + filter at `register_*` | setup screen 11 | **now** (after taxonomy) |
| Workspace | yes | none | new; needs path containment | setup screen 7 | **now** |
| Docker/container exec | yes | `ExecutionBoundary::Container` | one new boundary impl | setup screen 8 | **now** |
| Agent identity (SOUL/AGENT) | yes | `AgentIdentity` + `system_prompt` field | add a setter, read files, use namespace | setup screen 2 | **now** |
| Memory selection reaches session | yes | `open_selected` exists | call it in `Session::new` | setup screen 15 | **now** (one-line-class fix, removes a PLACEHOLDER) |
| TTS tool | yes | `voice.rs` provider plane done | add the tool | setup screen 14 | tool now, providers after |
| Web search | yes | none | `ToolProvider` trait + vendors | setup screen 13 | trait now, integrations later, **unavailable in setup until real** |
| Browser | yes | `Capability::Browser` | same | setup screen 12 | same |
| OAuth | yes | `SecretsBroker` / `.env` | `AuthFlow` trait + device flow | setup screen 4 | trait now; **no OAuth provider listed until real** |
| Slack | no | `Channel` trait | one adapter | setup screen 10 | later; **do not list until real** |
| Nous/Novita/etc. model rows | n/a | `catalog.yaml` | data entry | picker completeness | later, data task |
| MCP client | yes | `pantheon-mcp` projection | real client | spec defers it | out of scope for this work |

The recurring rule: where an abstraction already exists (`ExecutionBoundary`,
`Channel`, `open_selected`, `DefaultModel`, `Capability`, `voice.rs`), the work is
integration and the judgement is easy. Where none exists (web search, browser), stage
behind a trait and mark the setup screen unavailable. Either way, **no setup screen ships
selecting something the runtime cannot do.**

---

# Eval impact

`eval/cases.json` has 17 cases; three invoke setup:

- `setup-writes-complete-config` — `setup --yes --profile eval --provider local --model llama3.2`
- `setup-reset-roundtrip`
- `config-invalid-toml-is-reported-loudly`

All three rely on `setup --yes` staying non-interactive and on `[model]`/`[profile]`
round-tripping through `config.toml`. The orchestrator must preserve the flag path
exactly; `--yes` becomes "accept defaults for the resolved branch", not "run the old
five steps".

`doctor-system-form-fails-without-config` depends on setup *not* writing a config on
failure. Provisioner `Skip` must not create a partial config that doctor then accepts.

No eval case covers reasoning effort, tool groups, workspace, or execution backend, so
those need new cases rather than being protected by existing ones.

---

# Summary counts

| Class | Count | Items |
|---|---|---|
| EXISTS | 2 | default model selection, fallback chain |
| PARTIAL | 9 | provider browser, auth, permissions, gateways, memory, skills/MCP, TTS, execution env, workspace-root handling |
| MISSING | 12 | component layer, entry modes, SOUL/AGENT readers, reasoning effort, workspace, container exec, tool groups, browser, web search, review, provisioning, progress |
| DEAD | 2 | duplicate pickers, duplicate command handlers, inert `[tools]` |
| PLACEHOLDER | 3 | memory backend selection ignored by session, TUI hardcoded model/context, agent identity table unread |
| SPEC GAP | 4 | effort levels are provider-shaped, not a fixed six; workspace implies path containment the spec never states; tool groups assume a taxonomy the spec names but does not define; gateway list includes Slack with no seam priority |
