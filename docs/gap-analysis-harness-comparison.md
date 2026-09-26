# Pantheon gap analysis vs Hermes, OpenClaw, and OMP

Date: September 2026. Sources read: Pantheon `ARCHITECTURE.md`, `docs/product-overview.md`,
`crates/pantheon-cli/src/main.rs`, and every non-test file in `pantheon-scheduler`,
`pantheon-swarm`, `pantheon-mcp`, `pantheon-sandbox`, `pantheon-otel`,
`pantheon-runtime/src/session.rs`, `pantheon-exec/src/{builtins,context,acp,plugins,danger,skills}.rs`,
`pantheon-gateway/src/`, `pantheon-extensions/src/`. Harnesses read on this machine:
`~/.hermes/hermes-agent/tools/`, `~/.local/lib/node_modules/openclaw/docs/`, and
`~/node_modules/@oh-my-pi/pi-{agent-core,coding-agent}/src/`.

`~/.hermes/cache/scratch/harness-study/` no longer exists on this machine, so OpenClaw was read
from its installed package and OMP from its `node_modules` tree, not from a pinned checkout. I
cannot verify the exact revisions of those two from this evidence.

## The headline finding

Eight of the gaps below are the same defect wearing different clothes: a subsystem was built,
unit-tested, documented in `ARCHITECTURE.md` as implemented, and never connected to the path a
user actually types into. `Session::drive` is the production loop, and it does not use
`pantheon-exec::context`, `pantheon-swarm`, `pantheon-otel`, or a live MCP client. The
`docs/product-overview.md` audit already names this class of problem ("orphan crates") but lists
seven subsystems; by code inspection there are more, and one of them (context management) is the
single most user-visible.

---

## Ranked gaps

### 1. No context-window management in the production loop

Who has it: Hermes (`hermes_state_compression.py`), OMP (`pi-agent-core/src/compaction/`, 19 files
including `branch-summarization.ts`, `pruning.ts`, `tool-protection.ts`, `shake.ts`), OpenClaw
(`docs/agents` context handling). All three compact proactively, protect recent tool results from
being pruned, and branch on overflow.

Pantheon: `pantheon-exec/src/context.rs` (333 lines) implements `WindowBudget`, `fit_to_window`,
`compress_oldest`, a `ContextCompressor` trait, and a `CONTEXT_OVERFLOW` error. It is
self-contained and correct-looking. Nothing calls it. `Session::drive` builds `messages` and hands
the whole vector to the provider:

- `crates/pantheon-runtime/src/session.rs:1355`: `chain.turn_with_sink(messages, &msink)?`
- `crates/pantheon-providers/src/chain.rs:270`: `turn_with_sink` passes `messages` straight to
  `self.run`; no trim, no budget, no limit lookup.
- `crates/pantheon-runtime/src/session.rs:18-25`: the import block pulls `builtins`,
  `memory_tools`, `safewrite`, `session_search_tools`, `supervisor`, `tools`. `context` is not
  imported.
- `grep -rn 'context::' crates/ --include=*.rs` outside tests returns nothing.

A user hits this on any conversation past a few hundred KB of tool output. The provider returns
`CONTEXT_OVERFLOW` or a 400, and the run dies with no recovery. `ContextTrimmed` and
`ContextCompressed` events exist in `pantheon-core/src/events.rs:195` and are mapped by the otel
fold and the extension bridge, so an operator watching `/explain` will never see one emitted.

This is the worst gap because `ARCHITECTURE.md:80` describes the behaviour in the present tense
("assembled messages are fitted to the catalog `context_limit` before each provider call") and
`product-overview.md:169` correctly says it is not in the ordinary path. The architecture doc is
the document that is wrong.

### 2. Sub-agent delegation is refused at runtime

Who has it: Hermes (`tools/delegate_task.py:738`, 738+ lines), OMP
(`pi-coding-agent/src/collab/`, `src/goals/`, `src/live/`), OpenClaw (`docs/automation/taskflow.md`,
`standing-orders.md`).

