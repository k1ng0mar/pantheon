# cf-ops: Cloudflare operations through the cf CLI

A bundled tool-plugin. Speaks Pantheon's tool-plugin JSON protocol over
stdio and runs `cf` CLI operations. Three tools (`cf_read`, `cf_write`,
`cf_destroy`) differ in declared intent: the host's capability gate and
policy machinery use that to park mutating calls for operator approval.
The runner itself never decides what is safe.

## What it does

- `cf_read`: read-only operations (list, get, describe, search, whoami).
  Runs without approval under the default coder policy.
- `cf_write`: create/modify operations. Approval.
- `cf_destroy`: deletes and production-sensitive operations (DNS on
  proxied zones, nameservers, WAF/firewall, Access policy, tokens,
  certificates). Approval.

The agent builds argv through `cf cli search` (Cloudflare's own command
discovery); the runner hands the array to cf as exec args, never a shell
string. cf prints JSON by default and the runner parses it: malformed
output surfaces as an error instead of a mystery downstream.

## Authentication

No secrets live in the plugin. cf resolves `CLOUDFLARE_API_TOKEN` from
its own environment, which the host injects at spawn time when the
`[cloudflare]` section is enabled. The runner reads no variables and has
no credential path; `[cloudflare].enabled = false` means every
authenticated call fails with cf's own auth error.

## Setup

The cf CLI and the token are prerequisites: `pantheon cloudflare setup`
checks them, offers the install, imports Cloudflare's official skills,
and writes the config section. This plugin's `enabled` flag in
`[plugins]` is the usual bundled gate; shipping in the catalog does not
enable it (bundled plugins ship off).

## Behavior details

- argv tokens must be plain strings; shell metacharacters are irrelevant
  because there is no shell, but null bytes and newlines in a token are
  rejected (CF_OPS_BAD_ARGS) since they corrupt the stdio protocol.
- Output caps at 512 KiB with a marker, matching the builtin shell tool.
- Timeout is 120 s, then SIGTERM. A spawn failure is CF_OPS_SPAWN.
- Non-zero exit passes cf's own stderr through as the error cause, so
  the agent sees Cloudflare's real error text (rate limits, auth
  problems, bad ids) instead of a wrapper's guess.
