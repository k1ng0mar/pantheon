# MCP servers

MCP (Model Context Protocol) servers are third-party programs that give the agent extra tools: a GitHub server, a database server, a codebase index, anything that speaks the protocol. Pantheon launches them (or connects to them over the network) and hands each of their tools to the model as `mcp_<server>_<tool>` - so the `echo` tool on a server you named `github` becomes `mcp_github_echo`.

## Declaring servers

Add them to `config.toml` (see [Configuration](../reference/configuration.md#mcp-servers)):

```toml
[mcp.servers.github]
transport = "http"
url = "http://localhost:8080/mcp"

[mcp.servers.github.env]
GITHUB_TOKEN = "env:GITHUB_TOKEN"
```

Servers imported from another agent setup by `pantheon migrate apply` also work: their declarations live under `<data_dir>/mcp/*.json` and fill in any names the config section doesn't define. The config section always wins on a name clash.

A codebase index is not special: it's just another `[mcp.servers.<name>]` entry pointing at whatever MCP server indexes your repo. There is no bespoke codebase-memory integration to configure.

## Approving servers

Nothing runs until you say so. `pantheon mcp approve <name>` shows you exactly what would launch (or where it would connect) and asks you to type the server name to confirm:

```sh
pantheon mcp approve github
```

The approval is bound to three things: the server's name, the version it reports, and a hash of what would run (the binary and its script arguments for `stdio`, the endpoint URL for remote servers). Upgrade the server or change its arguments and the approval stops matching - you're asked again. To see what's declared and whether it's approved: `pantheon mcp list`.

Secrets a server needs go in its `env` map as `"env:NAME"` references. They resolve from your vault (or environment) when the server launches and are never printed - `mcp list` and the dashboard only ever show variable *names*.

## Watching them run

- `pantheon mcp list` - declared servers, transports, approval state.
- `pantheon mcp status` - live snapshot: status, tool counts, connects, failures.
- `pantheon mcp health <name>` - detail for one server, including the last error.
- `pantheon mcp enable|disable <name>` - toggle a declared server (config-defined servers are toggled in `config.toml`).
- Inside the TUI: `/mcp reload` re-reads config + declarations and hands the new specs to the launcher without dropping your session.

The dashboard's MCP page shows the same merged view with live health. A server that keeps failing backs off (1s, doubling, capped at 5 minutes) instead of hammering retries; a server that dies mid-call gets one reconnect and one retry, then the error goes to the model like any other tool failure.