Pantheon: the `Swarm` type in `crates/pantheon-swarm/src/lib.rs:117` is complete. Caps, refusal
enums, budget folding, `max_depth` defaulting to 2. `AgentLoop` carries a `spawner: Option<&dyn
AgentSpawner>` field (`pantheon-agent/src/engine.rs:176`) and the `Delegate` arm calls it
(`engine.rs:453`). The only non-test implementation of `AgentSpawner` in the entire repository is
`CappedSpawner` in `engine_tests.rs:59`.

The production session hard-codes `spawner: None` (`session.rs:879`) and returns
`SWARM_SPAWN_DENIED` with remediation "configure a spawner or disable delegation"
(`session.rs:1514-1524`). `grep -rn 'impl .*AgentSpawner' crates/` confirms it.

So `pantheon swarm` exists as a verb and `pantheon-swarm` is a working crate, and the agent cannot
delegate. A user who reads section 3 of `ARCHITECTURE.md` and asks the agent to fan out gets a
structured error.

### 3. No MCP client, only a token-to-capability projection

Who has it: OMP (`pi-coding-agent/src/mcp/`: 22 files: `manager.ts`, `loader.ts`, `tool-bridge.ts`,
`oauth-flow.ts`, `oauth-credentials.ts`, `smithery-registry.ts`, `transports/`, `tool-cache.ts`,
`timeout.ts`), Hermes (`optional-mcps/`, `mcp_serve.py`), OpenClaw (docs/plugins).

Pantheon: `crates/pantheon-mcp/src/lib.rs` is 56 lines and exports exactly two functions,
`capability_from_token` and `project`. It maps a declared tool list to `ProjectedTool { allowed }`.
There is no stdio or HTTP client, no `initialize`, no `tools/list`, no subprocess, no notification
handling.

`crates/pantheon-cli/src/mcp_cli.rs` is 156 lines with one subcommand, `list`, which calls
`read_mcp_declarations` to show what `pantheon migrate` wrote to `<data_dir>/mcp/`. The file's own
doc comment says "inspect the MCP servers a migration declared."

`ARCHITECTURE.md:469` admits this: "MCP server *attachment* ... no launcher yet". A user cannot run
one MCP server. Migration bridges 4 server declarations and then has nowhere to put them.

### 4. Tool breadth: 17 tools, none of them web, image, browser, grep, or patch

Who has it: Hermes registers roughly 45 model-facing tools. A representative set from
`tools/*.py`: `web_search` (543), `web_extract` (549), `x_search`, `vision_analyze`,
`vision_analyze` video (1082), `image_generate`, `video_generate`, `text_to_speech`,
`computer_use`, `browser_cdp` (397), `browser_exec` (772), `browser_vault_*`, `execute_code` (925),
`terminal`, `read_file`, `write_file`, `patch` (1395), `search_files`, `todo_list`, `delegate_task`,
`cronjob_manage`, `skill_manage`, `session_search`, `memory`, `clarify`, `process_manage`,
`annotate_preview`, `apply_layout`, `drive_preview`, `gh` (via OMP). OMP adds `lsp/`, `dap/`,
`ast-grep`, `gh`, `read-pdf`, `read-sqlite`, `puppeteer/`, `security-scan`, `todo`, `review`.

Pantheon registers, in a live session, via `session.rs:709-770` and the modules it calls:

`shell`, `read_file`, `write_file`, `list_dir` (`builtins.rs:62,102,121,180`), `preview`/`stage`/
`apply`/`checkpoint`/`rollback` (safewrite), `skills_list`, `skill_read` (`skills.rs:116,136`),
`session_search` (`session_search_tools.rs:51`), `memory_recall`, `memory_list`, `memory_propose`,
`memory_forget`, `memory_confirm` (`memory_tools.rs:183-447`), and `vault_archive`, `vault_read`,
`vault_search`, `vault_list` (`vault_tools.rs:197-380`, only when a vault is configured).

There is no grep, no glob, no file search, no patch or line-edit tool, no web fetch, no web search,
no image input, no browser, no todo tracker, no subprocess management, no git tool.

`write_file` takes a full file body (`builtins.rs:124-131`: `path` and `content` required). With
no read-modify-write tool and no patch tool, a model asked to change one line in a 2000-line file
must emit the entire file. `read_file` has no offset or limit and always does
`read_to_string` (`builtins.rs:114`), so reading a large file pulls the whole thing into context,
where gap 1 means it never gets trimmed out.

This is the gap a user hits on minute one of real work.

