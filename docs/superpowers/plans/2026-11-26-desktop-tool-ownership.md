# Desktop Tool Ownership Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task by task. Steps use checkbox syntax.

**Goal:** Make Pantheon Desktop a first party client with no parallel agent tool ecosystem, where Pantheon owns every agent callable tool contract and Desktop supplies only UI and native capability adapters.

**Architecture:** Pantheon API owns a stable desktop tool contract. Pantheon Runtime owns lifecycle, execution, approvals, events and recovery. Pantheon Tools owns native tools and schemas. Desktop registers only contracted tools through first party adapters. Panda Code has no runtime authority.

**Tech Stack:** Rust workspace crates `pantheon-api`, `pantheon-tools`, `pantheon-runtime`, `pantheon-web`; separate Tauri workspace under `desktop/`; vitest/Playwright for UI.

**Spec:** `docs/developer/desktop-tool-ownership.md`

## Global Constraints

- Desktop is a first party Pantheon client, not a multi harness application.
- Pantheon is not merely a default provider. `PROVIDERS` and `HARNESSES` contain only `pantheon`.
- Preserve capabilities, not desktop implementations.
- No fake implementations, duplicate tools, or indefinite compatibility shims.
- A desktop side implementation is allowed only for native window, UI or hardware work, with its definition, authorization, registration and invocation contract owned by Pantheon.
- All execution passes through Pantheon authorization and approval.
- Every change must be committed with tests and a green local verification run.

---

### Task 1: Pantheon API tool contract

**Files:**

- Create: `crates/pantheon-api/src/desktop_tool_contract.rs`
- Modify: `crates/pantheon-api/src/lib.rs`

**Interfaces:**

- Produces: `DESKTOP_TOOLS`, `DesktopToolDomain`, `is_desktop_tool`, `validate_catalog`.

- [x] Write the contract with all 79 currently registered desktop tools and domain tags.
- [x] Add tests for count, uniqueness, catalog validation and rejection of unowned tools.
- [x] Run `cargo test -p pantheon-api desktop_tool_contract`: 4 passed.
- [x] Commit as `feat(api): add Pantheon owned desktop tool contract`.

### Task 2: Desktop host validation against the contract

**Files:**

- Modify: `desktop/src-tauri/Cargo.toml`, `desktop/src-tauri/Cargo.lock`
- Modify: `desktop/src-tauri/src/providers/host_tools.rs`
- Create: `desktop/src-tauri/src/tool_ownership.rs`
- Modify: `desktop/src-tauri/src/lib.rs`
- Modify: `desktop/src-tauri/src/providers/mcp_host.rs` (test fixtures)

**Interfaces:**

- Consumes: `pantheon_api::desktop_tool_contract::validate_catalog`.
- Produces: architecture tests that fail on foreign tools, duplicate tools and alternative harnesses.

- [x] Add path dependency on `pantheon-api`.
- [x] Call the contract validator in `HostTools::new`.
- [x] Replace invented test tool names with contracted names.
- [x] Add `tool_ownership.rs` tests for harness allowlist, all live catalogs and foreign tool rejection.
- [x] Run `cargo test --lib`: 898 passed, 29 ignored.
- [ ] Commit as `feat(desktop): enforce Pantheon tool contract on host catalogs`.

### Task 3: Security boundary fixes

**Files:**

- Modify: `desktop/src-tauri/src/providers/acp.rs`
- Modify: `desktop/src-tauri/src/providers/mcp_host.rs`
- Modify: `crates/pantheon-tui/src/cli_group.rs`
- Modify: `crates/pantheon-mcp/src/transports.rs`

- [ ] Remove auto grant of permission requests from agent supplied options in ACP.
- [ ] Require explicit user grant for client supplied `mcpServers`, with provenance recorded.
- [ ] Bind the validated tool catalog to the live MCP server instead of discarding it.
- [ ] Emit `approval_cancelled` only for genuine cancellation, with tests for granted and denied paths.
- [ ] Make `McpManager::connect` fail closed when no policy is installed.
- [ ] Add regression tests for each fix.
- [ ] Commit each fix separately.

