# L0: one terminal product

Product decision: **Pantheon's terminal interface is the TUI.** Not a TUI and a REPL
sharing a component library. One product, one entrypoint, one interaction model.

This document revises the gap map's dependency order. It answers questions 1-6 about
the existing interface code, and it states what L0 delivers and what it does not.

**The gap map's interface findings stand, reclassified.** Under this decision the
duplicated pickers and the duplicate command dispatch are not "components to share."
They are redundant interaction models to be consolidated *into* the TUI:

| Component | Class | Fate |
|---|---|---|
| TUI cockpit (`tui.rs`) | **EXISTS** | becomes the product; moves to `pantheon-tui/src/session_view.rs` |
| TUI history overlay (`tui.rs:435`) | **EXISTS** | first component built on `ui::SearchList` |
| `model_cli::pick:88` | **DEAD** (duplicate) | body → `ui::Select`; interactive path → `/models` |
| `pick_model` (main.rs:423) | **DEAD** (duplicate) | delete |
| `pick_run` (session_cli.rs:83) | **DEAD** (duplicate) | delete; `/sessions` replaces it |
| setup `prompt`/`confirm` (setup_cli.rs:15) | **DEAD** (superseded) | delete; `ui::TextInput` / `ui::Confirm` |
| `tui::handle_slash:1154` | **DEAD** (duplicate dispatch) | delete; one command registry |
| `session_cli::command:174` | **DEAD** (duplicate dispatch) | delete |
| REPL (`session_cli.rs` 707 lines) | **DEAD** as an interface | delete after `build_model_policy` relocates |
| `build_model_policy` | **EXISTS** (not UI) | relocate to `config_doc.rs` first |
| administrative verbs | **EXISTS** (non-interactive) | stay; see (6) |
| model/provider config flow (`cmd_model` interactive) | **PARTIAL** | relocate into `/models` + `/settings`; flags stay |
| 14 admin verbs *inside* the TUI (`/tools`, `/skills`…) | **MISSING** | build on TUI components over the same runtime calls |
| setup wizard screens | **MISSING** | L0 hosts the graph; screens land with their runtime consumers |
| command palette, `/` completion, Ctrl+K | **MISSING** | registry in L0, autocomplete after |
| `router` catalog row | **PLACEHOLDER** | keep for the e2e probe, hide from the picker with a `dev: true` flag |

---

## Product decisions taken from your direction

1. **`pantheon` with no arguments enters the TUI. Always.** No `is_terminal()` branch.
   No REPL fallback.
2. **Unconfigured install enters setup.** Running bare `pantheon` on an install with no
   `config.toml` opens the setup wizard inside the TUI, then transitions into a session
   once provisioning completes. This is finding 4, and it changes the entrypoint.
3. **`/cost` removed.** It reports `usage.cost_usd`, which the catalog populates for 19
   model rows. Not worth the header weight. The header keeps tokens and elapsed.
4. **The router is not a provider.** `router` in `catalog.yaml` points at
   `http://127.0.0.1:8015/v1`, your local llm-router. It exists so the dev loop and the
   e2e probe can reach a live model. It is not part of the Pantheon product and should
   not appear in the provider picker. I have been using it as a live test target, which
   is exactly why it leaked into the catalog.

**On (4) I want to agree with a caveat.** Removing it from the picker is right. Deleting
the catalog row outright breaks the e2e probe in the skill: `pantheon chat --provider
http://127.0.0.1:8015/v1 --model router` resolves through `catalog::base_url_for`, and
that path is what the live loop proof depends on. Correct shape: keep the row, add a
`dev: true` flag in the catalog so the picker filters it out while `base_url_for` still
resolves. Filtered, not deleted.

---

## Questions 1-6, answered from source

### 1. What functionality exists only in the REPL/CLI?

