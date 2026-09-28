# Agents

An agent is a persistent identity: its own instructions, memory, skills, tools, and model configuration, living in one shared runtime. Change the model, the interface, or the machine, the agent remains itself.

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

`pantheon setup` creates the `default` agent. Add tables for specialists, a researcher, a coder, a planner, each inheriting shared defaults while keeping its own identity, memory namespace, and policy. Select the active profile with `agent = "zeus"`. No `[agents]` table at all means anonymous runs, as before.

Field details: [Configuration](../reference/configuration.md#agents).

## Work together

One agent can hand a subtask to another mid-turn, delegate, divide, review, combine, by naming it in plain language:

> "Ask Zeus to research the error handling, then combine it with what you found."

The child runs as the named agent, its own identity, memory, and policy, and returns a transcript fragment. The parent never fabricates its result: a child that stalls on approval, budget, or a further delegation reports a structured error (`SWARM_CHILD_APPROVAL`, `SWARM_CHILD_BUDGET`, `SWARM_CHILD_DELEGATED`) instead of silently dying.

Delegation is runtime-capped: delegation depth is bounded (`max_delegate_depth`, default 2) alongside concurrency and budgets, and over-cap fails with `SWARM_SPAWN_DENIED`.

```sh
pantheon swarm status [<id>] | list   # inspect recorded swarms; the verb doesn't spawn
```

## See also

- [Memory](memory.md), what each agent remembers, and how it learns
- [Runs](runs.md), how agent work executes and recovers
- [Configuration](../reference/configuration.md), all agent fields
