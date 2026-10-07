# Cloudflare

Pantheon integrates Cloudflare through three official channels, no
Pantheon-specific CLI surface:

| Channel | What it provides | Where it lives |
|---|---|---|
| cf CLI | The whole Cloudflare API (3,000+ operations), JSON by default, `cf cli search` for command discovery | The `shell` tool, classified by Pantheon's capability hook |
| cf-ops plugin | A bundled tool plugin exposing `cf_read` / `cf_write` / `cf_destroy` through the plugin protocol | `[plugins]` section, bundled catalog |
| Remote MCP | `cloudflare-docs` (public, no auth) bundled; the account servers (OAuth) documented but not bundled | `[mcp.servers]`, the `enable_mcp` tool |
| Official skills | Cloudflare's own skill library (workers, wrangler, durable-objects, agents-sdk, ...) | Imported on request from github.com/cloudflare/skills |

## Setup

```
pantheon cloudflare setup
```

The setup walks the chain and does what it is allowed to:

1. Detects node, npm, and the cf CLI. Offers `npm install -g cf` after a
   confirm. Node 22+ required.
2. Checks `CLOUDFLARE_API_TOKEN` in the environment. Never reads or
   prints the value.
3. Offers to import Cloudflare's official skills from
   github.com/cloudflare/skills into the data dir (shallow clone through
   the existing skills-import path).
4. Writes the `[cloudflare]` config section.

`pantheon cloudflare status` reports what is present, authenticated, and
enabled without changing anything. `pantheon doctor` carries a
cloudflare section with a fix hint per gap.

## Authentication and the token path

cf resolves credentials in this order: the `CLOUDFLARE_API_TOKEN`
environment variable, then named OAuth profiles (`cf auth login`). The
API token is the automation path and the one Pantheon wires.

The token reaches a `cf` child process through one sanctioned path:

1. `[cloudflare].enabled = true` in config.toml (the operator's
   declaration; the default section is disabled and injects nothing).
2. The runtime resolves `api_token_secret` (default
   `CLOUDFLARE_API_TOKEN`) through the secrets broker at call time.
3. The shell tool's env hook injects it only for `cf` commands, past the
   sandbox's env scrub, into the child's environment.

The token never appears in the ledger, logs, or tool output. Putting it
in the command text (`env CLOUDFLARE_API_TOKEN=x cf ...`) defeats the
redaction layer and lands it in the transcript; the plugin runner
rejects that shape.

## Capability classification

Every `cf ...` invocation through the shell tool is classified by
`pantheon-exec::cloudflare`:

| Class | Verbs | Capability | Default policy |
|---|---|---|---|
| Read | list, get, describe, search, whoami, status | shell.execute (already granted) | Allow |
| Write | create, update, put, patch, import, deploy, upload, rotate, ... | `cloudflare.write` | Approval |
| Destroy | delete, purge-all, revoke, logout + anything under dns, zones, firewall, waf, access, zero-trust, tokens, ssl, registrar, billing | `cloudflare.destroy` | Approval |

Unknown operations classify as Destroy: fail closed. The classification
table is pinned in tests to commands verified against the real cf
surface, so a verb rename in cf fails the test instead of silently
changing what needs approval.

The workflow the bundled cloudflare skill teaches: observe the current
state, plan the exact change, apply after approval, verify by re-reading,
report the observed result.

## The cf-ops plugin

Ships in the bundled catalog, off by default like every bundled plugin.
Enable in `[plugins]` or through the agent's `enable_plugin` tool (which
needs approval). It runs `cf` as exec argv, never a shell string, caps
output at 512 KiB, times out at 120 s, and passes cf's own error text
through on failure. No secrets live in the plugin: it inherits the same
env injection the shell tool gets.

## MCP servers

`cloudflare-docs` is bundled and public: documentation lookups with no
authentication. Enable it like any bundled server.

The account servers (`cloudflare` at mcp.cloudflare.com, plus bindings,
builds, observability) use per-user browser OAuth. Pantheon's HTTP MCP
transport supports header authentication (`headers` in
`[mcp.servers.<name>]`, values `env:NAME` resolve at load) but does not
implement the OAuth authorization-code flow. Until it does, those
servers are reachable through the cf CLI with an API token, which covers
the same API surface.

## Official skills

Imported by reference, not vendored:

```
pantheon skills import --repo https://github.com/cloudflare/skills --sub skills
```

Rerunning updates them. Skills are data gated on FilesystemRead and
declare no capabilities, so an import adds knowledge, not permissions.

## Current status

Implemented and tested: classification (12 tests incl. a live-verified
surface table), capability gating, token injection through the sandbox,
the cf-ops plugin (live-smoked against a real account), the
cloudflare-docs bundled MCP row, header auth on remote MCP entries,
config section with round-trip test, doctor section, skills import.

Not implemented: OAuth inside the MCP transport, a native Rust REST
client, wrangler-specific tooling beyond what cf delegates internally,
and a generic multi-provider framework.

## Troubleshooting

- `cf auth whoami` says the token is invalid: create a new scoped API
  token in the dashboard. Avoid the Global API Key; it is legacy.
- Every cf call fails with "No zone specified": pass `--zone` or set
  `CLOUDFLARE_ZONE_ID`.
- Token injection appears dead: check `[cloudflare].enabled`, then
  `pantheon cloudflare status`.
- A cf call parked for approval: the approval prompt shows the exact
  argv. Grant the specific call or deny it; a grant is one-shot.