Fourteen verbs, unaffected by the TUI decision. Unaffected, that is: `audit`, `doctor`,
`extensions`, `fallback`, `gateway`, `hook`, `mcp`, `memory`, `migrate`, `model`,
`pipeline`, `plugins`, `provider`, `providers`, `repair`, `reset`, `run`, `schedule`,
`serve`, `setup`, `skills`, `swarm`, `audit`. These are automation and administration
surfaces, not a human-facing terminal UI, and they stay. See (6).

Human-facing interaction that exists only outside the TUI:

| Capability | Where it lives | Belongs in TUI? |
|---|---|---|
| model/provider selection | `model_cli::pick:88`, `pick_model` (main.rs:423) | **yes**, as `/models` `/settings` |
| run/session browsing | `pick_run` (session_cli.rs:83) | **yes**, `/sessions` `/resume` |
| base URL, wire mode, key entry | `model_cli` interactive flow | **yes**, `/settings` |
| model switching | `/model <provider> <model>` (session_cli.rs:352) | **yes**, `/model` |
| policy inspection | `/policy` (session_cli.rs:333) | **yes**, `/settings` |
| memory search + write | `/memory`, `/remember` (session_cli.rs:265,295) | **yes**, `/memory` |
| run lifecycle (`/new`) | session_cli.rs:180 | **yes**, `/new` |
| cost display | `/cost` (tui.rs:1180) | **no**, removed per your direction |

**`build_model_policy` (session_cli.rs:444) is the one casualty.** It is used by
`agui_cli.rs:24,82`, `schedule_cli.rs:458`, and `tui.rs:823`. Deleting `session_cli`
deletes it. It must move before the REPL goes, not after, or the AG-UI server and the
scheduler break. It is a config-resolution function, not an interaction: correct home is
`config_doc`, beside `auxiliaries` and `chat_secrets`.

### 2. Which of that belongs in the TUI?

All of it except `/cost`. Concretely, L0 gives the TUI the command registry that makes
this a relocation rather than a rewrite:

- `/models`, `/model` — provider/model browser and quick switch
- `/sessions`, `/resume`, `/new`, `/runs` — session family
- `/memory`, `/remember` — memory surface
- `/settings` — policy, execution, workspace, model config
- `/tools`, `/skills`, `/gateway`, `/doctor` — surface the existing verbs, but the
  *selection* happens through TUI components
- `/status`, `/name`, `/clear`, `/exit`

Every one dispatches through one registry. No second dispatch table exists.

### (3) and (5) are answered together below. (4) is the L0 scope. (6) is the boundary
definition.

### 3. What can be deleted after migration?

| Item | Fate | Notes |
|---|---|---|
| `session_cli.rs` (707 lines) | **delete** | after `build_model_policy` relocates. `run_session`, `run_session_inner`, `run_session_with_resume`, `Repl`, `command`, `pick_run`, `pick_model`'s sibling helpers all go |
| `session_cli_tests.rs` (67 lines) | **rewrite** | the `/name` durability test is real behavior, belongs in the registry |
| `main.rs:423 pick_model` | **delete** | 75 lines, alternate-screen picker with a second implementation of the same idea |
| `main.rs:534-551` `is_terminal()` branch | **delete** | replaced by the unconfigured-install check |
| `tui.rs:1180` `/cost` | **delete** | per your direction |
| `tui.rs:1154 handle_slash` | **delete** | replaced by the registry |
| `tui.rs:1040-1105` key handling for overlays | **rewrite** | moves into components |
| `model_cli::pick:88` | **replace** | body moves to `ui::Select`; the CLI keeps non-interactive flags |
| `model_cli.rs:1147 cmd_model` interactive path | **migrate** | becomes `/models` in the TUI; `--provider/--model/--key` flags stay for automation |
| `setup_cli.rs:15 prompt/confirm` | **delete** | replaced by `ui::TextInput` / `ui::Confirm` |
| `main.rs` `KNOWN_VERBS` + `classify_first_arg` | **keep** | see (6) |

Roughly 1,100 lines deleted, 450 duplicated lines relocated. The runtime crates are
untouched: no runtime test needs to move, because the runtime never had a UI.

