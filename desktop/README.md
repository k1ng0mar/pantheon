# Pantheon Desktop

> This is a fork of [PandaCode](https://github.com/aicodeking/pandacode) —
> an open-source desktop workspace for coding agents (Tauri 2, Rust, SQLite,
> plain TypeScript, OS webview). The fork retargets that workspace at
> [Pantheon](../pantheon), the durable runtime for AI agents: Pantheon is the
> primary — and intended default — harness this app drives.

## What changed from upstream

- **Pantheon harness registered** (`src-tauri/src/providers.rs`): launches
  `pantheon serve acp` — Pantheon's Agent Client Protocol server (stdio
  JSON-RPC; one ACP session = one Pantheon run; approval-gated tool calls
  arrive as `session/request_permission`). Discovery checks PATH,
  `~/projects/pantheon/target/release/pantheon`, and `~/.local/bin/pantheon`.
- **Default harness is Pantheon** for new tasks (`storage.rs`), Pantheon sits
  first in the provider list, and it is in the executable-override allowlist.
- Identity: productName "Pantheon", identifier `app.pantheon.desktop`.
- Upstream's other harnesses (Claude Code, Codex, OpenCode, …) remain
  dormant-but-functional via the existing discovery layer: they only appear
  when their CLIs are installed. Stripping or hiding them is a later,
  deliberate product decision — not silently broken plumbing.

The multi-harness plumbing is kept intentionally: Pantheon's own design keeps
model/runtime authority in the runtime, and the shell's job (tasks, worktrees,
diffs per turn, steering, approvals bar, displays) is the part worth reusing.

## Build

Same toolchain as upstream: Node ≥ 20, Rust stable, Tauri 2 system deps
(`libwebkit2gtk-4.1-dev`, `libgtk-3-dev`, `libayatana-appindicator3-dev`).

```sh
npm install
npm run tauri dev        # first compile of the dep tree is the slow part
```

Build times: first `tauri dev` compiles the dependency tree once per profile;
after that, incremental runs are seconds. `npm run build` (release) produces
registered, installable bundles (`.deb`/`.rpm`/`.AppImage`).

Prereq for the harness: a Pantheon binary (`cargo build --release` in the
Pantheon repo) — discovery picks it up from the checkout path or PATH.

## License

PandaCode License (MIT with an attribution requirement) — see `LICENSE`.
This fork keeps that license and carries the attribution notice above.
