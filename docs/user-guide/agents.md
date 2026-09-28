# Agents

An agent is a named assistant with its own personality, memory, skills, and settings, all living in one shared Pantheon. Change the model or the machine, and the agent is still itself.

## Create one

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

`pantheon setup` creates the `default` agent for you. Add more for specialists: a researcher, a coder, a planner. Each one inherits the shared defaults but keeps its own name, memory, and permission level. Pick the active one with `agent = "zeus"`. If you define no agents at all, Pantheon just runs anonymously, like before.

All the fields are explained in [Configuration](../reference/configuration.md#agents).

## Working together

One agent can hand a subtask to another in the middle of a conversation, just by naming it in plain language:

> "Ask Zeus to research the error handling, then combine it with what you found."

The child runs as that agent, with its own personality and memory, and hands back what it found. It never makes up a result: if it gets stuck waiting for permission or runs out of budget, it reports a clear error instead of going quiet.

There are safety rails: delegation can only nest so deep (configurable, default 2), and too much at once is refused rather than allowed to spiral.

```sh
pantheon swarm status [<id>] | list   # see past collaborations; this command never starts new work
```

## See also

- [Memory](memory.md): what each agent remembers, and how it learns
- [Runs](runs.md): how agent work runs and recovers
- [Configuration](../reference/configuration.md): every agent setting
