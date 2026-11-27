# Slash commands

Type `/` at the start of a message in Conversation or the Board task panel.
The menu shows names, descriptions and argument hints from Pantheon. Type to
filter, then use Up and Down to move and Tab or Enter to insert. Selection never
starts a run. Add arguments and send the message separately. Escape dismisses
the menu. The Refresh button reloads discovery.

## Discovery

Pantheon advertises its commands over ACP in `available_commands_update`.
`slash_commands/acp.rs` reads that catalog for the saved task. The desktop does
not copy Pantheon's configuration, command files or credentials; it asks the
agent for command metadata. Local and cached commands appear immediately and
stay selectable while the agent starts. A slow start shows a message, and a
stalled request offers Retry without clearing the commands already available.

Pantheon's CLI handles the commands it advertises, so typing one sends it to
Pantheon as a prompt. It does not open a desktop panel.

Known app controls such as `/new`, `/model`, `/permissions`, `/settings`,
`/theme`, `/diff`, `/resume`, `/rename <title>`, `/copy` and `/pwd` are marked
Pantheon. They open or operate existing controls without adding a model turn or
consuming attachments. `/model` and `/permissions` open the respective pickers;
they do not accept inline configuration arguments.

## Limits

- Discovery runs on demand. The desktop caches a successful catalog for five
  minutes, bounds concurrent lookups and process lifetimes, and adds no
  background polling or filesystem crawler.
- Only recognized metadata reaches the renderer. Command names and collections
  are bounded, and discovery sends no model prompt, authentication request or
  reverse tool call.
- Local and cached commands stay selectable while Pantheon starts, and a failed
  refresh keeps the last good catalog when one exists.
- Sends resolve commands again in native code, then check the saved task,
  checkout and original message before saving or claiming the task. Normal
  approval behavior still applies to work requested by a command.

The menu updates existing row nodes and retains textarea focus. Catalog updates
are separate from transcript streaming. Async lookups cannot dispatch a local
control or collect files from a task selected in the meantime.
