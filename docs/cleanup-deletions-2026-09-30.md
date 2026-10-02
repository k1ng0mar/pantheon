# Cleanup-pass deletions - 2026-09-30

The concurrent "Rust cleanup pass" deleted 62 files. Most were legitimate
(dead code, authorized consolidations). The items below were **not** dead
code: public API and documented features with tests. I restored the
security-critical ones; the rest need your call.

## Restored (suite was red without them)

| Deleted | What it is | Why it matters |
|---|---|---|
| `pantheon-scheduler/src/webhook.rs` | Inbound webhook triggers (§21): HMAC-SHA256 signature verification | Security boundary. Without it, anyone reaching the endpoint can inject job triggers. Eval test `scheduler_webhook` (8 tests) covers the contract. |
| `pantheon-scheduler/src/idempotency.rs` | `ClaimLedger`, `occurrence_key`, `runs_for_missed` | Durability correctness: replayed occurrences must not double-fire. |
| `pantheon-scheduler/src/durable.rs` methods | `DurableClaimLedger::is_claimed/release/len/is_empty/names` | Used by eval tests `scheduler_durable` (6) and `storage_claims` (5). |
| `pantheon-gateway/src/allowlist.rs` | `Allowlist`, `Admission`, `Pairing` - gateway admission control | Public API re-exported at crate root. |
| `pantheon-gateway/src/canonical.rs` | `Canonical`, `Command`, `Conversation`, `Reaction` - normalized message types | Public API; the §16 "agent never knows which surface sent it" seam. |
| `pantheon-exec/src/acp.rs` | Agent Client Protocol handshake client (§6 interop seam) | 291 lines, deliberate: "no adapter code per harness, just one client speaking the protocol." Eval test `exec_acp` (2 tests). |
| `AgentRuntime::run_agent` / `send` | Run-ownership lookup + agent messaging | Eval test `runtime_agent_runtime` (22 tests): run-binding guarantee, message-is-data-not-instruction. |

Notes on the restoration:
- `ScheduleKind::Webhook` was re-added (the cleanup had removed the variant and made legacy webhook rows skip with a warning). Webhook jobs are never due on the tick; they fire on inbound calls.
- `Job.agent` replaced the old `Job.target_agent`; `webhook::Fire` now carries `agent`.
- `MissedPolicy::{Skip,RunOnce,CatchUp}` restored as a real policy type (was demoted to a private legacy shim).
- `hmac`/`sha2` re-added to `pantheon-scheduler/Cargo.toml`.
- Mermaid parser bug fixed along the way: `render_mermaid` returned `None` on edge labels (`A-->|yes|B`).

## Deleted and NOT restored - your call

| Deleted | What it is | Tests deleted with it |
|---|---|---|
| `pantheon-extensions/src/compat.rs` | OpenClaw/OMP extension compat adapter (arch doc §8) | `eval/tests/extensions_compat.rs` |
| `pantheon-extensions/src/js_runner.rs` | JS/TS hook runner (other half of compat) | `eval/tests/extensions_js.rs` |
| `pantheon-tui/src/session/overview.rs` | Session overview UI | (unit tests in-file) |

Deliberately left deleted (your earlier decision):
- `pantheon-tui/src/approval_notify.rs` - phone approval notifications, which you had removed 2026-09-29.

## Legitimate deletions (no action)

- Crate consolidation: `pantheon-capability/`, `pantheon-consolidate/`, `pantheon-swarm/`, `pantheon-sandbox/`, `pantheon-browser/`, `pantheon-websearch/`, `pantheon-reflect/` (authorized merges).
- `pantheon-dashboard/src/server.rs`, `pantheon-runtime/src/serve.rs`, `pantheon-runtime/src/transport.rs` - the authorized gateway serve-surface refactor (runtime stays client-agnostic; gateway owns sockets). The `runtime_transport` eval test was rewritten to exercise the dispatcher in-process.
- `pantheon-agent/src/agent_profile.rs` → moved to `pantheon-api/src/agent_profile.rs` (authorized config-doc consolidation).

## Verification

- `cargo test --workspace`: **142 targets, 2,222 passed, 0 failed** (exit 0).
- `cargo fmt --check`: clean.
- Nothing committed or pushed.
