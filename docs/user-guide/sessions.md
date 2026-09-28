# Sessions

The terminal app is how you talk to Pantheon directly. Everything is written down as it happens, so conversations survive exits, crashes, and switching to your phone.

## Open a session

```sh
pantheon
```

That opens the app. It needs a real terminal; without one it tells you to use `pantheon run` instead. Your last conversation picks up where you left off. Type `/help` to see every command.

## The interface

- **Status bar**: shows how much of the conversation fits in the model's memory, what the last reply cost, and which model is active.
- **Tabs** (`Ctrl+Tab`, `Alt+1..9`): keep several conversations open. A reply finishing in another tab shows a badge instead of yanking you over.
- **Turn timeline** (`Ctrl+O`): scroll back through the turns of a long conversation.
- **Rewind** (double-`Esc`, then confirm): undo the last turn in the view. History is never rewritten; the rewind is recorded as its own entry.
- **Editor** (`Ctrl+E`): a fullscreen editor for writing long messages.

Useful commands: `/models` (browse AI models, Enter switches), `/model [provider]` (show or change the current model), `/reasoning [off|minimal|low|medium|high|xhigh|max]` (how hard the model thinks), `/runs` (browse conversations), `/resume <id>` (open one), `/history`, `/status`, `/name <title>`, `/agent [name]`, `/agents`, `/remember KEY TEXT` (save a memory), `/skills [filter]`, `/settings`, `/gateway`, `/doctor`, `/sessions` (live sessions), `/new`, `/compress`, `/export [markdown|json]`, `/clear`, `/exit`, `/goal [text]` (set a goal for this session), `/tokens [n|off]` (cap spending), `/set [key value]` (change limits on the fly).

Open a specific conversation from the shell: `pantheon --resume [id]`.

## Saying yes or no

When Pantheon wants to do something that matters, like deleting files or pushing code, the conversation pauses and shows a permission card: `y` allows it, `n` refuses. Permission covers that one action only, never a blank check.

Away from the terminal? Answer from anywhere:

```sh
pantheon run --taskID <id> --grant <scope>   # allow it, and the work continues
pantheon run --taskID <id> --deny  <scope>   # refuse it; the refusal is written into the conversation
```

Saying no does not kill the conversation. The model sees "denied by operator" and works around it.

## Tasks without the app

```sh
pantheon run --taskID <id> --say "text" --deliver session|telegram|discord
```

One real reply from the model, printed here or sent to your phone. The request is saved first, so it survives even if the messaging service is down.

## Looking back

```sh
pantheon runs                 # all conversations, with status and title
pantheon runs <id>            # the full record of one, in plain words
pantheon runs <id> --metrics  # counts: turns, tool uses, permissions
pantheon audit <id> [out]     # machine-readable record for scripts
pantheon logs [errors] [-f]   # what the program has been doing
```

`runs` answers "why did that conversation end that way". `logs` answers "what has the program been doing". A run is `running`, `awaiting_approval`, `completed`, `failed`, or `canceled`.

## See also

- [Runs](runs.md): how work happens, recovers, and gets scheduled
- [Channels](channels.md): messaging apps and the web page
- [Terminal reference](../reference/terminal.md): every flag and exit code
