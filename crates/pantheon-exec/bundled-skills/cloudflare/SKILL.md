---
name: cloudflare
description: "Operate Cloudflare infrastructure through the cf CLI: inspect zones, DNS, Workers, KV, R2, WAF, and access policy; plan changes and apply them through the approval flow. Trigger when the user mentions Cloudflare, a zone, DNS records, Workers, wrangler, or asks what Cloudflare resources they have."
origin: bundled
prerequisites: ["the cf CLI (installed by `pantheon cloudflare setup`)", "CLOUDFLARE_API_TOKEN resolved through the secrets broker"]
---

# Cloudflare

Pantheon drives Cloudflare through Cloudflare's own `cf` CLI. It covers the
whole Cloudflare API (over 3,000 operations), outputs JSON by default, and
ships a command-discovery search so exact syntax is never guessed.

## What each interface is for

| Interface | Use it for | Do not use it for |
|---|---|---|
| `cf` | Everything: zones, DNS, Workers, KV, R2, WAF, access, certs, billing | Nothing; if a command is missing use `cf cli search` |
| `cf cli search "<words>"` | Finding the right operation when unsure of the command | Running anything; it only returns matches |
| `wrangler` | Only inside a Worker project whose build still needs esbuild or Rust/Python Workers; `cf` delegates to it internally | General Cloudflare operations; `cf` supersedes it |
| remote MCP servers | `cloudflare-docs` (public, no auth) for live documentation lookups | Account operations; the OAuth servers need a browser flow Pantheon does not perform |

## Command discovery, never guessing

Before running a `cf` command you have not seen verified, search for it:

```sh
cf cli search "point a subdomain at a server"
cf cli search "list dns records"
```

The result names exact operations (`cf dns records create`,
`cf dns records list`). Read one command's flags with `cf <command> --help`
before running it. If search returns nothing, say so; do not invent a
command shape.

## The workflow: observe, plan, approve, apply, verify

1. **Observe.** Read the current state before changing anything.
   `cf dns records list` for the zone, `cf workers list`, `cf zones list`.
   Never write a record you have not seen the current value of.
2. **Plan.** State the exact change: which record, which type, which
   content, which zone. The user reads this in the approval prompt.
3. **Approval.** Pantheon's policy classifies every `cf` call:
   - reads (list, get, describe, search, whoami) run without approval
   - create/update operations park the run for a human (`cloudflare.write`)
   - deletes and anything touching DNS on a proxied zone, nameservers,
     WAF/firewall, Access policy, tokens or certificates parks for a human
     (`cloudflare.destroy`)
   The approval prompt shows the exact command. Grant or deny it there.
4. **Apply.** Run the command. It goes through the same shell sandbox as
   every other tool call.
5. **Verify.** Re-read the state you changed
   (`cf dns records list` again) and compare against what you planned.
   Report the observed result, not the expected one.

## Worked examples

"Point api.example.com at my new server 1.2.3.4":

```sh
cf dns records list --zone example.com        # observe: does api exist?
cf dns records create --zone example.com ...  # approval parks here
cf dns records list --zone example.com        # verify the record content
```

Use `cf cli search "create dns record"` first if the flags are unknown.
The record type for a bare IPv4 target is A; for a CNAME target check the
existing records first.

"Deploy this Worker":

```sh
cf deploy          # in the Worker's project directory
```

"What Cloudflare resources do I have?":

```sh
cf auth whoami     # account, token validity
cf zones list      # zones
cf workers list
cf r2 buckets list
cf kv namespaces list
```

## Safety notes

- Zone-level DNS, WAF, Access, and token operations always need explicit
  approval regardless of the verb. A wrong record on a proxied zone takes
  the domain's traffic down.
- Never set `CLOUDFLARE_API_TOKEN` in a command line (`env VAR=x cf ...`
  or `CLOUDFLARE_API_TOKEN=x cf ...`): the token is injected by the
  runtime from the secrets broker, and putting it in the command text
  would copy it into the ledger transcript. If an authenticated call
  fails with an auth error, say the token needs attention; do not try to
  read or print it.
- Global API keys are legacy. If `cf auth whoami` reports the token is
  invalid, the fix is a new API token in the Cloudflare dashboard, not a
  different credential type.

## Setup and status

`pantheon cloudflare setup` checks node/npm, offers to install `cf`,
imports Cloudflare's official skills, and writes the `[cloudflare]`
config section. `pantheon cloudflare status` reports what is present,
authenticated, and enabled.
