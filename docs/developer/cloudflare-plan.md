# Cloudflare integration: architecture assessment and plan

## What Cloudflare's official agent setup says

Source: developers.cloudflare.com/agent-setup/prompt.md (fetched 2026-10-02), the
cloudflare/skills repo README, and the cf launch blog.

1. **Official skills** live at github.com/cloudflare/skills (Apache-2.0, ~10
   skills: cloudflare, wrangler, agents-sdk, durable-objects, sandbox-*, web-perf,
   cloudflare-one, ...). Install mechanisms: Claude plugin marketplace,
   `npx skills add cloudflare/skills`, or plain clone-and-copy of skill folders.
2. **Remote MCP servers** (five in the default set): `mcp.cloudflare.com/mcp`
   (Code Mode, whole API), plus docs / bindings / builds / observability. Docs
   server is public with no auth; the rest use per-user browser OAuth that
   triggers on first tool use.
3. **The cf CLI** (open beta, `npm i -g cf`, needs Node 22+): 3,000+ operations
   generated from Cloudflare's OpenAPI surface, JSON output by default, and
   `cf cli search "<natural language>"` for command discovery. Credentials:
   `CLOUDFLARE_API_TOKEN` env var first, then named OAuth profiles
   (`cf auth login`). Wrangler continues for esbuild/Rust/Python Workers builds;
   `cf migrate` moves projects to `cloudflare.config.ts`.
4. Cloudflare explicitly tells agents to install the skills plus the five MCP
   servers, and to let OAuth trigger on first use. They do not say "vendor our
   docs into your repo".

## What Pantheon already has (the mapping)

| Cloudflare piece | Pantheon mechanism | Status |
|---|---|---|
| Skills | `SKILL.md` discovery + `skill_read`/`skill_exec`, bundled seeding, `pantheon skills import --repo <URL>` | exists |
| cf CLI | `shell` tool + `capability_of` + `extra_capabilities` hook (the `git push` pattern) | exists, needs a Cloudflare-aware capability hook |
| Remote MCP | `pantheon-mcp` http transport + `[mcp.servers.<name>]` + bundled catalog + `McpEnable` approval | exists, **but the http transport sends no Authorization header and has no OAuth flow** |
| Secrets | `SecretsBroker` (env + keyring + encrypted file), `[secrets].plugin_env_allowlist` second-gate pattern | exists |
| Sandbox env scrub | `scrub_child_env` allowlists PATH/HOME/... and nothing else | exists |
| Setup | `pantheon setup` wizard, `ProviderKind` (Keyless/Cloud/SelfHosted/Local), `detect_binary`, `skill_deps` screen | exists |
| Doctor | `pantheon doctor` checks with section/status/detail/fix | exists |
| Approval | `Capability` enum + `Policy` + `register_with(extra_capabilities)` + one-shot call-scoped grants | exists |

Two hard gaps found:

- **MCP http transport cannot authenticate.** `post()` sets only Content-Type,
  Accept, and Mcp-Session-Id. `mcp.cloudflare.com/mcp` requires OAuth. The
  bundled `cloudflare` catalog row exists but would 401 on every call today.
- **Sandboxed shell children cannot receive `CLOUDFLARE_API_TOKEN`.**
  `scrub_child_env` wipes everything and re-adds a fixed list. The secrets
  broker resolves secrets for in-process tools (websearch) and for plugins via
  the two-gated allowlist, but no path feeds a secret into a shell child.

## Design decisions

1. **Skills: referenced, not vendored.** Cloudflare ships and maintains them;
   vendoring forks their content into our repo where it goes stale and where an
   update means a diff review of thousands of lines. Instead:
   `pantheon cloudflare setup` runs `pantheon skills import --repo
   https://github.com/cloudflare/skills` (existing code path) into the data dir
   with origin recorded. Refresh = rerun. This keeps provenance, licensing
   (Apache-2.0, license ships in the repo), and updates in one command. Supply
   chain: the import path already shallow-clones from github.com over https;
   skills are data gated on FilesystemRead and gain no capabilities by
   declaration, so a malicious skill still cannot act without the capability
   gate.

2. **cf CLI is the primary execution interface.** Detect `cf` on PATH (Local
   provider row in the wizard, detect/install-or-skip like cua-driver). The
   Cloudflare tool classifies `cf ...` invocations by subcommand through the
   shell tool's `extra_capabilities` hook (the `is_git_push` pattern):
   - read/list/get/search/describe/whoami/zones list etc -> `ShellExecute` (already allowed)
   - create/update/put/patch on non-production resources -> adds
     `Capability::Other("cloudflare.write")` (Approval in default policies)
   - delete / DNS record create+update+delete on proxied zones / nameserver
     changes / WAF/firewall / Access policy / zone settings / token & secret
     operations -> adds `Capability::Other("cloudflare.destroy")` (Approval,
     and Deny in the reader preset by omission)
   Command discovery uses `cf cli search` (real, verified on this box), so the
   agent never hallucinates syntax; the skill teaches this workflow.