### 5. Approval has no "always allow" and no rule persistence

Who has it: Hermes has 10 approval modules in `tools/approval*.py` covering floors, gateway wait,
human wait, smart detection, and prompt. OMP has `tools/approval.ts`, `tools/ask.ts`, and a
`capability/rule.ts` rule system. OpenCode is the harness-study reference for "once versus always"
(`references/harness-study.md:11`).

Pantheon: `Policy::check` returns `Allow` / `Deny` / `NeedsApproval`
(`pantheon-core/src/capability.rs`). The TUI renders a two-key card, literally
`"[y] Allow    [n] Deny"` (`crates/pantheon-cli/src/tui.rs:561`). There is no "a" for always, no
pattern or prefix rule, and no store for user-made decisions. `grep -ni 'always|once|remember'`
on `capability.rs` returns nothing.

Approval is also all-or-nothing at the scope string. `product-overview.md:43` calls this a
feature ("Approval is tied to the recorded call scope, not to a broad tool name"), and it is
correct for safety, but it means a user approving one `git push` answers the same question again
for the next one, in the same session, with no way to say "yes to all pushes in this repo".

Matters a lot for any agent that loops. It matters less for a one-shot command.

### 6. No OTLP exporter, no live metrics, no live trace

Who has it: Hermes ships `agent/monitoring/otlp_exporter.py` and
`gateway_health_export.py`. OMP ships `pi-coding-agent/src/telemetry-export-otlp.ts` (a real
`http/protobuf` OTLP graph, lazily loaded) and `pi-agent-core/src/telemetry.ts`. OpenClaw has
`docs/logging.md` and a diagnostics tree.

Pantheon: `crates/pantheon-otel/src/lib.rs` is 150 lines. `span_for` (42) maps events to
`SpanRecord` structs. `metrics_from` (122) folds into seven `u64` counters. `explain` (140)
formats one line. `SpanRecord` has no start time, no end time, no duration, no trace ID, no span
ID, no parent, no attributes, no status code. `Metrics` has no labels at all.

`grep -rni 'otlp|opentelemetry|prometheus|jaeger' crates/ Cargo.toml` returns zero hits. The
string "opentelemetry" does not appear in any Cargo.toml in the workspace.

`ARCHITECTURE.md:312` is honest: "no live OTel exporter ... instrumentation is fold-only today."
There is also a second, undocumented problem: the span records as designed cannot be exported
even if an exporter were added, because they carry no timing. An OTLP exporter needs spans with
start and end timestamps. That is a redesign, not a wiring job.

Matters for anyone running Pantheon in anything like production. A solo user testing the CLI will
not notice.

### 7. Sandbox fails open to an unsandboxed process, and cannot be told to fail closed

Who has it: OpenClaw `docs/security/`, Hermes has a sandbox tier system plus `approvals_floors.py`.
OMP has `tools/security-scan.ts` and `src/security/`.

Pantheon: `crates/pantheon-sandbox/src/runner.rs` is honest about this. Every one of the four
boundary arms has the same shape. For `Container` (runner.rs:84-121):

```rust
if has("bwrap") { /* bwrap with unshare + rlimits */ } else {
    let mut cmd = Command::new(program);   // no isolation at all
    cmd.args(args); cmd.current_dir(cwd); cmd
}
```

Same pattern for `InProcess` (runner.rs:52-59), `IsolatedProcess` (runner.rs:62-81), and `Vm`
(runner.rs:126-160, which "falls back to Container ... VM is Phase B"). The result struct does
carry a `sandboxed: bool` field (runner.rs:25) so the caller could know. `run_shell`
(`builtins.rs:202-222`) reads `result.exit_code` and `result.output` and never looks at it, and
never surfaces a warning to the model or the user.

Concretely: on a host without bubblewrap, a run that the operator believes is `SandboxLevel::High`
executes `sh -c` as a direct child of the daemon, with the daemon's cwd and its full filesystem
access. `SandboxProfile` values, `drop-caps`, `no-new-privs`: all still applied as rlimits, so
the process limits hold. The namespace boundary does not.

`builtins.rs:207` hard-codes `SandboxProfile::from(SandboxLevel::High)` for every shell call,
which is the right default and is why this is the top sandbox finding rather than "no sandbox".

