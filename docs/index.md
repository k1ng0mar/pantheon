# Pantheon docs

> Your AI. Your way of working.

Pantheon is an open, general-purpose AI agent harness. You bring the model and decide what the agent can reach. Pantheon provides the tools, permissions, memory and recovery that let it do useful work on your machine.

## Who it is for

Everyday users, professionals and small teams, developers, and researchers or builders. The common thread is having work you want done without building an agent framework first. Coding is one capability here, not the identity.

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
| [Web access](user-guide/web.md) | Web search and browser automation |
| [MCP servers](user-guide/mcp.md) | Third-party tools: declaring, approving, watching them run |
| [Cloudflare](user-guide/cloudflare.md) | Cloudflare through the cf CLI, cf-ops plugin and remote MCP |

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
