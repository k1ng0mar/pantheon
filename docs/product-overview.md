# Pantheon product overview

Status: current product description, September 2026

Pantheon is a Rust runtime for building agent products. It gives an agent a lifecycle, a durable record of what happened, a policy boundary for tools and data, and a recovery path when a process stops. A model provides language reasoning. Pantheon provides the system that lets that reasoning become controlled work.

The basic product idea is simple: a model is a replaceable component, while the runtime owns the parts that must remain dependable. Pantheon owns run state, permissions, execution, events, recovery, memory access, and delivery. Models, tools, skills, channels, and memory services sit around that core.

This document describes the product as the repository currently presents it. It separates working paths from components that are implemented but not yet connected to the main user experience. It also names the gaps that remain before the planned finished system can be claimed.

## What Pantheon is

Pantheon is an agent runtime and product foundation, not a hosted chatbot. It is intended to run locally on a laptop, desktop, VPS, or compatible edge device. SQLite is the default storage layer, so a working installation does not require a database service. The runtime is built around standard Rust threads rather than a separate async runtime. This keeps deployment simple, though the repository does not yet promise the same maturity for every planned surface.

Pantheon is for people and teams that need an agent to do more than return text. The system can run tool loops, inspect files, call shell commands, use git, hold a conversation across process exits, pause for approval, and resume after a crash. It is also for builders who want one runtime behind several forms of access: a terminal, a local web client, Discord, Telegram, and a JSON-RPC API.

The product is designed around a boundary between model output and host authority. A model may request a tool. The runtime checks the request against policy before the tool runs. A model may propose a memory record. The runtime attaches provenance, checks policy, and validates the record before storage. A model may ask for a secret. The intended execution boundary resolves the secret without placing its value in the model context or ledger.

This boundary is central to Pantheon's value. Agent products often lose trust when a successful prompt turns into an accidental file write, an exposed credential, or an unrecoverable process. Pantheon treats those concerns as runtime behavior rather than prompt instructions.

## Who it is for

Pantheon is for software builders who need an agent that can operate inside a real workspace. A solo developer can use the chat loop to inspect a repository, make gated changes, run commands, and inspect the resulting run history. A small team can put the same runtime behind a local server or a controlled chat gateway. A platform team can use the event ledger and policy boundary as the basis for an agent product without adopting a particular model vendor.

It is also for users who need to know why an agent did something. The runtime records events for run start, model requests, tool requests, approvals, memory writes, recovery, and completion. `pantheon explain` replays those events in a human-readable form, and reports the run's current state alongside them. `pantheon audit` exports a sequence-checked JSONL trajectory. This record is useful for debugging, handoff, compliance reviews, and tests.

The system is less suited to a user who wants a finished hosted personal assistant with a broad device ecosystem and managed accounts. Those capabilities are visible in the product plan, but the repository currently describes a local runtime with a small web client and channel adapters. The honest audience is a builder or technical operator who can configure providers, inspect permissions, and operate the process.

## The user value

Pantheon's user value comes from control over long-running work.

### Work that survives a restart

A conversation is represented as a run. Messages, assistant tool calls, tool results, approval decisions, and terminal state are stored in a SQLite event ledger. When a process exits, the next session can rebuild the transcript from persisted rows. A completed tool call is not silently repeated. A granted call that crashed before its result was recorded can be recovered from its stored name and arguments. A malformed or incomplete record receives an explicit tool error so the provider receives a valid transcript.

This behavior is working in the runtime path and is covered by the repository's recovery documentation and tests. It matters most when work includes a shell command, file edit, plugin process, or pipeline stage. A model should not be asked to remember the only copy of a result.

### Work that pauses for a person

Policies classify capabilities as allowed, denied, or approval-required. A call that needs approval creates a durable approval request and parks the run. The run can exit while it waits. A person can grant or deny the exact call scope from the CLI, the web client, or a supported channel. Granting a call allows that call to run on resume. Denying it writes a denied result into the transcript, so the agent can change course without treating refusal as a crash.

Approval is tied to the recorded call scope, not to a broad tool name. This prevents a grant for one shell call from authorizing another shell call. The runtime also settles a denied call on recovery, so a refusal does not leave a run waiting forever.

### Work with an audit trail

The event ledger is the source of truth for execution history. Status, explanation, audit export, recovery, and pipeline state all derive from the same rows. This gives a product owner one place to answer what happened without joining logs from the model, tools, gateway, and UI.

The offline `explain` path is current. OpenTelemetry span and metric mapping exists, but there is no live OTLP exporter. Users can inspect run data without sending it to an observability service.

### Work with controlled side effects

