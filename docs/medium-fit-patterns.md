# Medium-fit repo review: pattern-borrowing plan

Reviewed 2026-10-08. Four repos assessed for what Pantheon can borrow without wholesale integration. Each entry lists the transferable pattern, how it maps to Pantheon's existing architecture, and the concrete file paths where it would land.

---

## 1. ego-lite (citrolabs/ego-lite)

**What it is:** CDP browser automation harness for AI agents. Drives the ego lite browser through `globalThis.ego` bindings, exposes a snapshot/ref workflow, and layers reusable per-site knowledge packs ("learnings") on top.

### Borrowable: Task Space ownership model

ego-lite isolates browsing contexts into "Task Spaces" with an explicit ownership model (`agent` / `user`). Control handoff is a first-class API: `task.handOff()` yields to the user, `takeOverTaskSpace(spaceId)` claims it back.

This maps directly onto Pantheon's three-state permission model. Pantheon already has `PermissionMode` and approval parking; the ego-lite pattern adds the inverse: explicit user-to-agent reclaim, not just agent-to-user requests.

Integration path:
- `crates/pantheon-tools/src/browser_tools.rs` — add `handOff()` and `takeOver()` tool operations
- `crates/pantheon-api/src/capability.rs` — add `browser:handoff` capability token
- The permission card in `crates/pantheon-tui/src/session.rs` already handles approval; add a handoff-pending state

### Borrowable: Transient vs permanent element resolution errors

ego-lite classifies every element-resolution failure as `transient` (retryable, page still loading) or `permanent` (element gone, re-snapshot needed). Wait loops rely on this distinction.

Pantheon's browser tool has no equivalent. When a click fails, the agent doesn't know whether to retry or re-snapshot.

Integration path:
- `crates/pantheon-tools/src/browser_tools.rs` — wrap element resolution in a result type that carries `ResolutionError { kind: Transient | Permanent }`
- The agent prompt in `crates/pantheon-tui/src/commands.rs` should surface this so the model knows when to retry vs re-snapshot

### Borrowable: Site-specific learnings

ego-lite ships per-site knowledge packs: `learnings/<site>/manifest.json` + `notes/` + `tools/`. Each pack documents stable URLs, durable selectors, and reusable browser-tool scripts for a specific site.

This is a lightweight version of what Pantheon's bundled skills already do. The difference: ego-lite's learnings are scoped to a site and loaded conditionally, while Pantheon skills are always-on.

Integration path:
- New bundled skill pattern: `bundled-skills/browser-<site>/SKILL.md` with a `site:` frontmatter field
- `crates/pantheon-exec/src/skills.rs` — filter skills by `site:` metadata when the browser tool navigates to a matching domain

### Not borrowable

The VM-per-agent architecture, the C++ Firefox patching for anti-bot evasion, and the TaskSpace runtime itself. Pantheon already has its own sandbox model and browser tool.

---

## 2. gws / googleworkspace/cli

**What it is:** Rust CLI that dynamically generates its command surface from Google's Discovery Service at runtime. 40+ agent skills auto-generated from the API surface.

### Borrowable: Two-phase dynamic CLI parsing

gws parses argv to extract the service name, fetches the service's Discovery Document, builds a dynamic `clap::Command` tree, then re-parses.

Pantheon's MCP tool surface is currently static (registered at startup). If Pantheon ever needs to expose a large dynamic API surface (e.g., a bundled MCP server with hundreds of tools), this pattern avoids registering everything upfront.

Integration path (low priority):
- `crates/pantheon-mcp/src/client.rs` — lazy tool registration on first `tools/list` from an MCP server, filtered by capability
- Only worth doing if MCP server tool counts become a performance problem

### Borrowable: Input validation patterns

gws's AGENTS.md is explicit about adversarial inputs: path traversal (`../../.ssh`), control character rejection, URL path segment encoding, resource name validation before URL construction. Every helper has a paired rejection test.

Pantheon already has `danger.rs` (dangerous command pre-gate) and path validation in `skills.rs` (`contained_skill_dir`). The gws pattern adds: URL-encoding discipline for any value embedded in a URL path, and resource-name validation (rejecting `?` and `#` in identifiers).

Integration path:
- `crates/pantheon-exec/src/danger.rs` — add `encode_url_path_segment()` for tool arguments that construct URLs
- `crates/pantheon-exec/src/cloudflare.rs` — already has command classification; add resource-name validation for Cloudflare zone/account IDs passed as arguments
- `crates/pantheon-api/src/config.rs` — the profile-override tests already cover adversarial config; extend to URL-embedding values

### Borrowable: Helper command discipline

gws's rule: never write a helper that wraps a single API call, exposes data already in the response, or re-implements schema parameters as custom flags. Helpers control orchestration only.

Pantheon has the same risk with tool wrappers. A tool that just re-exposes one MCP method with renamed args is overhead, not value.

Integration path:
- Not a code change. A review rule: when adding a tool that wraps a single MCP method, justify why the wrapper adds orchestration value.

### Borrowable: AGENTS.md convention

Both gws and ego-lite use `AGENTS.md` as the agent-facing project guide (architecture, conventions, testing protocol, what not to do). Pantheon could adopt this for its own repo.

