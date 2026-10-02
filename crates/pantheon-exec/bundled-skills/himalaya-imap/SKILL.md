---
name: himalaya-imap
description: "Read, search, and send email over IMAP/SMTP using the Himalaya CLI. Trigger when the user needs mail access, mail search, or wants to send an email from their account."
origin: bundled
prerequisites: ["himalaya installed by the setup wizard's Skill dependencies step (`pantheon setup`)", "IMAP/SMTP credentials"]
---

# Himalaya IMAP

> Requires setup: the `himalaya` binary (installed by the setup wizard's Skill
> dependencies step) and an account configured with IMAP/SMTP credentials.
> This skill does nothing until `himalaya` is on PATH and an account
> authenticates. Never claim mail access exists before testing.

Himalaya (github.com/soywod/himalaya) is a CLI mail client. Pantheon drives
it through the shell: every mail operation here is one command, which keeps
mail work auditable and out of long-lived sessions.

## Tooling

Assume the account is configured as `himalaya account configure`. Common
operations (prefix every command with `himalaya`):

- `folders` - list folders.
- `list -f INBOX -p 10` - newest 10 messages in INBOX.
- `read <uid> -f INBOX` - read one message body.
- `search -f INBOX "from:example.com"` - server-side search.
- `write -s "subject" -t to@example.com -b "body"` - compose; use
  `--send` only when the user has explicitly approved the exact content.
- `flag add seen <uid> -f INBOX`, `move <uid> archive -f INBOX` - mutation;
  approval-gated, always.

If `himalaya` is missing or the account auth fails, stop. Say which one
failed and what to do about it ("install himalaya", "check the app
password"). Do not fall back to imagining mail.

## Auth

Credentials live in Himalaya's own config, not in Pantheon transcripts or
skill files. Prefer OAuth2 or app passwords per provider. If a provider
blocks password auth (Gmail does without an app password), say so and stop
rather than trying workarounds.

## Operating rules

- Read-only commands are free. Anything that changes mailbox state - send,
  flag, move, delete - needs the user's explicit approval first, naming the
  exact message and the exact action.
- Drafts are produced as text and shown to the user before any `--send`.
- Never paste full message bodies into other tools or logs. Quote only the
  minimum needed for a decision.
- Batch reads: prefer `list` + one `read` per interesting message over
  dumping whole folders.