The default policy is still a local development policy, not a hardened multi-tenant boundary. It permits common file, shell, and git operations according to the selected preset. The shell path has a dangerous-pattern pre-gate that blocks a narrow class of destructive commands such as `rm -rf /` before execution. The audit also records that the shell tool inherits the daemon's current working directory. A user who starts Pantheon in a home directory may grant the model access to files readable from that location.

Pantheon's current value is that these boundaries are visible and testable. The product should be operated with that local trust model in mind. Stronger container and VM enforcement is planned, not current.

## Product principles

### Runtime authority over model authority

The model does not select its own model, grant its own permissions, or declare that a memory record is trusted. The runtime selects the default and failure-only fallback chain, checks capabilities, and assigns provenance. The agent operates within those decisions.

### Durable state over hopeful memory

A run needs a persisted account of its messages, operations, approvals, and outcomes. The product treats execution as recoverable state rather than a long prompt that a model may eventually forget.

### Small core, replaceable edges

The center owns lifecycle, state, policy, execution, capabilities, recovery, and events. Models, providers, channels, tools, extensions, and memory backends attach through defined seams. This makes composition possible without cloning separate runtimes for coding agents, personal agents, swarms, and automations.

### Capability identity over broad role labels

A role such as coder is a starting policy, not the security mechanism itself. Tools declare the capabilities they need. Policy reasons over capability, provenance, and danger. A researcher can be read-only; a coder can require approval for git push. The same tool can therefore be safe in one context and gated in another.

### Explainable state

A run should be understandable after the fact. Event names, structured errors, status transitions, approval scopes, context trims, and pipeline gates are designed to support explanation. The intended UI state machine adds a product surface for this, but the ledger and offline explain commands are the dependable foundation.

### Compatibility through composition

Pantheon does not need to replace every external coding agent or plugin system. Skills carry portable instructions. Extensions carry runtime code. MCP is reserved for external interoperability. A future coding engine can use native tools or an external agent adapter while the same supervisor owns policy and recovery.

## The intended finished experience

The intended product is one runtime with several ways to use it.

A user installs Pantheon, runs setup, chooses a provider and model, selects a policy, and runs `pantheon doctor`. The user opens an interactive session or sends a one-shot request. The agent streams a response, calls tools when policy permits, and shows enough state for the user to understand what is happening. A shell command that needs approval pauses. The user grants or denies it. The session continues.

The user can leave the process and return later. The session finds the latest run, restores the transcript, and resumes unfinished work. `pantheon explain run_id` shows the decisions and events. `pantheon memory recall` returns relevant facts with their source and trust tier. A human can confirm a proposed record when it deserves higher trust.

The same user can use the terminal cockpit, local web client, Telegram, or Discord. The surface changes delivery and interaction, while the runtime keeps the same run, policy, and approval model. A future desktop client can consume the same runtime API rather than implement a second agent engine.

For builders, the intended experience includes importing skills, installing extensions, adding memory backends through a documented protocol, and composing agents or pipelines without modifying the core. A package can carry instructions, skills, tools, hooks, policies, and tests. Installation verifies the package, resolves dependencies, runs checks, and requires approval before activation.

The finished system would add strong sandbox levels, durable agent identities, a package lifecycle, a fully wired runtime API, a richer product UI, live telemetry export, external coding-agent adapters, and scheduler-driven live runs. These are product goals. They are not current capabilities.

## Current capabilities

### Conversation and model execution

The current runtime supports a terminal cockpit, a text session, and one-shot chat. Running bare `pantheon` in an interactive terminal opens the terminal cockpit. It falls back to the text REPL when the input or output is not a terminal. Session slash commands handle local actions such as help, cost, status, run history, and transcript clearing. Those commands do not reach the model.

Chat supports OpenAI-compatible and Anthropic provider adapters, including normalized streaming events. The catalog is YAML-backed. Users can list providers and models, choose a model interactively, or pass command-line flags. Fallbacks are ordered and failure-only. There is no model routing layer, and agents cannot choose a replacement model.

The terminal cockpit shows streamed text, reasoning, tool activity, token use, estimated cost, and permission requests. Tool cards change from running to complete when runtime events arrive. Completed thinking blocks collapse into compact transcript entries. The cockpit has local commands for help, cost, run history, status, transcript clearing, and exit. It is an early interface, not the full product UI described in the state design.

The current tool path includes shell, file operations, and git behavior exposed by the configured policy. The repository's evaluation system exercises the real CLI end to end.

### Capabilities and approvals

