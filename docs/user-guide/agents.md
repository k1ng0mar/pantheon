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

To open the terminal as a different agent for one session without changing the config:

```sh
pantheon --profile zeus   # -p zeus and --agent zeus work too
```

An unknown name fails before the terminal opens, with the fix spelled out - it never silently falls back to another agent.

All the fields are explained in [Configuration](../reference/configuration.md#agents).

## Working together

One agent can hand a subtask to another in the middle of a conversation, just by naming it in plain language:

> "Ask Zeus to research the error handling, then combine it with what you found."

The child runs as that agent, with its own personality and memory, and hands back what it found. It never makes up a result: if it gets stuck waiting for permission or runs out of budget, it reports a clear error instead of going quiet.

There are safety rails: delegation can only nest so deep (configurable, default 2), and too much at once is refused rather than allowed to spiral.

Every spawned child gets a self-contained briefing: its own identity, its persona and instruction files inlined verbatim (nothing arrives as a bare path it may or may not read), your active `/goal`, and a fixed result contract. The child reports back in a machine-parseable envelope - `status` (`completed`, `partial`, `failed`, or `unknown`), files changed, a summary of what it actually did, its decisions, open questions, and follow-ups. Free-text replies that skip the envelope degrade to `unknown` and are never treated as done.

Optionally, add a `[verify]` section to your config and each child's claimed result is checked by a separate adversarial model before you see it: the verifier assumes the goal was *not* achieved and must be convinced otherwise. A falsified claim fails the delegation outright; an inconclusive one is marked unverified, never accepted as complete. Verification is opt-in - without `[verify]` there is no verifier and results arrive with no verification mark. See [Configuration](../reference/configuration.md) for the section shape.

```sh
pantheon swarm status [<id>] | list   # see past collaborations; this command never starts new work
```

## See also

- [Memory](memory.md): what each agent remembers, and how it learns
- [Runs](runs.md): how agent work runs and recovers
- [Configuration](../reference/configuration.md): every agent setting
