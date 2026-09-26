# Comparative audit notes (NyxA pass, pre-agent-findings)

Scope: cross-crate consistency, dead weight, architecture-level observations.
Per-crate detail lives in group-A/B/C.md from the review agents.

## Orphan crates (zero consumers outside themselves)

Six crates compile and test green but nothing in the CLI/runtime path uses
them: pantheon-mcp (99), pantheon-scheduler (1012), pantheon-secrets (997),
pantheon-sandbox (416), pantheon-migrate (323), pantheon-swarm (294). Plus
pantheon-otel (184) which maps events to spans but no code calls it.

Assessment: these are spec-section implementations (sections 3, 10, 13, 15,
19, 21, 23) built ahead of their wiring. They carry 71 passing tests. They are
NOT dead code in the "delete" sense; they are unbuilt bridges. Options per
crate:

- pantheon-swarm: the runtime already has a Delegate denial path
  (SWARM_SPAWN_DENIED in session.rs). Wire Caps checks into that path or drop
  the crate until real spawning exists.
- pantheon-scheduler: the durable claim ledger is real infrastructure; wire
  `pantheon schedule` CLI verb or leave documented as forward work.
- pantheon-secrets: the broker is exactly what api_key_env should graduate to.
  Natural next wiring.
- pantheon-mcp / pantheon-migrate: small, self-contained, harmless. Document
  as libraries awaiting CLI surfaces.
- pantheon-otel: span_for is referenced by no one. Either fold explain()'s
  output through it or accept it as the future export format.

Recommendation: wire secrets + swarm caps now (cheap, high value), document
the rest as "implemented, not yet wired" in ARCHITECTURE.md (it already does
this for some, inconsistently).

## Cross-crate inconsistencies spotted by hand

1. Error layer values: pipeline errors use Layer::Agent (pipeline.rs,
   pipeline_runner.rs) but they are runtime orchestration, not model turns.
   Layer::Runtime fits. Small but systematic.
2. Two claim mechanisms coexist: ClaimStore (pantheon-storage claims.rs) and
   Ledger::claim. ARCHITECTURE.md already flags this (open gap). Decision
   needed, not code.
3. Mock provider selection: `--provider mock` in eval cases does nothing by
   itself; mock mode is actually PANTHEON_MOCK_FILE env. The eval case
   passing is because run.py sets the env. CLI help does not say this.
   Confusing for anyone trying `--provider mock` by hand.
4. CLI help text (main.rs help()) lists 20 verbs but misses: setup, reset,
   pipeline, gateway, memory, stage/apply/checkpoint/rollback (some present).
   Stale inventory.
5. README "CLI" section lists 6 verbs; the binary now has ~25. Stale.
6. ARCHITECTURE.md staleness: section 11 says memory "not yet wired into the
   agent loop" but it is (session.rs mem_recall); section 16 says live
   gateways "not wired" but pantheon gateway + daemon + websocket exist;
   section 17 lists 6 CLI verbs (now ~25); section 18's command list is
   aspirational vs implemented agui.* methods.
7. Version drift: workspace crates all 0.1.0, fine, but Cargo.toml members
   list vs crate map table in ARCHITECTURE.md differ in ordering/coverage.

## Test/eval coverage gaps

- No eval case exercises: setup, doctor (system form), reset, pipeline,
  gateway flag validation. The eval file even has a skipped case needing the
  binary (mock-chat). Cheap wins available.
- No integration test that a config.toml written by setup is actually honored
  by chat (model/provider selection). Unit tests cover load/save only.

## Things that are good and must not be regressed

- Event-sourced ledger as single source of truth; `pantheon runs <id>` offline.
- The deny-settle semantics (denied approval becomes a transcript result).
- Lease CAS + PID-reuse protections in process-group kills.
- MockTransport-driven deterministic evals.
- safewrite checkpoint/journal discipline.
- Zero-warning workspace build discipline.