The capability plane includes filesystem, shell, git, network, browser, channel, memory, secret, and agent-spawn concepts. The current implemented core has policy decisions and role maps, with the agent loop enforcing the decision before tool execution. Approval and denial are durable operations visible through the run interface.

The policy presets are `reader`, `coder`, and `coder_memory`. The reader preset can read files and git state but cannot execute shell commands. The coder preset permits file and shell work, with git push requiring approval. The coder memory preset adds memory writes. These are code-defined presets. Users are not expected to hand-edit a free-form capability policy in configuration.

### Durable runs and recovery

The SQLite ledger stores an append-only event history. The runtime has structured errors, run leases, durable tool operations, cancellation intent, operation state machines, and a recovery path. Completed and terminal statuses are guarded in the ledger append path.

Tool work advances through translation, execution, and result translation. The operation ID is the idempotency key for keyed execution. Process groups are associated with the run lease, and cancellation uses persisted intent before process termination. A watchdog checks activity and probes ledger health after a stall.

Current limits include a turn budget, a whole-run tool-call budget, and token and cost ceilings in the agent loop. The default turn and tool limits are 16 and 32. Token and cost limits are absent from the default budget, so a configured ceiling can stop the run but operators must still watch provider usage.

### Memory

The native memory store uses SQLite and full-text search. It has five layers: global, agent, project, task or session, and ephemeral. Ephemeral values do not enter the durable store. Recall returns provenance and the narrowest useful scope first.

Writes follow one path: proposal, policy check, provenance attachment, validation, and storage. The model cannot set the trust tier as authority. Model-authored and external material are clamped to untrusted unless a person performs an explicit promotion through `pantheon memory confirm`. Markdown export and import carry trust metadata in the current format, and conflict detection prevents a sync from silently choosing a side when the file and store both changed.

The native store is the practical default. The repository documents HTTP and named memory backend seams, but plugin backends require a service or bridge that speaks Pantheon's memory protocol. An offline construction or wrong URL can fail when the session first uses the backend. Import, export, and sync remain native-store behavior.

The vault tools provide a separate file library for longer documents. They are ordinary filesystem operations, not trust-tiered memory records. This keeps large notes editable in Obsidian while short facts stay in the controlled memory store.

### Extensions and skills

Skills are portable instruction files. Pantheon discovers skills in its own directories and in common `.agents`, `.claude`, `.pantheon`, Hermes, and OpenClaw locations. It can import a raw `SKILL.md` after validating it. The import path writes the original file so a later discovery pass can parse it again.

Extensions are runtime code. The current Python subprocess path supports hook calls and stdio tool calls. The supported hook family includes `pre_llm_call`, provider request hooks, and gateway dispatch. Hook failure is fail-open for context injection, with repeated failures disabling a plugin for the rest of the session. A plugin tool still passes through the capability gate.

The plugin doctor checks manifests, entry files, hook names, environment, and unsupported TypeScript entries. The repository reports Hermes plugin loading and anti-AI-writing and time-gap hooks as working through the ledger. The OpenClaw TypeScript adapter is not started.

### Pipelines

The orchestration pipeline runs intake, research, plan, implement, review, and commit. Human gates follow plan and review. Each stage is a durable operation, so a restart resumes completed work rather than beginning again. The default evaluator accepts output and leaves the human review gate in place. An opt-in strict evaluator can reject an implementation and feed it into another iteration.

The pipeline is a working current surface through the CLI. The repository docs describe the stage executor seam as the point where an external coding agent could later become an implementation backend.

### Channels and the web client

The local AG-UI server exposes a minimal web client, JSON-RPC routes, SSE frames, approval actions, cancellation, health, and signed artifact URLs. `pantheon serve` runs the same `Session` runtime used by CLI and gateway paths. The server can inject a serve token into the client. Without a token, the server is open on localhost. Non-local binding is rejected unless a serve token and signing secret are configured.

Discord uses a gateway websocket for the daemon and supports a normalized webhook bridge. Telegram uses long polling with a persisted update cursor. Both surfaces can deliver approval buttons. Both require `PANTHEON_GATEWAY_ALLOW`, a list of platform user IDs, before the daemon starts. The gateway is therefore an authenticated remote control surface, not an open bot shell.

The web client is intentionally minimal. It is a smoke-test surface, not a finished product interface.

### File changes and observability

Safe file operations support preview, stage, apply, checkpoint, and rollback. An apply writes through a temporary file and atomic rename, snapshots a checkpoint, and rejects a stale expected hash. The journal supports replay and rollback by checkpoint ID or ledger sequence.

`explain`, `status`, and `audit` are working. The OpenTelemetry crate maps events to spans and metrics, but there is no live OTLP push target. Users should treat the ledger as the current observability product.