Integration path:
- `AGENTS.md` at the Pantheon repo root — architecture overview, build commands, test protocol, code conventions, what NOT to do (e.g., "don't add generated Rust crates for external APIs")

---

## 3. claude-obsidian (AgriciDaniel/claude-obsidian)

**What it is:** Local-first knowledge base with 15 Agent Skills. Turns source material into source-cited Obsidian pages. Has a strict mutation protocol and vault boundary enforcement.

### Borrowable: Mutation protocol (read-check-apply-transaction)

claude-obsidian's protocol for any knowledge mutation:

1. Read targets, record expected SHA-256 values
2. Parallel workers return drafts and evidence only (no direct writes)
3. Merge drafts into one transaction bundle
4. Inspect the bundle, apply once
5. Report operation ID and exact changed paths

This maps onto Pantheon's approval flow. Currently Pantheon applies tool outputs directly. The transaction pattern would: snapshot the target state, let the agent produce a plan, park for approval with the exact diff, apply atomically on approval.

Integration path:
- `crates/pantheon-api/src/approval_store.rs` — add a `TransactionProposal` variant that carries pre-state hashes + the proposed diff
- The approval card in `crates/pantheon-tui/src/session.rs` shows the diff before applying
- On approval, the apply step re-checks the hashes and rejects if the target changed (optimistic concurrency)

This is the highest-value borrow from all four repos. It closes a real gap: right now Pantheon has no way to detect that a file changed between plan approval and apply.

### Borrowable: Vault boundary enforcement

claude-obsidian enforces a hard boundary between product source and user vault. The product repo is never the user's vault. Vault resolution has a strict precedence chain (explicit flag > env var > nearest marker file > fail closed).

Pantheon's data dir resolution is similar but less strict. The `PANTHEON_DATA_DIR` override I just added in `skills_scan.rs` follows the same pattern (env var with default), but Pantheon doesn't fail closed when the dir is ambiguous.

Integration path:
- `crates/pantheon-exec/src/skills_scan.rs` — the `data_dir()` function should verify the dir exists and is writable before scanning, and return an error if `PANTHEON_DATA_DIR` is set to a path that doesn't exist
- Low priority; the current default (`~/.pantheon`) is unambiguous in practice

### Borrowable: "Never fabricate evidence" principle

claude-obsidian's AGENTS.md: "Never fabricate evidence locators, quotations, page numbers, or confidence."

Pantheon doesn't have an explicit rule against this in its agent prompt. The model could cite a line number that doesn't exist in a file.

Integration path:
- `crates/pantheon-tui/src/commands.rs` — add to the system prompt: "Cite exact line numbers only from tool output you have seen. Never estimate or fabricate locators."
- Or better: add a lint rule in `crates/pantheon-exec/src/bundled_skills.rs` that checks skill instructions for this principle

### Not borrowable

The Obsidian-specific vault structure, the 15-skill knowledge system, the Python scripts. Pantheon's skill system is already more general-purpose.

---

## 4. invisible_dots (feder-cr/invisible_dots)

**What it is:** Self-hosted VM-per-agent architecture. Each "Dot" gets a QEMU VM with a Linux desktop, persistent disk, and a fingerprint-hardened Firefox. Sleeps when idle, wakes for tasks.

### Borrowable: Skill-learning loop

invisible_dots writes its own notes and how-tos during task execution, then reads them on the next task. This is a lightweight version of what Pantheon's nightly propose-only memory lessons do.

The difference: invisible_dots writes at task time (inline learning), Pantheon proposes at nightly time (batch learning). Inline is more responsive; batch is more controlled.

Integration path:
- Not recommended as-is. Pantheon's propose-only model was a deliberate choice to prevent unattended runs from polluting durable memory.
- If anything: add a "session notes" scratch file that persists across a session but is cleared on `/new`. This gives inline learning without the durability risk.

### Borrowable: Approval before automation

invisible_dots asks before converting a one-off task into a recurring automation. The agent proposes the automation; the user confirms before it becomes scheduled.

Pantheon's cronjob system already has this (approval before creating a cron). But the pattern of "do it once manually, then propose to schedule it" is worth documenting.

Integration path:
- Documentation only. Add to Pantheon's skill template: "After completing a task manually, propose scheduling it as a recurring automation if the user has done it 3+ times."

### Not borrowable

The VM architecture, the C++ Firefox patching, the QEMU setup, the hardware virtualization requirement. All out of scope for Pantheon.

---

## Summary: what to actually implement

| Repo | Pattern | Priority | Effort |
|---|---|---|---|
| claude-obsidian | Transaction proposal with pre-state hashes | High | Medium |
| ego-lite | Transient/permanent resolution errors | Medium | Low |
| ego-lite | Task Space handoff/takeover | Medium | Medium |
| gws | URL path encoding validation | Medium | Low |
| claude-obsidian | "Never fabricate evidence" in prompt | Low | Trivial |
| ego-lite | Site-scoped learnings | Low | Medium |
| gws | AGENTS.md convention | Low | Trivial |
| invisible_dots | Session-scratch notes | Low | Low |
| claude-obsidian | Vault boundary fail-closed | Low | Low |

**Recommendation:** implement the claude-obsidian transaction protocol first. It closes a real correctness gap (stale approval applies) and the ego-lite transient/permanent error classification second (improves agent retry behavior with minimal code). Everything else is nice-to-have.
