# Memory

Durable memory scoped to an environment, an agent, a project, or a task — with provenance and trust on every record, so recalled content is never mistaken for instructions.

## Layers

```
GLOBAL → AGENT → PROJECT → TASK/SESSION → EPHEMERAL (this turn only)
```

Recall searches all layers, narrowest first, each hit carrying where it came from and when it was written.

## Budgets

Every layer has a byte budget. A write that would exceed it evicts lowest-trust, oldest-first records until the write fits — recall stays fast and bounded no matter how much the agent remembers. A write that can't fit even after eviction fails with `MEM_BUDGET_EXCEEDED` instead of growing the store.

## Writes go through the gate

One path for the model, the CLI, and imports alike:

```
propose → policy check → provenance attach → validation → store
```

Writing needs the `MemoryWrite` capability (`coder_memory` policy or an explicit grant). Nothing the model wants remembered bypasses it — no silent prompt-injection writes, ever.

## Trust

| Tier | Meaning |
|---|---|
| `system` | Harness-authored, authoritative |
| `user` | You wrote it (`put`, `/remember`, edited `MEMORY.md`) |
| `memory` | User-confirmed (`memory confirm`) |
| `untrusted` | Model-proposed or tool/web-derived |

Trust never transfers by copying. A proposal lands `untrusted` no matter what it claims; only your explicit action promotes it. A document telling the agent to "ignore previous instructions" is data, not an order.

## Commands

```sh
pantheon memory put KEY VALUE
pantheon memory recall QUERY [--ns NAME]   # search; another agent's namespace on request
pantheon memory confirm KEY
pantheon memory list
pantheon memory import|export|sync [FILE]  # markdown round-trip; sync refuses on conflict
pantheon memory backend list|select NAME
```

## Learning into skills

Recurring experience shouldn't be re-solved every time. When an agent discovers a reliable procedure, it becomes an inspectable artifact — documented, versioned, reusable:

```
Work → discovery → documentation → reuse → refinement
```

Skills are portable knowledge (instructions, workflows, examples). They explain how; they never authorize — the runtime still decides whether the agent may act. Manage them with `pantheon skills list|import|doctor`.

## See also

- [Agents](agents.md) — memory scoping per identity
- [Extensions](extensions.md) — hooks and executable plugins
- [Terminal reference](../reference/terminal.md#memory) — full flags
