# Pantheon Desktop

Tauri 2 workspace for Pantheon. Rust, SQLite, TypeScript, OS webview.

New tasks default to the Pantheon harness (`pantheon serve acp`). Other installed CLIs still show up through the same discovery layer.

## Build

Node ≥ 20, Rust stable, Tauri 2 system deps (`libwebkit2gtk-4.1-dev`, `libgtk-3-dev`, `libayatana-appindicator3-dev`).

```sh
npm install
npm run tauri dev
```

Needs a Pantheon binary on PATH, at `~/projects/pantheon/target/release/pantheon`, or at `~/.local/bin/pantheon`.

`npm run build` produces `.deb` / `.rpm` / `.AppImage`.

## Permissions and sandbox

A task's access mode selects a policy in the harness that runs it. This build
advertises one harness - `PROVIDERS` in `src-tauri/src/providers.rs` lists
`pantheon`, and named connections are validated against the same list - so the
shipped modes map like this (`src-tauri/src/providers/acp.rs`):

| Mode | What Pantheon does |
| --- | --- |
| Standard | Applies the CLI's `build` mode and shows every approval the runtime asks for in the approval bar. |
| Accept edits | Same, and edit calls are answered without asking. |
| Plan mode | `session/set_mode plan`: the CLI's host-enforced read-only posture. Edits and commands are declined, not merely unapproved. |
| Full access | Answers every permission request itself. The CLI's own policy preset can still refuse an action, so this removes prompts, not the runtime's gate. |

`src/approval-modes.ts` is the UI copy for those modes, and `review_supported`
in `providers.rs` decides which harnesses may start a task in `review`. Keep the
two in step: this build has no agent-reviewed mode, so offering `review` made
every pick fail at launch.

The same file still carries the launch code for the other harnesses Pantheon
knows (`HARNESSES`: Claude, Codex, Kimi, OpenCode, Grok, Muse, …). It is not
reachable from this build's UI, and it is what a wider catalog would use:

| Mode | Claude / GLM / Mimo | Codex |
| --- | --- | --- |
| Standard | `--permission-mode default` | `workspace-write`, `on-request` |
| Accept edits / Approve for me | `acceptEdits` / `auto` | `workspace-write`, auto review |
| Plan | `plan` + read-only tool allowlist | `read-only`, network off |
| Full access | `bypassPermissions` | `danger-full-access` |

Claude-family launches are also given `permissions.deny` rules inline with
`--settings` (`DENIED_TOOL_PATHS` in `providers.rs`): keys, cloud credentials,
credential helpers, password stores, browser profiles, keychains. The TUI gets
the same effect from its sandbox masks; the desktop gets it from the harness, and
only where the harness offers one. `bypassPermissions` (Full access) is
documented as skipping permission checks, so expect the deny list to be lifted
there: Full access means no masks.

What is **not** covered, in any build:

- The agent CLI runs as you, with your environment. Pantheon does not scrub it,
  so a secret exported in your shell is visible to the agent's shell tool.
- Codex's sandbox has no read-deny list: `workspace-write` lets it read anything
  your user can read, including `~/.ssh`.
- Bash-style reads of denied paths (`cat ~/.ssh/id_rsa`) are not covered by
  Claude Code's `Read`/`Edit` rules; that needs the harness's OS sandbox.
- Network egress follows the harness and your machine, not the TUI's allowlist.

## License

See `LICENSE`.
