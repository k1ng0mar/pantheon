# Pantheon Desktop

Tauri 2 workspace for Pantheon. Rust, SQLite, TypeScript, OS webview.

Tasks run Pantheon (`pantheon serve acp`). The desktop discovers no other
agent and launches none.

## Build

Node ≥ 20, Rust stable, Tauri 2 system deps (`libwebkit2gtk-4.1-dev`, `libgtk-3-dev`, `libayatana-appindicator3-dev`).

```sh
npm install
npm run tauri dev
```

Needs a Pantheon binary on PATH, at `~/projects/pantheon/target/release/pantheon`, or at `~/.local/bin/pantheon`.

`npm run build` produces `.deb` / `.rpm` / `.AppImage`.

## Permissions and sandbox

A task's access mode selects a policy in Pantheon. This build advertises one
agent (`PROVIDERS` in `src-tauri/src/providers.rs` lists `pantheon`, and named
connections are checked against the same list), so the shipped modes map like
this (`src-tauri/src/providers/acp.rs`):

| Mode | What Pantheon does |
| --- | --- |
| Standard | Applies Pantheon's `build` mode and shows every approval the runtime asks for in the approval bar. |
| Accept edits | Same, and edit calls are answered without asking. |
| Plan mode | `session/set_mode plan`: Pantheon's host-enforced read-only posture. Edits and commands are declined, not merely unapproved. |
| Full access | Answers every permission request itself. Pantheon's own policy preset can still refuse an action, so this removes prompts, not the runtime's gate. |

`src/approval-modes.ts` is the UI copy for those modes. This build has no
agent-reviewed mode, so offering `review` made every pick fail at launch.

What is not covered:

- Pantheon runs as you, with your environment. A secret exported in your shell
  is visible to the agent's shell tool.
- A model-side read deny rule does not stop shell reads such as `cat
  ~/.ssh/id_rsa`; that depends on the OS sandbox.
- Network egress follows Pantheon and your machine, not the TUI's allowlist.
- Full access removes approval prompts, not Pantheon's own policy gate.

## License

See `LICENSE`.
