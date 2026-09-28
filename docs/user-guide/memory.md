# Memory

Pantheon remembers things for you: your name, your preferences, facts from past conversations. Every memory says where it came from and how much to trust it, so something the model guessed is never treated like something you said.

## Layers

Memories live at different scopes, from broad to narrow:

```
GLOBAL → AGENT → PROJECT → TASK → EPHEMERAL (this conversation only)
```

When Pantheon looks something up, it searches all layers, most specific first. Every hit shows where it was written and when.

## Limits

Each layer has a size limit. When a write would go over, the oldest, least-trusted memories are dropped until it fits. Recall stays fast no matter how much it remembers. If a write cannot fit at all, it fails loudly instead of silently growing.

## How a memory gets written

One path, whoever asks: you, the model, or an import:

```
suggest → permission check → label the source → validate → store
```

Writing needs permission (`coder_memory` policy, or an explicit grant). The model can never quietly write a memory for itself. Nothing from a web page or a document can tell it to "forget its instructions": that text is data, not an order.

## Trust

| Level | Meaning |
|---|---|
| `system` | Written by Pantheon itself, authoritative |
| `user` | You wrote it (`put`, `/remember`, edited `MEMORY.md`) |
| `memory` | You confirmed it (`memory confirm`) |
| `untrusted` | Suggested by the model or pulled from a tool or web page |

Trust never transfers by copying. A suggestion lands as `untrusted` no matter what it claims to be. Only you can promote it.

## Commands

```sh
pantheon memory put KEY VALUE
pantheon memory recall QUERY [--ns NAME]   # search; another agent's memories on request
pantheon memory confirm KEY
pantheon memory list
pantheon memory import|export|sync [FILE]  # plain markdown round-trip
pantheon memory backend list|select NAME
```

## Turning experience into skills

When Pantheon discovers a procedure that works reliably, it can be saved as a skill: a documented, reusable how-to. Skills explain how to do something; they never grant permission to do it. That decision still belongs to you and your policy.

```sh
pantheon skills list|import|doctor
```

## See also

- [Agents](agents.md): whose memory is whose
- [Extensions](extensions.md): plugins, the other half of extensibility
- [Terminal reference](../reference/terminal.md#memory): full flags
