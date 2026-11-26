# Desktop tool ownership migration

Status: proposed architecture, evidence collected at `e730e64` in worktree `desktop-tool-ownership`.

## Rule

Pantheon Desktop is a first party Pantheon client. It is not a multi harness application, and Pantheon is not merely its default provider. Panda Code is inherited implementation only, never a runtime dependency or product identity.

| Layer | Ownership |
| --- | --- |
| Pantheon Runtime | Agent lifecycle, execution, approvals, events, recovery |
| Pantheon Tools | All agent callable tools and their schemas |
| Pantheon API | Stable interfaces exposed to clients |
| Pantheon Desktop | First party UI and native capability adapters |
| Panda Code | No runtime authority or product dependency |

A tool that opens a native window may need a desktop side implementation. Its definition, authorization, registration and invocation contract still belong to Pantheon. Preserve capabilities, not the existing desktop implementation.

## Inventory summary

The desktop registers **79 agent callable tools** across eight host catalogs. `agent_tools.rs` holds 45 of them; the rest come from architect, bots, simulator, VM, design, cloud deploy, session titles and turn changes hosts. They are chained at `desktop/src-tauri/src/lib.rs:2031-2064` and exposed to the agent as client supplied MCP servers by `desktop/src-tauri/src/providers/mcp_host.rs:409`.

Native Pantheon already owns shell, file, terminal, safe write, memory, skills, browser, web search, LSP, code graph, worktree, delegation and MCP tools. Registration is in `crates/pantheon-runtime/src/session.rs:4767` and `crates/pantheon-runtime/src/session.rs:5425`, with implementations in `crates/pantheon-tools/src/tools_group.rs` and `crates/pantheon-web/src/browser/tools.rs:742`.

## Architectural findings

1. **Host tools bypass the native registry.** Desktop tools are validated in `providers/host_tools.rs` and served over a loopback MCP server, instead of being Pantheon tools. Evidence: `desktop/src-tauri/src/lib.rs:2031-2064`, `desktop/src-tauri/src/providers/mcp_host.rs:77`, `desktop/src-tauri/src/providers/mcp_host.rs:408-416`.
2. **Catalog validation is discarded.** `HostTools::new` validates a clone, then live dispatch reads raw `host.tools`. Evidence: `desktop/src-tauri/src/providers/mcp_host.rs:77`, `desktop/src-tauri/src/providers/mcp_host.rs:93-100`, `desktop/src-tauri/src/providers/host_tools.rs:63-74`.
3. **Full mode silently allows host actions.** `authorize` allows every tool outside `approval_tools` and, in full mode, all protected tools except `ALWAYS_ASK`. Evidence: `desktop/src-tauri/src/providers/mcp_host.rs:498-507`.
4. **ACP permission requests can be auto granted from agent supplied metadata.** Evidence: `desktop/src-tauri/src/providers/acp.rs:191-220`. The remote peer supplies the option list and tool kind.
5. **ACP session params can inject MCP servers that bypass the consent store.** Evidence: `crates/pantheon-tui/src/cli_group.rs:3667-3695`.
6. **Every approval emits `approval_cancelled` on drop, including success.** Evidence: `desktop/src-tauri/src/providers/mcp_host.rs:201-218`, `desktop/src-tauri/src/providers/mcp_host.rs:508-536`.
7. **Auto teams still let the lead choose worker models.** Conflicts with locked decision 2. Evidence: `desktop/src-tauri/src/architect.rs:557`, `desktop/src-tauri/src/architect.rs:1070`.
8. **Jev keys bypass the Pantheon secret broker and provider abstraction.** Evidence: `desktop/src-tauri/src/jev.rs:495`, `desktop/src-tauri/src/jev_decisions/api.rs:117`.
9. **MCP spawn enforcement is opt in at the manager level.** Evidence: `crates/pantheon-mcp/src/transports.rs:3042-3050`, `crates/pantheon-mcp/src/transports.rs:393-442`.

Findings 4, 5 and 9 are security defects and get their own workstream. They are not solved by the tool contract alone.

## Complete migration matrix

Priority: P0 blocks ownership or security, P1 duplicate native capability, P2 state and lifecycle migration, P3 desktop adapters and add ons.

