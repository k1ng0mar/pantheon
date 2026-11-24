---
name: repo-authentic-policy
description: "Use when writing or reviewing code. Repo fit over AI habits."
origin: bundled
license: MIT
---

# Repository authenticity policy

## When to Use

Any time code gets written, edited, or reviewed in an existing repository. The trigger is not "is this a Rust repo" or "is this Pantheon": it is that the code lands next to code somebody else already wrote and a human maintainer will read it.

Core question before writing code: how would an existing contributor to this repository solve this? The repo is the primary source of truth, not generic best practices and not model memory.

## Before writing

- Inspect the repo first: neighboring files, module, types, error handling, tests, dependency versions. Never start from the task description alone.
- Find where similar behavior already exists and which abstraction owns it. Extend existing mechanisms instead of creating parallel ones.
- Priority order for implementation style: same subsystem code, project conventions, dependency usage, project docs, language idioms, official docs, general knowledge, model memory.
- Verify APIs against the installed dependency source, lockfile, or compiler. Never assume an API exists from memory. Respect the versions in the manifest, not current docs. For a client talking to a server you can read, see "Client adapters against a server you do not own" before writing any call site.

## Client adapters against a server you do not own

When the change is a client (phone app, script, SDK) talking to a server whose source you can read, enumerate the server's real surface before implementing anything. A guessed route or verb is a guaranteed runtime failure.

- List the server's actual endpoints from its router source (grep the route decorators, for example `@router.get("/api/...")` and `@router.post(...)`) and its actual RPC verbs from the method registry (`@method("name")`, handler dicts, dispatch tables). Do not trust the client being replaced, a README, or memory.
- Cross-check every call site in the new client against that inventory before shipping, and report only calls you confirmed.
- If an assumed route or verb does not exist, reroute to the real equivalent (often a differently named POST, or a REST route where an RPC verb was guessed), or drop the method. Guard optional provider methods behind optional calls so the UI disables the feature instead of surfacing a 404.
- Confirm the request contract too, not just the path: query parameter names, request body model fields, response field names, and which fields are optional. A flat row versus a nested object is a common mismatch.
- Treat the server's installed version as authoritative over the client. Version drift silently breaks adapters; the route list is a snapshot, so re-run it when the server moves.

## Scope and change shape

- Make the smallest change that is actually correct. Every modified file needs a "this changed because..." reason. "It was nearby" and "I cleaned it up" are not reasons.
- No unrelated refactors, renames, reformatting, dependency bumps, or cleanup passes bundled into a feature or fix. Unrelated problems go to a separate change unless they block the task.
- Do not rewrite existing code just because another approach looks cleaner. If the repo uses `foo().await?`, do not add error wrapping everywhere.
- Do not assume the repo is wrong: strange code may exist for compatibility, performance, or historical reasons. Use git log/blame before removing a workaround you do not understand.
- Minimal means minimum for correctness, not minimum lines. Do not under-implement known requirements. Watch for negative requirements: "except", "unless", "only", "never" often carry the real constraint.

## Avoid generic AI patterns

- No Manager/Service/Handler/Factory/Builder/Helper/Wrapper/Context types without a concrete architectural reason.
- Abstraction needs a reason: repeated behavior, stable domain concept, real interface boundary, multiple implementations, or an existing convention. "Useful later" is not a reason.
- Prefer small local duplication over a premature abstraction when the relationship between pieces is uncertain.
- Use the repo's domain vocabulary. If the project calls it a Run or a Receipt, do not rename it Task or Operation.
- Reuse before creating: search for the function, type, or helper before writing a new one.

## Reviewing AI-generated code

Before reading a diff as "the implementer's code", check it as a stranger's code. Re-deriving the author's intent from the commit message is not verification — a commit message describes intent, and intent is exactly what a bug hides behind.

On any change that moves money, authenticates, or writes state, check these before anything stylistic:

- **Non-transactional paired writes.** A balance change and its ledger row in separate operations can diverge on crash. This repo's convention is a MongoDB session; see `wallet.service.ts`.
- **Missing idempotency key on a money path.** The backend middleware only enforces exactly-once when the `Idempotency-Key` header is present, so omitting it silently disables the guarantee. Existing callers that do it right are the reference.
- **Unbounded trust in an external response.** Any figure taken from a provider and used as money needs a sanity band and a refusal path, not just a null check.
- **Resource leak on the error path.** A session started in `try` must be aborted and ended in `finally`.
- **Regression check, not just a green check.** Run the failing suite on the untouched pre-change tree to establish which failures are pre-existing. "Tests pass" and "tests fail the same as before" are different claims and only one of them may be reported honestly.

Report pre-existing failures as pre-existing, with the evidence. Do not let them disappear into a summary that implies the change is clean.

## Comments and docs

- Comments explain what code cannot: why a decision exists, invariants, workarounds, constraints. No narration of syntax ("// loop through the array").
- Match the neighboring comment density. No documentation dumps for small internal changes.
- Docs must reflect actual behavior. Never describe aspirational functionality as implemented.

## Errors, tests, security

- Follow the project's error model. No catch-all-and-return-None, unwrap_or_default everywhere, or log-and-continue without a reason. If the caller needs to know it failed, propagate.
- Tests verify behavior and contracts, not implementation details. Never weaken, skip, or delete a failing test to make it pass; find out why it fails.
- No speculative features, config fields, or compatibility layers that were not requested. Every fallback needs a reason.
- Security controls correspond to real threats and trust boundaries, not arbitrary defensive checks.

## Never

- Never invent APIs or package names.
- Never manufacture human imperfections: fake typos, inconsistent style, fake comments. The goal is to write like the project, not to look less like AI.
- Never overstate completion. Distinguish implemented, compiled, tested, manually verified, and unverified.
- Never trust your own previous output as authoritative; evaluate generated code like any other code. Do not validate AI code with AI tests without independent evidence (compiler, existing tests, runtime).
- Never present placeholders, mocks, or stubs as production behavior.

## Final review

Read the diff hunk by hunk like a reviewer of someone else's PR: scope, architecture fit, style continuity, correctness, failure behavior, security, test quality. Then the maintainer test: would a new contributor understand why this exists, and will it make sense in six months.

Short version: read before writing, search before creating, reuse before abstracting, local conventions before best practices, smallest correct change, every change has a reason, verify behavior not compilation, review the diff like a maintainer.