# Goals and command workflows

Pantheon carries a goal in the session's system prompt. The desktop registers it
through the `_pantheon/goal` extension before a turn, then keeps the state
locally. Reading, pausing, resuming and clearing a goal answer from the store
and never start an agent.

`/goal` opens an editor with an objective and an optional token budget, and the
task menu exposes the same control while a task runs. Status is `active`,
`paused`, `blocked`, `usageLimited`, `budgetLimited` or `complete`. The banner
shows the reported state, token use, elapsed time and budget. Setting a goal
stores it and starts a turn so the model begins the work. Pause before opening
a Pantheon console. Goals cannot run inside an architect team; use the team
controls instead.

Goal state is validated against the saved task ID and stored separately from
the transcript. A task keeps at most one goal, and a goal needs an objective
under 32 KiB with a positive token budget when one is set. An interrupted
session recovers with the goal unchanged.

## Command panels and slash commands

The panels answer from local Pantheon sources. Usage comes from `pantheon stats
--json` over a one-year window; plugins come from `pantheon plugins list`; hooks
come from the data directory's `config.toml`; conversations come from this
app's own task list. The native caller supplies the workspace. The webview
cannot choose the workspace or supply an import body. Each panel caps its rows
at 200.

Slash commands come from the CLI's advertised catalog. Discovery opens a
throwaway `pantheon serve acp` session, reads the first
`available_commands_update` frame and sends no prompt. Names are capped at 128
characters and the catalog at 512 commands. Terminal-only commands open a
Pantheon console in the embedded terminal. Commands are inserted only when the
user presses insert, and the user submits them there after any login or trust
screens finish.
