# Cline lessons for Pantheon

Study target: `/home/ubuntu/.hermes/cache/scratch/harness-study/cline`, with emphasis on `sdk/` and current runtime code.

This note records source-backed engineering lessons for Pantheon. Paths are relative to the Cline repository.

## 1. Overall stack and layout

The current Cline codebase is a monorepo with two related layers:

- `sdk/packages/shared`: contracts, schemas, storage paths, hooks, extension contracts, message and tool types.
- `sdk/packages/llms`: provider settings, model catalog, gateway/provider execution.
- `sdk/packages/agents`: browser-safe, stateless agent loop and tool orchestration.
- `sdk/packages/core`: stateful runtime composition, sessions, persistence, plugins, MCP, checkpoints, compaction, telemetry, hub services.
- `apps/cli`: terminal and headless host. `apps/cli/src/runtime/` contains interactive session orchestration and CLI approval plumbing.
- `apps/vscode`: VS Code host and compatibility layer. `apps/vscode/src/sdk/SdkController.ts` owns UI-facing session and checkpoint actions.
- `apps/examples`: desktop app, marketplace UI, code-review and multi-agent examples.
- `evals`: smoke and benchmark harnesses.

The dependency direction is explicit in `sdk/ARCHITECTURE.md`: `shared <- llms <- agents <- core <- host apps`, with `agents` stateless and `core` owning persistence and host lifecycle. Pantheon has a similar split in crates, but its `pantheon-agent` loop currently passes a `Vec<String>` transcript, while Cline uses typed messages and typed event snapshots.

## Scope note

The current Cline SDK does not use a separate shadow Git clone. It stores stash-compatible snapshot commits in the workspace repository under private `refs/cline/checkpoints/...` refs, with a private scratch index for untracked files. Older descriptions may call this a shadow repository, but the current implementation is private refs plus a transaction-based restore path.
