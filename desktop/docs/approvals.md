# Approvals

Pantheon's ACP server offers two modes on the wire, `build` and `plan`. The
desktop shows three choices. Standard and Plan mode are Pantheon's own policy.
Full access is the only one the app sets itself: it answers every
`session/request_permission` with allow. The agent's own policy can still refuse
an action, even in Full access.

New user tasks start in Full access unless a mode is supplied. The chosen label
stays visible, and each menu row says what the mode allows. The mode is saved
with the task. A worker or agent-created task inherits its parent's mode, as
does a worktree or fork. Retrying a worker after lowering the parent's mode uses
the new one. Stop an active turn before changing its mode.

| Stored mode | Label | Behavior |
| --- | --- | --- |
| `ask` | Standard | Work in this project; Pantheon asks before running a tool its policy requires approval for. |
| `plan` | Plan mode | Read-only; Pantheon investigates and proposes a plan, and declines edits and commands. |
| `full` | Full access | The app approves every permission Pantheon asks for, including outside this project. |

Two older choices remain for tasks that saved them. `auto` (Accept edits) and
`review` (Approve for me) appear disabled with a reason, and `review_supported`
in `providers.rs` refuses to start a run that still holds one. Pantheon has no
edit-approval mode and no agent-reviewed approvals.

A mode is applied through ACP before the turn, never simulated by the UI.
Returning to Standard restores that policy on the next turn, including resumed
sessions. A failed policy change stops the turn instead of running under a
different one. Structured questions still need an answer; Full access does not
pick one for the user.

Native console permissions belong to the CLI. Opening a Pantheon console needs
an inactive goal, and Pantheon's own policy still applies there. Terminal-only
output stays in the console history.
