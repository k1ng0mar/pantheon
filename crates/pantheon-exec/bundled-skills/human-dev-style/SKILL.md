---
name: human-dev-style
description: Use when commits should not look AI generated.
origin: bundled
---

# Human Dev Style

Write like a real everyday developer, not an AI. Humans write for themselves under constraints, AI writes for an imagined reviewer.

Core rules:
- Short commit messages only, no body and no description. Subject line is the whole message, plain wording, concrete. Do not assume the shape: run `git log --format=%s -40` and count `type(scope):` prefixes before choosing one. Pantheon subjects are bare lowercase one liners with no prefix, so a `feat(scope): ...` subject there is itself the give-away tell; a repo whose log does carry prefixes gets the prefix and still no body. Do not add billing, promise, or deadline language the user did not give. The user rejects AI-sounding conventional plus essay bodies (feat(scope): plus a paragraph), so rewrite to the shortest honest one-liner when humanizing.
- Split a multi-part change into logical incremental commits (transport, mapping, adapter, wiring, UI, docs) rather than one bulk squash, each with its own short subject. A sequence of scoped one-liners reads like real development; one mega-commit reads as generated. Confirm the log before push with `git log --format=%s -40` and `git show -s --format=%B HEAD`.
- Check every commit before push: if a commit has a body, rewrite it to a one liner. This rule has failed before when new commits were pushed with essay bodies, so verify with git show -s --format=%B HEAD and git log --format=%B and never assume the prior push followed the policy.
- When rewriting history to humanize messages, use git filter-repo message callback to strip bodies and shorten subjects, backup with git bundle first, restore origin after, force push with lease, and verify sole author remains. Code style should stay untouched in a message rewrite, because reformatting history breaks fmt, bisect, and hashes.
- Comments are marginalia. Sparse, uneven density. Apology, plea, fossil confusion, occasional lowercase no period. No banner comments on every function. Refactor fossils ("Extracted from X verbatim, behaviour-preserving move only") get deleted, not reworded: a file header explaining a move nobody remembers is noise.
- Names from the writer head, not systematic clarity renames.
- Leave small honest mess where it does not break correctness. Rust fmt stays enforced.
- No architectural ceremony the task does not need.
- Avoid uniform polish, perfect punctuation, explanatory banners, and summarizing obvious code.

- Dial light by default. Correctness, security, fail closed behavior, fmt, clippy, and tests always win over style.

## File merges and folder consolidation

Use when collapsing many files in one crate into umbrella files, as with provider adapters or gateway groups.

Procedure:
- Inventory the crate first. Read lib.rs mod and pub use lines, grep for `use crate::` and `crate::` refs across the crate to map shared state before writing any replacement file.
- Keep public re exports in lib.rs pointing at the new groups so external callers that used `pantheon_providers::embeddings` or `pantheon_gateway::http` still compile.
- Merge with submods (`pub mod anthropic { ... }` inside adapters.rs), not flat concatenation. Flat concat creates duplicate imports, duplicate test modules, and duplicate constants, and rustfmt plus cargo check both fail.
- Always re-read the crate after a merge attempt before retrying. Revert with `git checkout -- crates/<name>/src/lib.rs crates/<name>/src/*.rs` and remove the generated group file, because a half applied script corrupts further retries.
- Fix in file references as part of the same commit: `crate::gemini::` to `crate::adapters::gemini::`, `crate::answer_line::` to `crate::auxiliary::answer_line::`, and similar for each group. Grep the crate for remaining `crate::` paths after the first pass.
- Leave modules with `#[path]` attributes alone only if you are doing a script merge. For a surgical merge, inline the pointed-at test file as a `#[cfg(test)] mod tests { ... }` block inside the wrapped module instead, because adjusting the path with `../` fails when the intermediate directory does not exist on disk. See safe-refactor for the full surgical procedure.
- Crates with shared crate helpers in lib.rs (dashboard, runtime, api, tui, secrets, storage, nightly) need surgical merges, one leaf file at a time with the build kept green after each step, not script merges. They are attainable, just slower and higher risk than leaf crates. See safe-refactor.

Pitfalls:
- Do not merge `plugins.rs` with its build support path by script. The `#[path]` attr resolves relative to the new group folder and misses the file.
- Do not merge a file that declares `mod tests;` with a path attribute by script without updating the path in the same edit. The test module will be missing and cargo check fails.
- Do not patch `PluginManifest` initializers by regex across eval tests. Use precise per file patches and re run `cargo check -p pantheon-eval --all-targets` before commit. Over broad patches broke eval tests twice.
- Ignore `cargo check` exit code when the command is piped through `tail` or `head`. Rerun without the pipe to get the real exit code, the output is auto truncated anyway.
