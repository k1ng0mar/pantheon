---
name: warn-sensitive-files
enabled: true
event: write_file
action: warn
conditions:
- field: path
    operator: regex_match
    pattern: \.env(\.|$)|credentials|secrets|\.pem$|\.ssh/
---

Sensitive file detected.

You're writing to a file that may hold secrets:
- keep credentials out of the content; use the environment or the secrets store
- verify the file is covered by .gitignore
- consider whether the write is really needed