### Task 4: Consolidate native overlaps

**Files:**

- Modify: `desktop/src-tauri/src/agent_tools.rs`
- Modify: `crates/pantheon-web/src/browser/tools.rs`
- Modify: `crates/pantheon-tools/src/tools_group.rs`
- Modify: `crates/pantheon-api/src/desktop_tool_contract.rs`

- [ ] Replace desktop browser automation with the native browser tool contract, keeping only visible browser UI adapters.
- [ ] Replace `pantheon_code_search` with native `codebase.search`.
- [ ] Replace `pantheon_create_worktree` with native `worktree.create`.
- [ ] Replace `pantheon_list_subagents` with native `subagent_list`.
- [ ] Replace bot memory with native memory tools plus bot scope.
- [ ] Replace `pantheon_open_terminal` execution guidance with native terminal tools.
- [ ] Remove consolidated names from the desktop contract only after all adapters stop advertising them.
- [ ] Add tests that no removed desktop tool is advertised.

### Task 5: Runtime lifecycle migration

**Files:**

- Modify: `crates/pantheon-api/src/desktop_tool_contract.rs`
- Modify: `crates/pantheon-runtime/src/session.rs`
- Modify: `desktop/src-tauri/src/agent_tools.rs`
- Modify: `desktop/src-tauri/src/architect.rs`
- Modify: `desktop/src-tauri/src/connections.rs`

- [ ] Define native contracts for task state, queue, steering and team orchestration.
- [ ] Move execution authority into Pantheon Runtime.
- [ ] Make desktop hosts call runtime APIs instead of mutating SQLite directly.
- [ ] Move connection secret resolution through the Pantheon broker.
- [ ] Remove model choice from auto team lead prompts and dispatch schemas.
- [ ] Keep desktop UI adapters behind the Pantheon interface.
- [ ] Add lifecycle tests in the runtime crate.

### Task 6: Add ons and desktop adapters

**Files:**

- Modify: `desktop/src-tauri/src/agent_tools.rs`
- Modify: `desktop/src-tauri/src/jev.rs`
- Modify: `desktop/src-tauri/src/jev_decisions/api.rs`
- Modify: `desktop/src-tauri/src/simulator/tools.rs`
- Modify: `desktop/src-tauri/src/vm/tools.rs`
- Modify: `desktop/src-tauri/src/design.rs`

- [ ] Decide and document whether Jev is independently justified. If yes, move it behind a Pantheon tool contract and secret broker. If no, remove it.
- [ ] Map simulator, VM, design and local server tools to first party desktop adapters with Pantheon owned schemas.
- [ ] Route VM shell execution and cloud CLI through Pantheon execution and secrets.
- [ ] Add tests that no adapter bypasses approval.

### Task 7: Discovery and final acceptance

**Files:**

- Modify: `desktop/src-tauri/src/integrations.rs`
- Modify: `desktop/src-tauri/src/providers/host_tools.rs`
- Add: Playwright spec for tool discovery.

- [ ] Make every Pantheon tool intended for desktop use discoverable through the desktop integration.
- [ ] Assert the discovered set matches the contract.
- [ ] Run `cargo test --workspace`, desktop `cargo test --lib`, `npm test`, and a focused Playwright run.
- [ ] Delete the migration matrix only after all rows are either implemented or explicitly retired with a reason.

## Acceptance tests

1. `tool_ownership::tests::every_live_catalog_tool_is_in_the_pantheon_contract`
2. `tool_ownership::tests::unknown_and_foreign_tools_cannot_be_registered`
3. `tool_ownership::tests::only_pantheon_can_be_selected_or_launched`
4. Catalog validation rejects duplicate, malformed and unowned tools.
5. No desktop tool advertises a model routing or provider selection parameter.
6. All approval events distinguish granted, denied, expired and cancelled.
7. Native browser, code search, worktree and memory tools replace desktop implementations.
8. No secret is stored outside the Pantheon broker.
