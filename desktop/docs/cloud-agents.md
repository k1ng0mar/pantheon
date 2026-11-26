# Cloud agents

A task can run on a cloud machine instead of this computer. In a New task, the
context bar's computer menu lists Cloud targets: every provider connected in
Settings → Cloud (Daytona, Cloudflare, AWS EC2, Google Cloud, E2B, Modal, Vercel
Sandbox, CodeSandbox, Runloop, Blaxel, Tensorlake, Freestyle, Railway, boat and
InstaBox). Picking one and sending starts the task there. It then reads like any
other task, keeps running when the laptop sleeps or the app quits, and catches
up when it comes back.

## How it works

1. Machine. `cloud::sandbox::connected(provider)` returns a `Box<dyn Sandbox>`
   with list, create, power, state, exec, upload and download operations. There
   is one machine per cloud task, named `pc-<first 8 of task id>`. Its ID stays
   `provider:native`, so the VM panel sees it too.
2. Runtime. `cloud_agents::setup::ensure_runtime` uploads the same Linux
   Pantheon binary the desktop runs locally to `.pantheon/bin/pantheon`, after
   ensuring git, curl and tar. A marker in `~/.pantheon/runtime/pantheon`
   records the installed build. Nothing is fetched from npm or a second release,
   and any other agent name is refused.
3. Credentials. Pantheon signs in on this computer itself, so no login file is
   copied. A cloud run gets a git identity from local git config and, when `gh`
   is signed in, a scoped `GH_TOKEN` with a matching credential helper. Secret
   values are never logged.
4. Checkout. The task gets a local mirror worktree on branch
   `pantheon/cloud-<short>`, including uncommitted work as a first commit.
   `sync::upload_checkout` sends it as a git bundle to `$HOME/pantheon/<task id>`.
   After each run `sync::pull_changes` brings the machine's commits back and
   fast-forwards the mirror, so the Changes tab, PR flow and Run script work
   locally. Local edits in the mirror keep the cloud commits on
   `refs/pantheon/cloud/<task id>`.
5. Run. A small runner (`cloud_agents/runner.sh`) runs the agent detached: its
   stdin is an append-only `inbox`, and `out`, `err` and `exit` hold output and
   exit status in `~/.pantheon/runs/<run id>/`. The polling loop uses the same
   provider pipeline as a local task; only the transport differs. Local MCP host
   tools are not offered to cloud runs.
6. Sleep, quit, resume. Sleep pauses only the poller; keep-alive holds the
   machine up, capped by the provider's `max_unattended_minutes`. Quitting the
   app leaves the run alone. On startup, or when polling recovers, active cloud
   tasks replay their log from offset 0 into a fresh protocol without re-sending
   lines already in `inbox`. Stop kills the remote process group.
7. Idle. After a run ends, a 15-minute keep-alive lets the machine stop. The next
   prompt powers it on. A deleted or non-persistent machine is recreated from the
   mirror worktree and Pantheon's saved session state.

## State

`sessions.cloud` (schema 27) is a JSON object or NULL with `provider`, `machine`,
`remoteDir`, `base`, `stage`, `detail`, `runId`, `offset` and `inboxLines`.
`stage` is one of `creating`, `installing`, `credentials`, `uploading`, `ready`,
`running`, `reconnecting`, `syncing`, `stopped` or `error`. The session sent to
the webview carries `cloud` (`provider`, `label`, `machine`, `stage`, `detail`),
and each change emits `cloud-agent://progress` plus `sessions-changed`.

## IPC

- `cloud_agent_targets() → [{provider, label, ready}]` lists connected machines.
- `cloud_agent_start({sessionId?, projectId, provider, prompt, harness, model,
  mode, effort})` turns a New task draft into a cloud task, sends the first
  message at once and prepares the machine in the background. Images are refused.
- `cloud_agent_action({sessionId, action})` takes `sync`, `retry`, `stop`,
  `delete` or `open`. Follow-ups, stop, approvals and steering use the ordinary
  commands.

## Provider settings

Keyed providers connect through `cloud_connect(provider, key, options)`:

| Provider | Key | Options |
| --- | --- | --- |
| e2b | API key | none |
| modal | token secret | `tokenId` (required), `environment` |
| codesandbox | API key | none |
| runloop | API key | none |
| blaxel | API key | `workspace` (required) |
| tensorlake | API key | none |
| freestyle | API key | none |
| boat | API key | `apiUrl` |
| instabox | API key | machines from the `agent-browser` template; output is read back from a file because only its first 1 MiB is kept |
| vercel | existing deploy token | `projectId` and `teamId` for Sandbox |
| railway | existing deploy token | `workspace` (existing) |
| gcloud | gcloud CLI login | `zone` for Compute Engine VMs |

Non-secret options live in `cloud::Settings.providers[<id>]`, and a `detail`
entry there is shown in Settings. Secrets use `cloud::set_secret("<id>_key", value)`.

## Privacy

Nothing is sent anywhere until the user connects a provider and starts a cloud
task. A cloud task sends the checkout, the prompt and a scoped GitHub token to
the chosen provider's machine in the user's own account.