## Product status by subsystem

### Working today

Working paths are those exercised through the documented CLI, runtime, ledger, or live channel path. This includes chat, tool loops, provider adapters, approvals, recovery, durable operations, memory with provenance, plugin loading and hooks, the setup wizard, doctor, the six-stage pipeline, the local AG-UI surface, Discord and Telegram adapters, safe file changes, and offline explanation.

### Partial functionality

Several subsystems have working code with incomplete product connection. Context compression has a provider seam and configurable auxiliary model, but deterministic trimming remains the correctness path. STT and TTS backends have a registry and testable command or HTTP implementations, while gateway and CLI voice consumers are not wired. Memory backend adapters have a protocol, but a bridge is required for external services. Sandbox profiles map capabilities to levels, but no container or VM enforces those levels. The scheduler has durable cron, interval, one-shot, and webhook logic, but does not yet drive a live agent run end to end. MCP has a policy-to-capability projection, but no live MCP server is attached. Migration has Hermes and OpenClaw analysis and planning code, but no CLI verb. The runtime API supports a useful subset of JSON-RPC and SSE methods, while the larger command list in the architecture document remains aspirational. The UI state matrix describes a future cockpit; a terminal cockpit exists (`pantheon` with a TTY) while a GUI remains absent.

### Planned

The planned product includes container and stronger VM sandboxing, durable agent identity, a package format and lifecycle, live scheduler execution, a migrated-item command, richer API coverage, external coding-agent adapters, live OpenTelemetry export, and a richer local interface. Default token and cost ceilings are also planned. The package plan also includes stable, beta, and nightly channels with pinning and rollback. The coding engine plan names LSP, DAP, AST, test, build, and package tools alongside external agent adapters. None should be described as available until the main runtime invokes it and the repository documents a real user path.

## The two agent loops

There are two, and this is the first thing to understand before reading the
runtime.

**`Session::drive` (pantheon-runtime) is the production loop.** It owns a
typed `Message` transcript, streams from a real provider chain, persists every
turn to the ledger, honours the run lease and watchdog, and enforces tool
gates. Everything a user does goes through it.

**`AgentLoop::run` (pantheon-agent) is a test harness.** It runs a
`Vec<String>` transcript against scripted `ModelTurn`s with no network and no
provider. It exists so the engine's budget, cancel, and tool-call logic can be
tested deterministically. It has no production caller.

`AgentLoop` the *struct* is used by both: `drive` constructs one to carry the
policy, budget, and run identity, then runs its own turn loop over those
fields. So the struct is shared and the loop is not. That distinction is the
confusing part, and it is why the two must not be collapsed casually:
`drive` is where the ledger, lease, streaming, and extension gates live.

