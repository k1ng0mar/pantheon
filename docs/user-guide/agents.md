# Agents

An agent is a persistent identity: its own instructions, memory, skills, tools, and model configuration, living in one shared runtime. Change the model, the interface, or the machine — the agent remains itself.

## Declare one

```toml
[agents.default]
display_name = "Default"
agents_file  = "agents/default/AGENTS.md"
soul_file    = "agents/default/SOUL.md"
policy       = "coder_memory"

[agents.zeus]
inherits     = "default"
display_name = "Zeus"
soul_file    = "agents/zeus/SOUL.md"
```

`pantheon setup` creates the `default` agent. Add tables for specialists — a researcher, a coder, a planner — each inheriting shared defaults while keeping its own identity, memory namespace, and policy. Select the active profile with `agent = "zeus"`. No `[agents]` table at all means anonymous runs, as before.

Field details: [Configuration](../reference/configuration.md#agents).

## Work together

Multiple agents in one installation collaborate natively: delegate, divide, review, combine. Ask for it in plain language:

> "Work with Zeus and Athena on this. Split the research, then combine the results."

Each keeps its own perspective and memory while participating in the larger task. Spawns are runtime-capped (depth, concurrency, budgets); over-cap fails with `SWARM_SPAWN_DENIED` instead of exploding.

```sh
pantheon swarm 5 "research this topic"
```

## See also

- [Memory](memory.md) — what each agent remembers, and how it learns
- [Runs](runs.md) — how agent work executes and recovers
- [Configuration](../reference/configuration.md) — all agent fields
