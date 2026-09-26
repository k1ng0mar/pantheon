# Contributing

Crate boundaries, testing rules, and the three most common kinds of
change. Read ARCHITECTURE.md first for the design positions; this page is
the mechanical how.

## Crate boundaries (what goes where)

| You are adding... | It belongs in... | Because |
|---|---|---|
| An event variant | `pantheon-core/src/events.rs` | The Event enum is the single source of truth; every other crate consumes it |
| A capability or policy preset | `pantheon-core/src/capability.rs` | Policies are code-defined on purpose |
| A model provider adapter | `pantheon-providers` | Transport + normalization only; no business logic |
| A built-in tool | `pantheon-exec/src/builtins.rs` | Tools declare capabilities; the gate applies them |
| A plugin manifest feature | `pantheon-extensions` | Manifest, hooks, manager live together |
| A storage table or migration | `pantheon-storage` | SQLite owns all durable state |
| A memory layer change | `pantheon-memory` | Write path stays propose->policy->provenance |
| A CLI verb | `pantheon-cli/src/main.rs` dispatch + its own module | Thin surface; business logic goes in the crate that owns it |
| An agent surface (Discord/Telegram/web) | `pantheon-gateway` or `pantheon-api` | The channel seam normalizes; adapters format |

Rules that keep the architecture honest:

- **No business logic in pantheon-cli.** It parses arguments and calls
  into crates. If your verb needs logic, the logic goes downstream.
- **No model SDKs above pantheon-providers.** The agent loop speaks
  ModelTurn/ModelEvent, never HTTP.
- **Events over prints.** If a runtime transition matters, emit an Event.
  eprintln is for operator warnings only.
- **Durable before clever.** New long-running work gets an operation
  state machine (pantheon-storage/src/operations.rs), not an in-memory
  flag.

## Testing rules

1. Every behavior change ships with a test. Unit test next to the code;
   behavior that spans crates goes in the eval harness.
2. `cargo test --workspace` must pass with **zero warnings**. Warnings
   are gate failures here, not noise.
3. `python3 eval/run.py` drives the real binary in fresh sandboxes. New
   CLI verbs get an eval case. Look at eval/cases.json for the shape;
   `_`-prefixed commands are probes (see eval/run.py).
4. Determinism: unit tests never touch the network. Use scripted
   `ChatTransport` test doubles local to the test file. Anything needing a
   live model is behind an env check (see router_streaming test).
5. Concurrency code gets a race test (see leases.rs
   `cross_connection_acquire_has_one_winner` for the pattern: two
   connections, a Barrier, assert one winner).

## Adding a CLI verb

1. Create `crates/pantheon-cli/src/<verb>_cli.rs` with
   `pub fn cmd_<verb>(args: &[String])`.
2. Parse with `crate::cli_args::Args` (both `--f v` and `--f=v` work).
3. Call into the owning crate. Print structured codes on failure, exit 1
   for operation errors, exit 2 for usage.
4. Register the module and the dispatch arm in main.rs.
5. Add the verb to the help() list and to docs/cli.md.
6. Add an eval case.

## Adding an event

1. Add the variant in pantheon-core/src/events.rs with doc comments
   saying when it fires and what is persisted.
2. Match it in: stream.rs (frame projection), audit.rs (export),
   otel span_for (mapping), ledger.rs status projection if it affects
   run state.
3. Emit it from exactly one place. If two places can emit the same
   event, one of them is wrong.
4. Tests: replay round-trip (append -> replay -> assert).

## Adding a tool

1. Define the schema (name, description, JSON parameters).
2. Pick the capability honestly. If it writes files, it is
   FilesystemWrite. Do not route new powers through an existing weaker
   capability.
3. Register in the registry; the gate and budgets apply automatically.
4. Keep tool output bounded: large output goes through compaction
   (`compact_output`), never raw into the transcript.

## Style

- rustfmt via `cargo fmt` before every commit.
- Module doc comments explain WHY (contracts, invariants), not WHAT
  (the code says what).
- Error helper per module (`fn xerr(code, cause)`) so codes stay
  consistent; error codes are uppercase snake and are part of the
  public interface (docs/troubleshooting.md lists them).
- No new async. The codebase is std threads by decision; if you need
  async for a dependency, isolate it behind a blocking trait.
