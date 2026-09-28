# Contributing

Where code belongs, how to test it, and the three most common kinds of change. Read [Architecture](architecture.md) first for the design reasoning; this page is the mechanical how.

## Where code belongs

| You are adding... | It belongs in... | Because |
|---|---|---|
| An event type | `pantheon-api/src/events.rs` | The Event enum is the single source of truth |
| A capability or policy preset | `pantheon-api` capability/policy | Policies are defined in code on purpose |
| A model provider adapter | `pantheon-providers` | Transport + normalization only; no business logic |
| A built-in tool | `pantheon-tools/src/builtins.rs` | Tools declare capabilities; the gate applies them |
| A plugin manifest feature | `pantheon-extensions` | Manifest, hooks, manager live together |
| A storage table or migration | `pantheon-storage` | SQLite owns all durable state |
| A memory layer change | `pantheon-memory` | The write path stays suggest → permission → label the source |
| A terminal command | `pantheon-tui` dispatch + its own module | Thin surface; logic goes in the owning crate |
| A channel surface | `pantheon-gateway` or `pantheon-api` | The channel seam normalizes; adapters format |

Rules that keep the architecture honest:

- **No business logic in pantheon-tui.** It parses arguments and calls into crates.
- **No model SDKs above pantheon-providers.** The agent loop speaks ModelTurn/ModelEvent, never HTTP.
- **Events over prints.** A meaningful state change emits an Event; stderr is for operator warnings only.
- **Durable before clever.** New long-running work gets a proper state machine in the database, not an in-memory variable.

## Testing

- `cargo test --workspace`, the workspace test gate
- `python3 eval/run.py`, eval cases
- Nothing ships until both are green.

## Common changes

### Adding a terminal command

1. Add dispatch in `pantheon-tui/src/terminal.rs` + register in `KNOWN_VERBS` and `usage()`
2. Implement the logic in the owning crate
3. Add tests (`*_tests.rs`)
4. Document it in `docs/reference/terminal.md`
5. Green workspace tests + evals

### Adding a tool

1. Define it in `pantheon-tools/src/builtins.rs`
2. Declare its capability in `pantheon-api`
3. Add behavior tests
4. Document user-visible tools in the relevant user-guide page

### Adding a provider

1. Add the adapter in `pantheon-providers/`, register in `catalog.yaml`
2. Add model definitions and adapter tests
3. Keep the agent loop on ModelTurn/ModelEvent only

## Decisions

Known deviations are recorded in `decisions/0001-workspace-restructure.md`. Review it before architectural changes.

## See also

- [Architecture](architecture.md): system design, crate map, locked decisions
- [Terminal reference](../reference/terminal.md): the surface your command joins