| Tool | Current site | Finding | Required action | Priority |
| --- | --- | --- | --- | --- |
| `pantheon_list_projects` | `agent_tools.rs:699` | Project task state | Implement native Pantheon task API; desktop adapter only | P2 |
| `pantheon_list_tasks` | `agent_tools.rs:700` | Project task state | Implement native Pantheon task API; desktop adapter only | P2 |
| `pantheon_read_task` | `agent_tools.rs:705` | Project task state | Implement native Pantheon task API; desktop adapter only | P2 |
| `pantheon_list_moodboards` | `agent_tools.rs:720` | Desktop UI state | First-party desktop adapter behind Pantheon contract | P3 |
| `pantheon_read_moodboard` | `agent_tools.rs:721` | Desktop UI state | First-party desktop adapter behind Pantheon contract | P3 |
| `pantheon_read_moodboard_image` | `agent_tools.rs:727` | Desktop UI state | First-party desktop adapter behind Pantheon contract | P3 |
| `pantheon_list_subagents` | `agent_tools.rs:737` | Overlaps native subagent_list | Use native Pantheon tool; keep only UI presentation | P1 |
| `pantheon_create_task` | `agent_tools.rs:738` | Project task state | Implement native Pantheon task API; desktop adapter only | P2 |
| `pantheon_start_task` | `agent_tools.rs:752` | Runtime lifecycle | Move launch authority into Pantheon Runtime | P2 |
| `pantheon_send_message` | `agent_tools.rs:752` | Runtime lifecycle | Move launch authority into Pantheon Runtime | P2 |
| `pantheon_rename_task` | `agent_tools.rs:772` | Project task state | Implement native Pantheon task API; desktop adapter only | P2 |
| `pantheon_search_tasks` | `agent_tools.rs:791` | Project conversation search | Use native session_search with task scope | P1 |
| `pantheon_code_search` | `agent_tools.rs:794` | Overlaps native codebase.search | Use native Pantheon code search | P1 |
| `pantheon_list_queue` | `agent_tools.rs:802` | Runtime queue | Move queue into Pantheon Runtime contract | P2 |
| `pantheon_queue_message` | `agent_tools.rs:806` | Runtime queue | Move queue into Pantheon Runtime contract | P2 |
| `pantheon_remove_queued` | `agent_tools.rs:814` | Runtime queue | Move queue into Pantheon Runtime contract | P2 |
| `pantheon_steer_task` | `agent_tools.rs:837` | Runtime steering | Move steering into Pantheon Runtime contract | P2 |
| `pantheon_interrupt_task` | `agent_tools.rs:848` | Runtime lifecycle | Move lifecycle authority into Pantheon Runtime | P2 |
| `pantheon_pin_task` | `agent_tools.rs:852` | Project task state | Implement native Pantheon task API; desktop adapter only | P2 |
| `pantheon_archive_task` | `agent_tools.rs:772` | Project task state | Implement native Pantheon task API; desktop adapter only | P2 |
| `pantheon_set_task_stage` | `agent_tools.rs:772` | Project task state | Implement native Pantheon task API; desktop adapter only | P2 |
| `pantheon_create_worktree` | `agent_tools.rs:782` | Overlaps native worktree.create | Use native Pantheon worktree tool | P1 |
| `pantheon_open_task` | `agent_tools.rs:870` | Desktop UI | First-party desktop adapter behind Pantheon contract | P3 |
| `pantheon_open_file` | `agent_tools.rs:861` | Desktop UI | First-party desktop adapter behind Pantheon contract | P3 |
| `pantheon_open_terminal` | `agent_tools.rs:870` | Desktop display, native terminal exists | Keep display adapter; execution stays in native terminal tools | P1 |
| `pantheon_list_local_servers` | `agent_tools.rs:790` | Desktop local state | First-party desktop adapter behind Pantheon contract | P3 |
| `pantheon_open_browser` | `agent_tools.rs:870` | Overlaps native browser automation | Use native browser contract; visible browser is desktop adapter | P1 |
| `pantheon_read_browser` | `agent_tools.rs:870` | Overlaps native browser automation | Use native browser contract; visible browser is desktop adapter | P1 |
| `pantheon_browser_action` | `agent_tools.rs:870` | Overlaps native browser automation | Use native browser contract; visible browser is desktop adapter | P1 |
| `pantheon_browser_click` | `agent_tools.rs:870` | Overlaps native browser automation | Use native browser contract; visible browser is desktop adapter | P1 |
| `pantheon_browser_type` | `agent_tools.rs:870` | Overlaps native browser automation | Use native browser contract; visible browser is desktop adapter | P1 |
| `pantheon_capture_browser` | `agent_tools.rs:870` | Overlaps native browser screenshot | Use native browser contract; visible browser is desktop adapter | P1 |
| `pantheon_run_project` | `agent_tools.rs:82` | Desktop terminal UI | Desktop adapter behind Pantheon execution contract | P2 |
| `pantheon_preview_file` | `agent_tools.rs:82` | Desktop browser UI | Desktop adapter behind Pantheon browser contract | P2 |
| `pantheon_jev_test` | `agent_tools.rs:254` | External addon, browser overlap | Move to native browser or remove; no separate harness | P3 |
| `pantheon_jev_read` | `agent_tools.rs:296` | External addon state | Remove unless independently justified | P3 |
| `pantheon_jev_wait` | `agent_tools.rs:296` | External addon state | Remove unless independently justified | P3 |
| `pantheon_jev_list` | `agent_tools.rs:263` | External addon state | Remove unless independently justified | P3 |
| `pantheon_jev_screenshot` | `agent_tools.rs:281` | External addon state | Remove unless independently justified | P3 |
| `pantheon_jev_cancel` | `agent_tools.rs:277` | External addon execution | Remove unless independently justified | P3 |
| `pantheon_jev_review` | `agent_tools.rs:222` | External model API | Remove unless independently justified | P3 |
| `pantheon_jev_check_rules` | `agent_tools.rs:222` | External model API | Remove unless independently justified | P3 |
| `pantheon_jev_find_context` | `agent_tools.rs:222` | External model API | Remove unless independently justified | P3 |
| `pantheon_jev_choose_worker` | `agent_tools.rs:222` | External model API and routing aid | Remove; conflicts with no routing | P0 |
| `pantheon_jev_assessments` | `agent_tools.rs:246` | External addon state | Remove unless independently justified | P3 |
| `pantheon_dispatch_workers` | `architect.rs:1495` | Desktop team orchestration | Move to Pantheon Runtime team contract | P1 |
| `pantheon_list_workers` | `architect.rs:1611` | Desktop team orchestration | Move to Pantheon Runtime team contract | P1 |
| `pantheon_read_worker` | `architect.rs:1624` | Desktop team orchestration | Move to Pantheon Runtime team contract | P1 |
| `pantheon_followup_worker` | `architect.rs:1629` | Desktop team orchestration | Move to Pantheon Runtime team contract | P1 |
| `pantheon_cancel_worker` | `architect.rs:1681` | Desktop team orchestration | Move to Pantheon Runtime team contract | P1 |
| `pantheon_finish_team` | `architect.rs:1706` | Desktop team orchestration | Move to Pantheon Runtime team contract | P1 |
| `pantheon_team_directory` | `architect/coordination.rs:221` | Desktop team orchestration | Move to Pantheon Runtime team contract | P1 |
| `pantheon_send_team_message` | `architect/coordination.rs:243` | Desktop team orchestration | Move to Pantheon Runtime team contract | P1 |
| `pantheon_read_team_mailbox` | `architect/coordination.rs:228` | Desktop team orchestration | Move to Pantheon Runtime team contract | P1 |
| `pantheon_checkpoint_worker` | `architect/coordination.rs:295` | Desktop team orchestration | Move to Pantheon Runtime team contract | P1 |
| `pantheon_title` | `session_titles.rs:116` | Desktop metadata | Desktop adapter behind Pantheon contract | P3 |
| `pantheon_track_files` | `turn_changes/host.rs:52` | Desktop change tracking | Move contract to Pantheon API; desktop adapter owns UI | P2 |
| `pantheon_design_list` | `design.rs:405` | Desktop design UI | Desktop adapter behind Pantheon contract | P3 |
| `pantheon_design_write` | `design.rs:412` | Writes files from Desktop | Use native write_file contract; desktop adapter for artboards | P1 |
| `pantheon_design_comments` | `design.rs:415` | Desktop design UI | Desktop adapter behind Pantheon contract | P3 |
| `pantheon_design_resolve` | `design.rs:422` | Desktop design UI | Desktop adapter behind Pantheon contract | P3 |
| `pantheon_bot_remember` | `bots/tools.rs:111` | Overlaps native memory tools | Use Pantheon memory contract with bot scope | P1 |
| `pantheon_bot_forget` | `bots/tools.rs:118` | Overlaps native memory tools | Use Pantheon memory contract with bot scope | P1 |
| `pantheon_bot_recall` | `bots/tools.rs:124` | Overlaps native memory tools | Use Pantheon memory contract with bot scope | P1 |
| `pantheon_bot_schedule` | `bots/tools.rs:125` | Bot scheduler, no native equivalent | Implement native Pantheon scheduler contract | P2 |
| `pantheon_bot_list_schedules` | `bots/tools.rs:138` | Bot scheduler, no native equivalent | Implement native Pantheon scheduler contract | P2 |
| `pantheon_bot_cancel_schedule` | `bots/tools.rs:141` | Bot scheduler, no native equivalent | Implement native Pantheon scheduler contract | P2 |
| `pantheon_bot_notify` | `bots/tools.rs:103` | Gateway notification | Use Pantheon gateway/API contract | P2 |
| `pantheon_simulator_devices` | `simulator/tools.rs:238` | Desktop hardware adapter | First-party desktop adapter behind Pantheon contract | P3 |
| `pantheon_simulator_screenshot` | `simulator/tools.rs:239` | Desktop hardware adapter | First-party desktop adapter behind Pantheon contract | P3 |
| `pantheon_simulator_describe` | `simulator/tools.rs:246` | Desktop hardware adapter | First-party desktop adapter behind Pantheon contract | P3 |
| `pantheon_simulator_control` | `simulator/tools.rs:258` | Desktop hardware adapter | First-party desktop adapter behind Pantheon contract | P3 |
| `pantheon_simulator_manage` | `simulator/tools.rs:259` | Desktop hardware adapter | First-party desktop adapter behind Pantheon contract | P3 |
| `pantheon_vm_list` | `vm/tools.rs:290` | Desktop VM adapter | First-party desktop adapter behind Pantheon contract | P3 |
| `pantheon_vm_screenshot` | `vm/tools.rs:291` | Desktop VM adapter | First-party desktop adapter behind Pantheon contract | P3 |
| `pantheon_vm_control` | `vm/tools.rs:292` | Desktop VM adapter | First-party desktop adapter behind Pantheon contract | P3 |
| `pantheon_vm_exec` | `vm/tools.rs:305` | VM shell execution | Route through Pantheon execution boundary | P1 |
| `pantheon_vm_manage` | `vm/tools.rs:306` | Desktop VM adapter | First-party desktop adapter behind Pantheon contract | P3 |
| `pantheon_cloud_cli` | `cloud/deploy.rs:596` | Direct CLI execution | Route through Pantheon execution boundary and secret broker | P0 |