**Not deleting:** the 14 administrative verbs. They are not a UI, they are not
interactive, and `eval/run.py` depends on nine of them.

---

### 4. What files/modules become unnecessary?

```
DELETE  crates/pantheon-cli/src/session_cli.rs
DELETE  crates/pantheon-cli/src/session_cli_tests.rs
DELETE  crates/pantheon-cli/src/setup_cli.rs      (the text wizard)
DELETE  crates/pantheon-cli/src/setup_entry.rs     (flag shim; flags move to the registry)
        → setup flags become TUI-preexisting answers, not a separate entry

MOVE    build_model_policy  session_cli.rs  →  config_doc.rs
ADD     crates/pantheon-cli/src/ui/
          mod.rs          component server, app state, screen stack
          widget.rs       Select, MultiSelect, TextInput, Confirm, SearchList
          overlay.rs      modal rendering, focus, capture
          picker.rs       ModelPicker, ProviderPicker, DirectoryPicker
        crates/pantheon-cli/src/commands.rs     the one command registry
        crates/pantheon-cli/src/setup.rs         section graph + provisioner
```

`setup_cli.rs` goes because the wizard is no longer a text program. The non-interactive
flag path survives as a `SetupAnswers` struct consumed by the same section graph, so
`setup --yes` still works for the eval. That is the same pattern the spec asks for: one
implementation, two entry conditions.

**`crates/pantheon-cli` should not own the TUI.** It is currently a 13,424-line binary
crate owning 20 modules, and it is the reason this consolidation takes a week instead of
of an hour. It should become a library crate (`pantheon-tui`) with a thin `main.rs`.

### 5. What tests need to move?

| Test | Now | Destination |
|---|---|---|
| `tui_tests.rs` (4) | render/estimate tests | stay; retarget `ui::widget` tests |
| `tui_interrupt_tests.rs` (6) | double-Esc state machine | **move to runtime** or keep. It is pure state logic on `TuiState` and touches no terminal. Pure logic should live in a testable place, not in a render struct |
| `tui_interrupt_tests.rs::resume_rebuilds_transcript_from_ledger` | ledger replay | **move to `pantheon-runtime`** where it belongs; it tests `rebuild_messages`, not the TUI |
| `session_cli_tests.rs::name_renames_the_conversation_durably` | `/name` behavior | **move to the command registry test**; the behavior is real and stays |
| `setup_cli_tests.rs` (33 lines) | wizard internals | **rewrite** against the section graph |
| `eval/cases.json` 3 setup cases | `--yes` path | keep; extend with new-branch cases |
| runtime crate tests | — | unchanged, zero work |

The rule: a test that only exercises a deleted interface dies with it. A test that
verifies ledger or state behavior moves to the crate that owns that state.

### 6. Which interfaces are genuinely non-interactive?

**These stay, and they are not a "terminal UI."** They are automation surfaces:

```
chat "msg"                 one-shot turn, no interaction
run --taskID --say         scripted run, delivery target
logs / audit               run inspection, pipeable
doctor                     machine-readable JSON, exit code
setup --yes --flags        provisioning for CI
memory import|export|...   data operations
schedule / gateway / serve  service control
skills / plugins / mcp / provider / fallback / repair / reset
```

Criteria for the line: takes a fixed argv, never reads a TTY, emits pipeable output or a
JSON document, has an exit code. `pantheon doctor` is the clearest case: JSON plus exit
code, consumed by scripts. It is not a UI.

`chat` deserves a note because it looks interactive. It is not: it is a single turn
with a message argument, and that is a scripting primitive. It stays as a verb and is
*not* replaced by the TUI. `pantheon chat "hi"` in a pipeline stays valid forever.

---

## L0 scope

Milestone: **one terminal product.** Not "three surfaces sharing components."

### Step 1. Relocate `build_model_policy` → `config_doc.rs`

First, because four modules import it and deleting the REPL takes it with them. Pure
move, no behavior change.