The `product-overview.md:234` claim that "That executor currently has no consumer in the main chat
path" is now wrong. `builtins.rs:212` is the chat path.

### 8. Scheduler: no daemon, no catch-up, and the durable claim ledger is unused

Who has it: Hermes `tools/cronjob_tools.py` (1105+ lines, `create`/`list`/`update`/`remove`/`run`/
`run_now`/`trigger` with at-most-once claiming, `run_code` in-process). OpenClaw
`docs/automation/cron-jobs.md` plus `docs/automation/hooks/`, `imap.md`. OMP has a goals and
task system.

Pantheon has real pieces and three holes.

(a) No autonomous firing. `schedule_cli.rs:280-330` implements `tick` with an optional `--watch`
loop that sleeps 30 seconds. Its own comment at line 282-284 admits it: "`tick` is the primitive a
daemon, cron entry, or CI step calls". Nothing in the repo installs a crontab entry, starts a
daemon, or registers a service. A user creates a job and it never fires. The gateway daemon
(`pantheon-gateway/src/daemon.rs`) does not reference the scheduler at all.

(b) Missed-run catch-up is policy-only. `runs_for_missed` (`pantheon-scheduler/src/idempotency.rs:65`)
implements `Skip` / `RunOnce` / `CatchUp` correctly. `grep -rn 'runs_for_missed' crates/` shows
the only non-definition callers are its own test file and the `pub use` in `lib.rs:14`. The `tick`
loop calls `scheduled.due(now, j.last_run)` and fires once. `ARCHITECTURE.md:470` admits this.

(c) The durable claim ledger has no consumer. `DurableClaimLedger`
(`pantheon-scheduler/src/durable.rs:19`) wraps `ClaimStore` with an atomic first-wins claim that
would make at-most-once firing real. `grep -rn 'DurableClaimLedger' crates/` shows it is
constructed in its own tests and nowhere else. `schedule_cli.rs:4-5` claims idempotency "is handled
by the scheduler's DurableClaimLedger over the ClaimStore" and then never constructs one. A
`tick --watch` that fires a job and crashes before saving `last_run` will re-fire it.

Also: `MissedPolicy` is hard-coded to `RunOnce` on job creation (`schedule_cli.rs:166`) and there
is no CLI flag to change it.

### 9. Extensions: hooks fire, but the provider registry is refused and TS plugins are out

Who has it: OpenClaw has 106 real extensions; the `ARCHITECTURE.md:168` measurement is that 2
import into Pantheon and 105 archive. Hermes loads plugins from `~/.hermes/plugins`. OMP has
`extensibility/plugins`, `custom-commands`, `custom-tools`, `hooks`.

Pantheon: the hook surface is real. 14 declared, 13 wired (`ARCHITECTURE.md:140`), with
`Hook::is_wired()` as the source of truth and a compat adapter that refuses to report an unwired
hook as mapped. That is better discipline than any of the three harnesses. I am not listing hook
coverage as a gap.

The gaps are:

(a) The OpenClaw adapter refuses `registerProvider`, `registerTool`, `registerHttpRoute`, and the
media/speech/search registrations (`compat.rs:222`, `js_runner.rs:166`). This is why 105 of 106
extensions archive. It is the stated design ("a provider registry is a different subsystem") and
it is correct as a scoping decision. It is still the single largest import gap, and closing it
means building the provider plane that section 14 lists as future work.

(b) OpenClaw TypeScript plugins are flagged, not run. `doctor` reports `TS_ENTRY`; the TS adapter
is "not started" (`ARCHITECTURE.md:162`). A JS runner exists for OMP-style `package.json` plugins.

(c) No install lifecycle. `install_catalog` (`pantheon-exec/src/plugins.rs:350`) clones a pinned
SHA from the Hermes docs API and writes a manifest. `set_enabled` and `verify_plugin` exist. There
is no dependency resolution, no sandbox-before-run, no per-plugin test, no update, no
`uninstall`, no channel or pinning concept. `ARCHITECTURE.md:363` says the package ecosystem is
"not started" and that is accurate.

### 10. ACP adapter is a handshake probe, not an execution backend

Who has it: OMP has `tools/acp-bridge.ts` and `modes/acp/acp-agent.ts`. Hermes has `acp_adapter/`.
OpenClaw has `docs/agent-runtime-architecture.md`.