Consequence worth knowing: because only `AgentLoop::run` consults a judge, a
`[judge]` in config validates and then does nothing. See
[configuration.md](configuration.md#judge-model).

## Security and recovery model

Pantheon's security model has four linked controls: capability policy, provenance, dangerous-pattern detection, and durable approval. A tool's capability comes from the runtime registry, not from a model's description of the tool. Policy then decides whether the call may execute, must stop for approval, or is denied. Shell commands receive an additional in-process danger gate before a subprocess starts. Plugin tools do not receive a bypass.

Memory has a separate trust model. Source content cannot raise its own authority by being copied into another layer or through markdown. The runtime clamps derived records at the write boundary. A user can confirm a record explicitly. This is designed to reduce prompt-injection persistence.

Secrets are represented in configuration by environment variable names, not values. The secrets crate includes environment and encrypted-file vaults plus a broker that can inject values at an execution boundary. The current configuration docs state that the broker is not yet wired into chat, so a user should not assume arbitrary agent tools can use it yet. The running chat path resolves its model key from the configured environment variable and keeps it out of prompts and ledger events.

Recovery is based on persisted state. A run lease prevents two supervisors from believing they own the same run. A lease can expire, allowing recovery after a dead supervisor. Losing a lease stops tool work. Process termination follows lease ownership rather than trusting a process group number. This is an important distinction when a crashed process has left a process group behind.

The web and gateway surfaces have explicit access controls. `PANTHEON_SERVE_TOKEN` protects AG-UI routes except health. `PANTHEON_GATEWAY_ALLOW` identifies the people allowed to send work to the gateway. Signed artifact URLs use a secret and expiration. A default development signing secret is documented as unsuitable for an exposed deployment.

## Deployment and surfaces

The primary deployment is a local binary built from Rust. Installation can build from source and link the binary into `~/.local/bin`. The data directory defaults to `~/.pantheon` and can be changed with `PANTHEON_DATA_DIR`. It contains the config, ledger, memory database, extensions, safe-write journal, and gateway cursors.

The supported user surfaces are the CLI, the interactive session, the local AG-UI server, and the Discord and Telegram gateway daemon. The CLI is the most complete surface today. The JSON-RPC API is the intended shared surface for future clients. The repository currently supports Unix socket transport and an HTTP server for the AG-UI route set.

The runtime is designed to be self-contained. SQLite, local files, and environment-based configuration are enough for a single-user installation. The current design does not claim a hosted control plane, account system, multi-tenant policy service, or managed secret store. The package ecosystem and migration workflow are intended to make distribution easier later.

## Ecosystem and extension model

Pantheon separates portable knowledge from executable extension behavior. A skill can teach an agent a workflow, supply examples, or carry references. It does not grant a capability. A plugin can provide code, tools, hooks, context, state, and UI behavior, subject to the same runtime controls. This separation lets a user bring in a written workflow without treating its text as permission.

The runtime is also designed to import parts of existing agent systems. Hermes and OpenClaw detection and migration analysis exist in the library. Hermes plugins load natively from `~/.hermes/plugins`. The architecture also names OpenCode, Cline, Codex, Pi, OMP, Claude Code, Aider, Goose, and other coding agents as possible adapters or references. That list is an integration direction, not a claim of current support.

MCP is an external interoperability boundary. An MCP tool can be projected into the native capability plane and denied if policy does not allow it. MCP does not define the internal architecture, and the current repository has no live MCP server attached.

The memory ecosystem has a small HTTP protocol for external stores. Custom HTTP or stdio bridges can be registered through manifests in the data directory. The boundary remains in Pantheon: policy, provenance, validation, and confirmation happen before a backend sees a write. A backend stores and recalls data; it does not decide whether the agent is allowed to do so.

## Current gaps and operating guidance

There are seven subsystem areas that the repository's own audit calls out as built ahead of their consumers: OpenTelemetry export, migration, swarm, MCP, scheduler, secrets, and sandbox. The audit calls them orphan crates because they compile and pass local tests without being used by the main runtime. The audit recommends deciding which should be wired next. The architecture document keeps sandbox and secrets as roadmap priorities and raises the risk that the other APIs will need reshaping when their real consumers arrive.

Sandbox levels are backed by a process executor that can use `unshare` or bubblewrap for namespace and resource limits. That executor currently has no consumer in the main chat path, so users should not assume a shell command runs inside it. The runtime falls back to direct process execution if the host tool is unavailable, which preserves policy checks but loses the requested OS boundary.

The default tool-call budget is also incomplete for cost control. The agent loop can enforce total-token and cost ceilings, but neither ceiling is part of the default budget. A 16-turn run with a large context model can still consume a large number of tokens unless an operator configures a ceiling and watches usage.

The hook surface is still pre-LLM. Pre-tool, post-tool, run-start, and run-stop hooks are not built. The current shell danger gate sits inside the shell tool, so it is not waiting for a future hook process to make its decision. Extensions can still affect model context through `pre_llm_call`, and plugin tools pass through the capability gate.

The gateway has known delivery limits. HTTP 429 responses currently surface as generic transport errors rather than driving a Retry-After-aware backoff. Conversation-to-run mappings are in memory for the daemon process, so a daemon restart creates new run IDs for existing chat threads while the old ledger remains available. The local web client is deliberately minimal.

The repository's September 24 audit reports 311 workspace tests, 20 evaluations, zero warnings, all committed and synced. This is evidence for the current build state, not a promise that every planned surface works. The product description should continue to use the status labels above as the codebase changes.

## Product direction

Pantheon's near-term product work should follow the order in which trust depends on integration. Wire the secrets broker into real execution. Land a real sandbox enforcement backend, beginning with a documented process boundary. Add a total-token budget and make context compression part of the ordinary long-run path. Add tool hooks after the deterministic danger gate is fixed in place. Give persistent agents identity and configuration records. Then expose scheduler and migration through the CLI. Package lifecycle and external coding-agent adapters can follow once the core contracts have survived real consumers.

The finished product is a runtime that can be used from several surfaces without splitting the agent's brain. A local operator should be able to start a task, leave, return, inspect, approve, and recover. A builder should be able to add a tool or memory service without inventing a new lifecycle. A reviewer should be able to answer why the system acted from durable events. A security-conscious operator should know which boundaries are real today and which are still policy labels.

That is the product Pantheon's current architecture is designed to produce. The repository has working pieces for that experience. The remaining work is to connect the right pieces, prove them through real paths, and remove any label that implies a capability the runtime has not yet enforced.
