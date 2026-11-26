# Agent tools

Pantheon launches the installed CLI with `pantheon serve acp`. Shell, file
editing, search, image inspection and MCP integrations come from that CLI's own
version and configuration. The desktop does not simulate them or enable tools
the user turned off.

## App tools

Each new task registers `pantheon_*` dynamic tools through ACP: project and task
discovery, task history and subagent activity, creating, starting and following
up tasks, board stages, worktrees, opening files and terminals, local ports,
browser navigation and screenshots, and moodboard lookup. See
[Moodboards](moodboards.md).

Task operations stay inside the current project. File previews use the task's
checkout and the existing native path checks. A new worktree creates a task under
its original project. Starting another agent uses the same admission and
concurrency limits as the UI, and starting a follow-up on the running task is
rejected. Use the CLI's shell tool for commands and its image tool on the local
PNG returned by `pantheon_capture_browser`.

`pantheon_code_search` searches the task's checkout with the third-party `codedb`
CLI when it is installed and on PATH. It runs one bounded process per query (64
KiB of output, 30 seconds, `CODEDB_NO_TELEMETRY=1`), writes nothing into the
project and keeps its index in its own home directory. Its results are
untrusted data. Without codedb the tool says so and the agent keeps its own
search tools.

## Approvals and browser access

Host callbacks run outside the agent's process, so the desktop applies its own
approval boundary. Mutations and browser actions ask in Standard mode and are
unavailable in Plan mode. Full access answers every `session/request_permission`
with allow. The agent's own policy can still refuse an action. Browser access
requires the calling task to be visible, so a background task cannot take
another task's browser. The browser interface accepts a finite set of actions
and data, never agent-supplied JavaScript or native IPC. Remote page text is
untrusted, and remote webviews get no application IPC.

Callbacks run asynchronously, eight at a time, with at most 64 admitted per run
and bounded arguments, results, timeouts and cancellation. A slow callback
cannot block the agent's stdout.

## Tools and integrations

The installed CLI persists dynamic tools with newly created threads. Its
`session/resume` cannot add them to a thread created without them, so tasks
started before a tool was added keep their older tool set. Start a new task to
pick up new app tools.

MCP servers and skills are configured through the Pantheon CLI: `pantheon tools
mcp list`, `pantheon tools mcp status` and `pantheon tools skills list`. The
desktop's tools panel cannot enumerate them automatically. The next task run
inherits configuration changed in the CLI. Questions and MCP elicitations that
the CLI supports are isolated by run and task; drafts survive streaming and
Stop clears them. Secret inputs use password fields and are not saved in the
transcript. An unsupported form constraint returns a protocol error instead of
an incomplete answer, and authorizing a URL always takes a separate open and
confirm action.