Pantheon: `crates/pantheon-exec/src/acp.rs` spawns a server over stdio, frames JSON-RPC 2.0 both
ways with both Content-Length and bare-line reads, and performs `initialize` with version
negotiation. That is real work and it fails closed on a version mismatch. `grep -n 'method'`
shows `initialize` as the only method it sends. There is no `session/new`, no `session/prompt`, no
tool-call round trip. `ARCHITECTURE.md:132` calls it "a probe, not an execution backend."

So the "use Claude Code or OMP as an execution backend" idea in section 6 has no working path.
Every coding task runs on Panthe's own four built-in tools, which is gap 4.

### 11. Channels: two, both authenticated, no attachments, no more

Who has it: OpenClaw ships channel adapters for Telegram, Discord, Slack, Signal, WhatsApp,
Matrix, IRC, LINE, Feishu, Google Chat, iMessage, and more
(`~/.local/lib/node_modules/openclaw/docs/channels/` lists 30+ entries including `matrix`,
`irc.md`, `line.md`, `feishu/`, `imessage/`, `googlechat.md`). Hermes has Telegram, Discord, Slack,
Feishu, email, WhatsApp.

Pantheon: Telegram (long-poll daemon, persisted cursor) and Discord (gateway websocket,
IDENTIFY/RESUME, heartbeat with missed-ACK reconnect, plus a webhook bridge). Both have approval
buttons, both are behind a default-deny allowlist with one-shot pairing. This is solid work.

The gaps: `Attachment` is a struct in `crates/pantheon-gateway/src/lib.rs:52` and it is carried
on `InboundMessage` (line 67), but `grep -n 'photo|document|download|file_path'` on
`pantheon-gateway/src/telegram.rs` returns nothing. Telegram photo and document messages do not
download the file. The Discord adapter has no attachment handling either. A user who photographs
a whiteboard gets a caption or nothing.

There is also no third channel, and the ARCHITECTURE.md:272 note that Hermes's proactive
`_rate_limits.json` pacing is unbuilt is still true.

### 12. No image or file input on any surface

Who has it: Hermes `vision_analyze` (`tools/vision_tools.py:932`), `video_analyze` (1082),
`image_generate`, and file attachments through `web_extract` and `drive_preview`. OMP has
`tools/image-gen.ts`, `read-pdf.ts`, `read-sqlite.ts`, `puppeteer/`.

Pantheon: `grep -n 'image|attachment|upload|multipart'` on
`crates/pantheon-api/src/serve.rs` finds one hit, a comment about the 1 MiB request-body cap at
line 283. There is no multipart handling, no image decode, no vision call path. `AuxiliaryKind::Vision`
exists in the model policy (`pantheon-core/src/model.rs:37`) as a config value with an API key env
name, and `ARCHITECTURE.md:249` confirms vision-service swaps are future work. The TUI has no
paste-image path.

### 13. Scheduling of costs and default budgets

Who has it: Hermes `/cost` and token accounting per run. OMP has `omp-stats` and a `stats` module.

