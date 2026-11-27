# Providers

Pantheon is the only agent this desktop launches. It runs the installed `pantheon`
CLI with `pantheon serve acp`, a stdio JSON-RPC session using ACP version 1.
The task's mode and the selected model or effort go over ACP (`session/set_mode`,
`session/set_model`, `session/set_config_option`), never in command arguments. A
resumed task keeps the policy it was saved with.

Install Pantheon and sign in through the CLI. The CLI owns the provider registry
and credentials; the desktop stores no provider API keys. Pantheon finds the
executable on PATH and in common install folders, and Settings → Providers can
override the path in `providers.json`. `default` leaves model choice to the CLI.

## Models and discovery

Model menus load on demand from the installed CLI. Discovery opens a throwaway
ACP session, reads catalog metadata, and sends no prompt and no user thread.
Only picker metadata crosses the bridge; account data in the initialization
response is discarded. The frontend caches a successful catalog for five minutes
and does not poll in the background. A failed or empty catalog leaves the saved
choice and any typed model ID usable.

Effort choices come from the model's advertised config options. `Default` sends
no override, so the CLI keeps control, including a saved effort in a resumed
session. Context usage comes from ACP usage updates when Pantheon reports them;
unknown usage stays hidden. Counts are the last reported snapshot and can drop
after compaction.

## Named connections

Settings → Providers → Connections holds named setups of Pantheon, for example
a work and a personal one. Each connection has a name, an optional executable,
extra arguments (at most 32, each under 1024 characters, no NUL bytes), and
environment variables marked Secret or plain. Non-secret values live in
`connections.json`; secret values live in the OS secure store under
`app.pantheon.connections`, one account per variable, and never reach the
webview or logs. An edit that leaves a secret blank keeps the saved value.

Assign a connection from the model picker. The assignment is stored per task,
and a task never switches on its own. At launch the connection supplies the
executable and arguments. Its environment is applied after the global setup,
and secret values are redacted from returned errors. Deleting a connection
removes its secrets and returns its tasks to the global setup.

## Sessions and streaming

Each task keeps one ACP session. Its saved session ID is reused across turns and
app restarts, so a run resumes instead of starting over. One process serves each
active turn; finishing closes stdin, and Stop sends `session/cancel` before the
process group is terminated after a short grace period. Closing the app drops
the input channel and stops the group. Frames are capped at 4 MiB with a 16 KiB
stderr tail.

The adapter forwards complete `session/update` frames as stdout supplies them.
Text deltas build one live assistant message. Tool calls and results appear as
bounded snapshots: input previews are 4 KiB and output previews 8 KiB, with at
most 64 held per run. Subagent activity arrives as updates with
`kind: "subagent"` and stays isolated from the parent's response. Each task owns
its own protocol state, so stopping one run never interrupts another. Admission
rejects duplicate starts of the same task before adding a message or spawning a
process.

## Secrets on this computer

`src-tauri/src/secret_store.rs` holds connection secrets, cloud keys, cloud
settings and bot channel tokens. Provider credentials are not here: they stay
in the Pantheon CLI's own store. On macOS the desktop writes a 0600
`secrets.json` in its app data folder, keyed by service and account, with atomic
writes under a process lock. Linux uses the Secret Service and Windows uses
Credential Manager. Values never enter prompts, logs, the database or the
webview.

## Checks

`cargo test` runs parser and permission tests. The live smoke test needs a
signed-in CLI:

```sh
PANTHEON_SMOKE_PROVIDER=pantheon cargo test smoke_provider_from_env -- --ignored --nocapture
```

It creates a temporary Git repository, asks for one fixed token, resumes the
session, rejects every approval request and checks that the same session ID is
reused. Set `PANTHEON_SMOKE_MODEL` to target a model without changing settings.
