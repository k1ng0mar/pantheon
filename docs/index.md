# Pantheon docs

> The model is replaceable. Your agent is not.

Pantheon is an assistant that runs on your own computer. It talks to AI models for you, remembers what happened, asks before doing anything important, and picks up where it left off, even if you close the laptop or switch models.

## Start here

- [Getting started](getting-started.md): install, set up, have your first conversation.

## User guide

| Page | Contents |
|---|---|
| [Sessions](user-guide/sessions.md) | The terminal app, one-off tasks, looking back at old conversations |
| [Agents](user-guide/agents.md) | Named assistants with their own personality and memory |
| [Memory](user-guide/memory.md) | What it remembers, and how it earns your trust |
| [Runs](user-guide/runs.md) | How work happens, permissions, recovering from crashes |
| [Channels](user-guide/channels.md) | Terminal, web, Discord, Telegram |
| [Providers](user-guide/providers.md) | Which AI models it talks to, and how to switch |
| [Scheduling](user-guide/scheduling.md) | Recurring tasks that run on their own |
| [Extensions](user-guide/extensions.md) | Plugins and extra capabilities |

## Reference

| Page | Contents |
|---|---|
| [Terminal](reference/terminal.md) | Every command, flag, and exit code |
| [Configuration](reference/configuration.md) | The `config.toml` file, settings, secrets |
| [Troubleshooting](reference/troubleshooting.md) | Error codes, common problems, how to fix them |

## Developer

| Page | Contents |
|---|---|
| [Architecture](developer/architecture.md) | How the system is put together |
| [Contributing](developer/contributing.md) | How to add code or docs |
| [TUI states](developer/tui.md) | Terminal interface spec (internal) |
| [Decisions](developer/decisions/) | Past design decisions (internal) |

## The idea behind it

Your assistant should outlive the model it talks to. Change models, restart the machine, close the app: the assistant stays the same, and everything it learned stays with it.