3. **MCP: add header auth to the http transport, enable the docs server, leave
   OAuth servers to a documented manual step.** Minimal transport change:
   `McpServerEntry` grows `headers: HashMap<String,String>` with `env:` value
   resolution (same convention as `env`), and `post()` sets them. That makes
   any token-authenticated remote MCP usable. The `cloudflare-docs` server
   (public, no auth) can be enabled with zero new auth work and gives the
   agent live docs. The OAuth servers (cloudflare-api, bindings, builds,
   observability) need a browser OAuth dance the stdio-free transport does not
   have; implementing a full OAuth authorization-code flow in pantheon-mcp is
   its own project and is NOT in this slice. cf with `CLOUDFLARE_API_TOKEN`
   covers the same API surface.

4. **Secrets: one env name, broker-resolved, sandbox-injected.**
   `CLOUDFLARE_API_TOKEN` resolves through the `SecretsBroker` (env, then
   keyring, then encrypted file) and is injected into the shell child's env
   only when (a) the session config opts in via a new
   `[cloudflare]` section, and (b) the call carries the Cloudflare capability.
   Implementation: `run_sandboxed_with_env(profile, ..., extra_env)` passing
   resolved values; the existing two-gate shape (declare + allowlist) is
   preserved because the config section IS the operator's declaration.
   The token never appears in logs, ledger text, or tool output; the existing
   redaction (`logging::redact`) covers the failure paths.

5. **No second approval system.** Cloudflare operations are shell commands;
   they ride the existing capability gate, approval park, and one-shot grant
   machinery. The only new code is classification (which `cf` verbs need which
   capability).

6. **No generic cloud-provider framework.** A `[cloudflare]` config section, a
   classification module, a bundled skill, and setup/doctor wiring. When a
   second provider shows up, the reusable pattern is the one this documents:
   skill import + Local provider row + capability hook + env injection + doctor
   checks. That is the pattern, not a framework.

## Implementation slices

1. `pantheon-exec/src/cloudflare.rs`: command classification
   (`classify_cf(args) -> CfOp {Read, Write, Destroy}`), pure functions,
   unit-tested.
2. `pantheon-tools`: register a `cloudflare` capability hook on the shell tool
   mapping CfOp to `Capability::Other("cloudflare.write" / "cloudflare.destroy")`;
   policies add Approval for those tokens.
3. `pantheon-exec/src/sandbox/runner.rs`: `run_sandboxed_with_env` +
   `build_sandboxed_with_env` threading a small extra-env map past the scrub.
4. `[cloudflare]` config section: `enabled`, `api_token_secret` (SecretRef-style
   env name), `account_id`; config schema + doc.
5. `pantheon-mcp`: `headers` on `McpServerEntry` + transport support; update
   the bundled cloudflare row's setup notes (docs server usable now; OAuth
   servers need cf or manual OAuth).
6. Bundled skill `crates/pantheon-exec/bundled-skills/cloudflare/SKILL.md`:
   teaches observe-plan-approve-apply-verify with `cf cli search`, the
   destructive-class list, and when Wrangler beats cf.
7. Bundled tool plugin `cf-ops`: `cf_read` / `cf_write` / `cf_destroy`
   through the tool-plugin stdio protocol, exec-argv only (never a shell
   string), same env injection as the shell tool, off by default like
   every bundled plugin. Umar's direction: plugin + skills + MCP, no
   Pantheon CLI verb for this integration.
8. Eval tests: classification table-driven, config round-trip, dependency
   detection with fake PATH, policy gating, env-injection unit test with a
   temp token; live tests behind `PANTHEON_CF_LIVE=1` (skipped by default,
   like the gsd-browser live tests).
9. Docs: `docs/user-guide/cloudflare.md` written to current truth.

## What is deliberately NOT in this slice

- A `pantheon cloudflare` CLI verb (Umar's direction: plugin + skills + MCP,
  no verb; setup/status live in the doctor's cloudflare checks and the
  bundled skill's prose).
- OAuth authorization-code flow inside pantheon-mcp (the remote cloudflare-api
  server stays behind a documented manual step; cf + API token covers it).
- Wrangler-specific tooling: cf delegates to wrangler internally for the
  Workers builds that still need it; no separate Pantheon surface.
- A generic multi-provider abstraction layer.
- Any non-shell native Cloudflare REST client: cf already speaks the API and
  Cloudflare keeps it current; a second client in Rust would rot.

## Environment facts this plan relies on (verified on this host)

- `cf` v1.0.0-beta.12 already installed and authenticated via
  `CLOUDFLARE_API_TOKEN` in `~/.hermes/.env`; `cf auth whoami` returns valid
  accounts; `cf cli search` works.
- Node v26.9.0, npm 11.19.1 present.
- The token lives in Hermes's env, which the Pantheon runtime does NOT inherit
  through the sandbox scrub. The integration must resolve it through the
  broker explicitly.