### Step 2. New crate: `pantheon-tui`

```
crates/pantheon-tui/
  Cargo.toml          ratatui, crossterm, ctrlc, pantheon-core/runtime/exec/memory
  src/lib.rs          TuiApp: state, event loop, screen stack
  src/widget.rs       Select, MultiSelect, TextInput, Confirm, SearchList
  src/overlay.rs      modal draw + key capture, back to the history overlay precedent
  src/picker.rs       ModelPicker, ProviderPicker, DirectoryPicker
  src/commands.rs     the one registry: name, desc, category, args completer, handler
  src/session_view.rs current cockpit render + blocks + status
  src/setup.rs        section graph, conditional branches, provisioner
```

`widget.rs` is the component layer. A widget is a value, not a widget object: `Select`
is data plus a `handle_key` event, `Select { title, items, filter, sel, mode }`. The
main loop owns the stack. The TUI's existing history overlay is the proof the pattern
works; it becomes the first component built on it.

### Step 3. Entry points

```
no config.toml          → TUI opens straight into the setup wizard (finding 4)
config.toml present     → TUI opens into a session
pantheon --resume [id]  → TUI opens into that run
--version / --help      → stdout, exit 0, non-TTY
```

No `is_terminal()` branch remains in `main`. When there is no TTY and the user asks for
the product, the honest failure is: `pantheon: no terminal available (stdin/stdout not
a TTY)`, exit 1, remediation "run it in a terminal." That is a real error, not a second
interface.

### Step 4. Command registry

One table. TUI and nothing else dispatches commands. Every command in the target list
(`/models`, `/sessions`, `/resume`, `/memory`, `/tools`, `/skills`, `/settings`,
`/gateway`, `/doctor`, `/status`, `/help`) resolves here. `/cost` does not exist.

### Step 5. Migrate the cockpit

The current `tui.rs` becomes `session_view.rs`. Behavior preserved: streaming, live
token counter, tool cards, collapsed thinking, permission card, double-Esc interrupt,
history overlay → `SearchList`.

### Step 6. Delete

As per the table above. `cargo test --workspace` green, `cargo clippy --all-targets`
zero warnings, eval green.

---

## What L0 does not do

Explicitly, so the milestone is not oversold:

- **No setup screens yet.** L0 delivers the section graph's *host* (stack, navigation,
  provisioner interface) and the components. The actual wizard screens land with the
  runtime consumers they configure, per the gap map's L3/L4 pairing.
- **No tool groups, no reasoning effort, no workspace, no container exec.** Those are
  L1-L4.
- **No command palette autocomplete yet.** `/` opens the registry-backed list. Fuzzy
  filtering and Ctrl+K land with the registry polish.
- **`router` stays in the catalog**, filtered from the picker by a `dev: true` flag.

---

## Verification for L0

The rules in the skill that matter most here, and how each is honored:

- **A local green run is not evidence.** After L0 I push and read
  `gh run view <id> --log-failed`, I do not report success from a local count.
- **Rebuild the binary before any e2e probe.** `cargo build -p pantheon-cli` before
  running anything that invokes `target/debug/pantheon`.
- **Env lock discipline.** Any test touching `PANTHEON_DATA_DIR` locks
  `dotenv::test_support::TEST_ENV_LOCK` with
  `.unwrap_or_else(|e| e.into_inner())`. The deleted `session_cli_tests` set
  `PANTHEON_DATA_DIR` without locking; its replacement must not repeat that.
- **No silent no-ops.** Every removed interface is either migrated into a TUI screen or
  deleted. Nothing is left as a dead shim, and no screen ships selecting something the
  runtime cannot do.

Eval cases that must stay green through L0: the 3 `setup` cases (`--yes` still
non-interactive), plus `logs`, `run`, `doctor`, `audit`, `memory`, `hooks`, `plugins`,
`reset`. L0 does not change any runtime crate, so the other 14 cases are untouched by
construction.