Pantheon: `AgentLoop` enforces `max_tokens` and `max_cost_cents` (`engine.rs`, the
`total_tokens` / `total_cost_cents` accumulation) and the TUI shows a live estimate at roughly four
characters per token (`tui.rs:201-207`). Both ceilings are absent from the default `Budget`
(`product-overview.md:121`: "Token and cost limits are absent from the default budget, so a
configured ceiling can stop the run but operators must still watch provider usage"). A 16-turn run
on a large-context model with no ceiling set can spend freely.

A related dead feature: the judge model. `product-overview.md:196-198` says it plainly, and the
code agrees. `AgentLoop.judge` is consulted only inside `AgentLoop::run`
(`engine.rs`, `consult_route_advisory` and `consult_gate_advisory`), and `product-overview.md:185`
establishes that `AgentLoop::run` is a test harness with no production caller. `session.rs:880` sets
`judge: None`. A `[judge]` block in config validates and does nothing.

### 14. Config and secrets: env-file only, broker not on the tool path

Who has it: Hermes has a layered config with `cli-config.yaml`, profile-aware defaults, and a
setup wizard. OpenClaw has `auth-profiles.json` and multi-account helpers
(`dist/account-*.mjs`).

Pantheon: config is TOML in the data dir with a YAML catalog, a `doctor`, a `setup` wizard, and
34 CLI verbs. `pantheon-secrets` has `SecretVault`, `MemoryVault`, `EnvVault`, an AES-256-GCM
`EncryptedFileVault`, and a broker. `ARCHITECTURE.md:239` states "OS keychain backends not
implemented", and the most recent commit on this repo is a keyring fix, so that is moving.

The live gap: the broker resolves the model API key (`main.rs:543`, `config_doc.rs:805`), and
`grep -rn 'broker'` outside `pantheon-secrets/` finds no tool-execution injection path. The
ARCHITECTURE.md:236 design ("agent → credential capability → secret broker → inject at execution
boundary") is not connected to the execution boundary. No model-facing tool can currently resolve
a secret. There is no OAuth flow and no multi-account support; `grep -ni oauth` on `crates/`
returns only a file-exclusion list in the migrator.

### 15. Packaging and distribution

Who has it: Hermes `setup.sh`/`install.ps1` and a `pyproject.toml`. OMP and OpenClaw both ship as
npm packages with update channels. OpenClaw has `node-runtime-update.mjs` and
`update-check.json` in its state dir.

Pantheon: `install.sh` and `install.ps1` at the repo root, a Rust binary, a data dir under
`~/.pantheon`. That is a working local install. `ARCHITECTURE.md:363` says the package ecosystem
(`packages/` format, install/verify/resolve/sandbox/test/approve/activate/rollback, stable/beta/
nightly channels, pinning) is "not started". No update mechanism exists. For a user testing today
this means rebuilding from source to get a fix.

---

## What Pantheon has that the harnesses do not

Worth stating, because the brief asked for skepticism in both directions.

- **The event ledger.** Every meaningful transition is a typed `Event` persisted to SQLite with
  sequence numbers, giving replay, crash recovery, and `/explain` offline. Hermes has
  `hermes_state_*` modules, OMP has `append-only-context.ts`, and both are less uniform about it.
  Hermes's own audit trail is a parallel concept.
- **Capability identity over role labels.** Tools declare the capabilities they need; policy
  reasons over the capability. The `git push` detection in `builtins.rs:82-98` is the kind of
  detail most harnesses get wrong.
- **Provenance on every memory write.** `propose → policy → provenance → validation → provider`,
  with model-authored content clamped to untrusted and human `memory confirm` required to promote.
- **Hook honesty.** `Hook::is_wired()` as the single source of truth, with the compat adapter
  refusing to report an unwired hook as mapped, is stricter than anything in the three harnesses.
- **The durable safe-write path as the default**, not an opt-in: `write_file` routes through
  `SafeWriter` with checkpoint, journal, atomic publish, and stale-hash rejection.
- **Migration provenance.** `pantheon-migrate` archives 105 OpenClaw extensions with specific
  reasons rather than dropping them, never writes a credential outside `<data_dir>/.env`, and
  refuses to clobber an existing key.

These are real. The gap list above is about the distance between the parts and the product, not
about the parts.

---

## Recommendation order

The first three are wiring jobs, not design work, and each one changes what a user can do in a
single afternoon:

1. Call `fit_to_window` in `Session::drive` before `chain.turn_with_sink` (gap 1). The module
   exists and is tested. This is the highest-value change in the repository.
2. Give `Session` a real `AgentSpawner` backed by `Swarm` (gap 2). Also mostly wiring, plus
   deciding whether the transcript fragment comes back synchronously.
3. Add a patch or line-edit tool and a grep/glob tool (gap 4). Small in code, large in practice.

After that, the sequence that `product-overview.md:246` already recommends is right: secrets
broker into execution, then a sandbox backend that can fail closed, then a live OTLP exporter
(which needs the `SpanRecord` redesign first, so do it before the exporter), then the package
lifecycle.

Fix the two documents as well. `ARCHITECTURE.md` section 2 claims context fitting is implemented
and section 3 presents swarm as live. Both are the cause of the orphan-crate pattern: a subsystem
gets a "Status: Implemented" line the moment its unit tests pass, and nobody checks the second
condition, which is that a user can reach it.