## Acceptance criteria

1. Every desktop agent tool name is in the Pantheon API tool contract.
2. No desktop catalog can register a tool outside that contract.
3. No duplicate tool names are accepted across chained desktop hosts.
4. Host tool execution goes through Pantheon authorization; no callback runs before its decision.
5. Every mutation and desktop UI effect has an explicit approval classification.
6. No alternative harness or provider can be selected or launched.
7. Native Pantheon equivalents replace desktop implementations for browser, code search, worktree, memory, subagent listing and terminal execution.
8. A desktop side adapter exists only for native window, hardware or UI operations, and its contract lives in Pantheon API.
9. Approval lifecycle events distinguish granted, denied, expired and cancelled.
10. All Pantheon tools intended for desktop use are discoverable through the desktop integration.

## Prioritized implementation stages

1. **Stage 1, tool contract:** add a Pantheon API contract for all 79 desktop adapter tools, with uniqueness and schema tests. Freeze new desktop tools outside the contract.
2. **Stage 2, host binding:** make `ToolHost` carry the validated catalog from Pantheon API and reject unknown, duplicate or malformed tools before MCP exposure.
3. **Stage 3, security boundary:** fix ACP auto grant, client MCP injection, approval lifecycle and fail closed MCP spawn policy.
4. **Stage 4, direct overlaps:** replace browser, code search, worktree, memory and subagent listing duplicates with native Pantheon tools.
5. **Stage 5, lifecycle migration:** move task, queue, steering, team and scheduler contracts into Pantheon API and Runtime, leaving desktop adapters.
6. **Stage 6, add ons and UI adapters:** remove Jev unless independently justified; map simulator, VM, design and local server tools to first party adapters behind Pantheon contracts.
7. **Stage 7, discovery:** make every Pantheon tool intended for desktop use discoverable through the desktop MCP integration and assert it in tests.
