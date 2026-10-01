---
name: block-dangerous-rm
enabled: true
event: shell
pattern: rm\s+-[a-z]*r[a-z]*f?\s+/(?:\s|$)
action: block
---

Dangerous rm command detected.

This command deletes from the filesystem root. Before proceeding:
- verify the path is exactly right
- consider a safer approach (trash/archive first)
- make sure you have backups
