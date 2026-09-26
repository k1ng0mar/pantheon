# Group B review (exec, storage, memory, extensions) — Nyx hand review

## pantheon-exec

**[medium] supervisor.rs:215-250 — timeout path leaks the reader thread's
stdout handle by design, but the plugin supervisor is then permanently dead**
After PLUGIN_TIMEOUT the supervisor marks itself dead (`alive = false`) even
though the error says "respawn it". The respawn path exists only at the
session level (plugins reload next session). Within one long session, one
hung plugin call permanently removes that plugin's tools. Fix: add
`respawn()` reusing the original spawn parameters (they are all in the
struct already: dir, timeout, policy), and have the session-level tool
adapter call it once on PLUGIN_TIMEOUT before failing the call. Keep the
one-retry limit so a poison plugin cannot loop.

**[medium] supervisor.rs — stdin writes have no timeout coupling**
`call()` writes the request to stdin, then starts the read deadline. If the
plugin's stdin buffer is full (plugin not reading because it is wedged), the
write itself can block forever before the deadline logic even starts. The
read side is protected; the write side is not. Stdin of a newline-JSON
request is tiny (well under pipe buffer), so in practice this cannot block
until requests exceed 64KiB. A plugin with huge args could wedge. Fix:
either cap request size at the registry layer (reject >32KiB args with
TOOL_ARGS_TOO_LARGE) or move the write into the reader thread. The cap is
simpler and also protects against pathological model output.

**[low] builtins.rs:58 — write_file_unsafe exists as a public tool**
The unsafe (non-atomic, non-journaled) write path is reachable as a tool.
Every other write goes through safewrite. Justify in the tool description or
make it policy-gated separately from safe writes. At minimum the tool
description must say "does not checkpoint; not rollback-able".

**[low] tools.rs — registry has no listing of which capability each tool
maps to at runtime**
`capability_of` exists (used by gate), but there is no `list()` returning
schema+capability together for the the `pantheon runs <id>` path or a future tools.list RPC.
Additive, cheap.

Keep as-is: the reader-thread + deadline pattern in supervisor is correct
and well-commented; process.rs TERM/KILL with lease re-check; safewrite
journal/replay is the strongest subsystem in the repo; compaction policy
is deterministic and tested.

## pantheon-storage

**[medium] ledger.rs — append() takes the mutex, then runs BEGIN IMMEDIATE
per event**
Every event append is its own transaction. Correct for durability but the
per-event fsync cost on run start (RunStarted + ModelRequested + ... in a
burst) makes runs measurably slow on spinning disks. On this EC2 box it does
not matter. Do not batch (correctness first), but document the tradeoff.
If throughput ever matters: group events per turn into one transaction with
the existing immediate-transaction helper.

**[medium] claims.rs + ledger.rs — two claim stores, confirmed**
ARCHITECTURE.md flags it; the audit confirms: ClaimStore (durable
occurrence claims with prune) and Ledger::claim (separate table) overlap.
Scheduler uses ClaimStore; nothing uses Ledger::claim outside its own tests.
Decision: keep ClaimStore, deprecate Ledger::claim with a doc note, remove
at next major. Deleting now risks nothing (zero consumers) but this audit
deliberately avoids destructive change without a separate decision.

**[low] audit.rs — audit_line seq field**
Previously flagged (finding 6 in the phase-7 review, still open): audit
exports entry.id as seq. If id != seq ever diverges, downstream validators
misread. Fix is two lines; do it in the fix pass.

**[low] artifact table grows without bound**
put_artifact has no TTL or size cap; generative-UI blobs live forever in
ledger.db. Fix: size cap at put (reject >8MiB with ARTIFACT_TOO_LARGE) and
a `pantheon artifacts prune --before <ms>` verb later. Cap first.

Keep as-is: terminal-status immutability, CAS transition table, immediate
transactions on the hot paths, hash validation on artifacts.

## pantheon-memory

**[medium] store.rs recall — FTS query builds from raw tokens**
Recall strips tokens to [alnum_-] before quoting (verified safe against
injection in the phase-7 review), but a query of pure stopwords or an empty
string after stripping produces degenerate FTS queries that either match
everything or error. Fix: return Ok(empty) early when the sanitized query
is empty instead of querying.

**[low] markdown.rs sync — sync() direction is implicit**
`memory sync` decides direction from hashes (good) but the CLI output does
not say which direction it chose. A user cannot tell whether store->file or
file->store happened. Print it.

**[low] backend.rs — http backend options are freeform strings**
BackendSelection.options is HashMap<String,String> with no validation per
backend. A typo'd option key silently does nothing. Fix: per-backend
required/known keys validated at selection time.

Keep as-is: propose->policy->provenance write path, sentinel-header markdown
format (v1), narrowing recall order, http backend encoding.

## pantheon-extensions

**[medium] python_runner.rs — hook timeout produces fail-open Ok(None) with
only an eprintln**
A plugin that hangs 10s on every turn degrades every turn silently. The
doctor catches static problems, not runtime latency. Fix: count consecutive
timeouts per plugin in ExtensionManager; after N (say 3), eprintln a loud
warning suggesting removal and skip the plugin for the session. State only
in memory; a session boundary resets it, which is the right scope.

**[low] manager.rs fire() — hooks receive no message payload**
Known from the phase-7 review (finding 7): HookInput.extra is always empty.
pre_llm_call plugins that want the user message cannot get it. The fix is
an extra parameter through fire(); do it in the fix pass.

**[low] manifest.rs — both Hermes manifest spellings accepted but which
one is canonical is undocumented**
Document in the plugin authoring docs: both load, canonical is X.

Keep as-is: fail-open hook contract, doctor's static analysis, sorted
deterministic load order, once-per-session dedup persistence.
