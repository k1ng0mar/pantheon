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

There is no `pantheon cloudflare` command and no `[cloudflare]` config
section: Cloudflare is reached entirely through the three channels above,
so there is nothing Pantheon-specific to configure. In particular:

- **cf CLI** — install it yourself (`npm install -g cf`, Node 22+) and
export `CLOUDFLARE_API_TOKEN`. Pantheon classifies `cf` invocations by
capability (`cloudflare.write` / `cloudflare.destroy` park for approval)
and scrubs everything else from the child environment.
- **cf-ops plugin** — bundled but disabled by default; enable it with
`pantheon extensions approve cf-ops` (or the `/plugins` screen).
- **Cloudflare skills** — import them explicitly through the skills path
(`pantheon skills import <source>`); nothing is cloned automatically.

`pantheon doctor` does **not** carry a Cloudflare section. The closest
signal it reports is `security.exec_sandbox`, which tells you whether the
sandboxed shell the `cf` CLI runs under is available; a missing
`CLOUDFLARE_API_TOKEN` is reported by `cf` itself, not by the doctor.

## Authentication and the token path

cf resolves credentials in this order: the `CLOUDFLARE_API_TOKEN`
environment variable, then named OAuth profiles (`cf auth login`). The
API token is the automation path and the one Pantheon wires.

The token reaches a `cf` child process through one sanctioned path:

1. `CLOUDFLARE_API_TOKEN` is exported in the operator's environment (or
   provided as a vault secret under that name). There is no
   `[cloudflare].enabled` switch — the credential is present or it is not.
2. The shell tool's env hook injects it only for `cf` commands, past the
   sandbox's env scrub, into the child's environment.

The token never appears in the ledger, logs, or tool output. Passing it
inline as an `env` prefix on the command defeats the redaction layer and
lands it in the transcript; the plugin runner rejects that shape.

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
- Token injection appears dead: check `CLOUDFLARE_API_TOKEN` is exported
  in the environment the shell tool inherits, then run
  `cf auth whoami` to confirm the token works.
- A cf call parked for approval: the approval prompt shows the exact
  argv. Grant the specific call or deny it; a grant is one-shot.
