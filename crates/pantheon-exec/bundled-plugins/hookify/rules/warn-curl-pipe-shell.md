---
name: warn-curl-pipe-shell
enabled: true
event: shell
pattern: (curl|wget)[^\n|]*\|\s*(ba)?sh
action: warn
---

Piping a download straight into a shell.

Fetching a script and executing it in one step means you never saw what
ran. Prefer: download first, inspect, then run - or fetch from a pinned,
trusted source.
