//! Behavioral / integration tests moved out of the crate per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval`.
//! Tests for `pantheon_memory::tests` - sibling file so sources stay test-free.
use pantheon_api::capability::{Capability, Policy};
use pantheon_api::provenance::TrustTier;
use pantheon_memory::Provenance;
use pantheon_memory::{
    confirm_write, propose_write, recall, LayerBudgets, LayerKind, MemoryConfig, MemoryStore,
    Proposal,
};

fn prov(origin: &str) -> Provenance {
    Provenance {
        source: "import".into(),
        origin: origin.into(),
        trust: pantheon_api::provenance::TrustTier::User,
        recorded_at_ms: 1,
    }
}

/// `coder()` grants `MemoryRead` but deliberately not `MemoryWrite`
/// (see core capability presets); tests that must reach the store (or
/// the validation gates) grant it explicitly.
fn writer_policy() -> Policy {
    Policy::coder().allow(Capability::MemoryWrite)
}

fn proposal(layer: LayerKind, ns: &str, k: &str, v: &str, origin: &str) -> Proposal {
    Proposal {
        layer,
        namespace: ns.into(),
        key: k.into(),
        value: v.into(),
        provenance: prov(origin),
    }
}

// ------------------------------------------------------- namespace isolation
//
// `MemoryStore::search` had no namespace predicate and used `layers` only
// to sort rows it had already fetched, so every recall returned every
// agent's records from the shared store. The write path was already
// namespace-scoped, which made recall the leak. These tests fail against
// the old query.

#[test]
fn recall_does_not_cross_agent_namespaces() {
    let store = MemoryStore::open_in_memory().unwrap();
    // `coder()` grants memory.read but not memory.write, so the fixture
    // needs the writer policy; the point under test is the read scope.
    let policy = writer_policy();
    for (ns, city) in [("agent:nyx", "Kano"), ("agent:zeus", "Lagos")] {
        propose_write(
            &store,
            &policy,
            proposal(LayerKind::Agent, ns, "city", city, "user"),
            4096,
        )
        .unwrap();
    }
    // The word matches both records; the namespace must still separate them.
    let nyx = recall(
        &store,
        &policy,
        &["agent:nyx"],
        &[LayerKind::Agent],
        "city",
        10,
    )
    .unwrap();
    assert_eq!(nyx.len(), 1, "nyx must not see zeus' record");
    assert_eq!(nyx[0].record.namespace, "agent:nyx");
    assert!(nyx[0].record.value.contains("Kano"));

    let zeus = recall(
        &store,
        &policy,
        &["agent:zeus"],
        &[LayerKind::Agent],
        "city",
        10,
    )
    .unwrap();
    assert_eq!(zeus.len(), 1, "zeus must not see nyx' record");
    assert!(zeus[0].record.value.contains("Lagos"));
}

#[test]
fn recall_without_a_namespace_returns_nothing() {
    // The dangerous default: forgetting to scope must yield no data, never
    // every agent's data.
    let store = MemoryStore::open_in_memory().unwrap();
    let policy = writer_policy();
    propose_write(
        &store,
        &policy,
        proposal(LayerKind::Agent, "agent:nyx", "city", "Kano", "user"),
        4096,
    )
    .unwrap();
    let hits = recall(&store, &policy, &[], &[LayerKind::Agent], "city", 10).unwrap();
    assert!(
        hits.is_empty(),
        "an unscoped recall returned {} rows; it must return none",
        hits.len()
    );
}

#[test]
fn an_explicit_wildcard_still_reads_every_namespace() {
    // Operator tooling needs this, and it must stay reachable -- otherwise
    // the fix above would just delete the capability.
    let store = MemoryStore::open_in_memory().unwrap();
    let policy = writer_policy();
    for ns in ["agent:nyx", "agent:zeus"] {
        propose_write(
            &store,
            &policy,
            proposal(LayerKind::Agent, ns, "city", "Somewhere", "user"),
            4096,
        )
        .unwrap();
    }
    let hits = recall(&store, &policy, &["*"], &[LayerKind::Agent], "city", 10).unwrap();
    assert_eq!(hits.len(), 2, "the explicit wildcard must read both");
}

#[test]
fn cross_agent_recall_is_possible_but_never_implicit() {
    // Two namespaces named together is a deliberate act, and it works --
    // shared context for collaboration goes through an explicit scope.
    let store = MemoryStore::open_in_memory().unwrap();
    let policy = writer_policy();
    for (ns, city) in [("agent:nyx", "Kano"), ("agent:zeus", "Lagos")] {
        propose_write(
            &store,
            &policy,
            proposal(LayerKind::Agent, ns, "city", city, "user"),
            4096,
        )
        .unwrap();
    }
    let hits = recall(
        &store,
        &policy,
        &["agent:nyx", "agent:zeus"],
        &[LayerKind::Agent],
        "city",
        10,
    )
    .unwrap();
    assert_eq!(hits.len(), 2);
}

#[test]
fn recall_also_honours_the_layer_filter() {
    // The layer list was previously sort-only, so out-of-layer rows were
    // returned (just ranked last). Now it is a real predicate.
    let store = MemoryStore::open_in_memory().unwrap();
    let policy = writer_policy();
    propose_write(
        &store,
        &policy,
        proposal(
            LayerKind::Agent,
            "agent:nyx",
            "topic",
            "swordsmithing",
            "user",
        ),
        4096,
    )
    .unwrap();
    propose_write(
        &store,
        &policy,
        proposal(
            LayerKind::Global,
            "agent:nyx",
            "topic",
            "agriculture",
            "user",
        ),
        4096,
    )
    .unwrap();
    let hits = recall(
        &store,
        &policy,
        &["agent:nyx"],
        &[LayerKind::Agent],
        "topic",
        10,
    )
    .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].record.layer, LayerKind::Agent);
    assert!(hits[0].record.value.contains("swordsmithing"));
}

#[test]
fn write_without_capability_is_refused() {
    let store = MemoryStore::open_in_memory().unwrap();
    let policy = Policy::researcher_readonly();
    let p = proposal(LayerKind::Agent, "nyx", "city", "Kano", "user");
    let err = propose_write(&store, &policy, p, 4096).unwrap_err();
    assert_eq!(err.code, "MEM_NO_CAPABILITY");
}

#[test]
fn write_without_provenance_is_refused() {
    let store = MemoryStore::open_in_memory().unwrap();
    let policy = writer_policy();
    let mut p = proposal(LayerKind::Agent, "nyx", "city", "Kano", "user");
    p.provenance.origin = "  ".into();
    let err = propose_write(&store, &policy, p, 4096).unwrap_err();
    assert_eq!(err.code, "MEM_NO_PROVENANCE");
}

#[test]
fn ephemeral_turns_never_hit_the_store() {
    let store = MemoryStore::open_in_memory().unwrap();
    let p = proposal(LayerKind::EphemeralTurn, "s1", "scratch", "x", "user");
    let err = propose_write(&store, &writer_policy(), p, 4096).unwrap_err();
    assert_eq!(err.code, "MEM_EPHEMERAL");
}

#[test]
fn recall_returns_provenance_and_narrowest_first() {
    let store = MemoryStore::open_in_memory().unwrap();
    let policy = writer_policy();
    propose_write(
        &store,
        &policy,
        proposal(LayerKind::Global, "g", "stack", "rust runtime", "user"),
        4096,
    )
    .unwrap();
    propose_write(
        &store,
        &policy,
        proposal(
            LayerKind::TaskSession,
            "s1",
            "fix",
            "rust parser bug",
            "tool:cargo",
        ),
        4096,
    )
    .unwrap();
    let layers = [
        LayerKind::TaskSession,
        LayerKind::Agent,
        LayerKind::Project,
        LayerKind::Global,
    ];
    // The fixtures live in two different namespaces ("g" and "s1"), so the
    // caller must name both. Before recall was namespace-aware this test
    // passed with no namespace at all -- it was quietly reading across
    // every namespace in the store, which is exactly the leak.
    let hits = recall(&store, &policy, &["g", "s1"], &layers, "rust", 10).unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].record.layer, LayerKind::TaskSession);
    assert_eq!(hits[1].record.layer, LayerKind::Global);
    assert_eq!(hits[1].record.provenance.origin, "user");
    assert_eq!(hits[0].record.provenance.origin, "tool:cargo");
}

#[test]
fn upsert_replaces_value_and_keeps_provenance_fresh() {
    let store = MemoryStore::open_in_memory().unwrap();
    let policy = writer_policy();
    propose_write(
        &store,
        &policy,
        proposal(LayerKind::Agent, "nyx", "tz", "UTC", "user"),
        4096,
    )
    .unwrap();
    // Same trust tier on both sides, so the second write wins. `propose_write`
    // clamps any non-user origin to Untrusted, so `tool:system` would be
    // blocked by the trust ceiling; `user` is used to exercise the
    // replace-and-refresh path itself.
    let rec = propose_write(
        &store,
        &policy,
        proposal(LayerKind::Agent, "nyx", "tz", "Africa/Lagos", "user"),
        4096,
    )
    .unwrap();
    assert_eq!(rec.value, "Africa/Lagos");
    assert_eq!(rec.provenance.origin, "user");
    let hits = recall(&store, &policy, &["nyx"], &[LayerKind::Agent], "tz", 10).unwrap();
    assert_eq!(hits.len(), 1);
}

/// A `tool:system` origin is clamped to Untrusted by `propose_write`, so it
/// must not overwrite the user-tier record written above. This is the trust
/// ceiling observed through the real public write path rather than through
/// `store.put` directly.

#[test]
fn a_tool_origin_write_cannot_overwrite_a_user_record_via_propose_write() {
    let store = MemoryStore::open_in_memory().unwrap();
    let policy = writer_policy();
    propose_write(
        &store,
        &policy,
        proposal(LayerKind::Agent, "nyx", "tz", "UTC", "user"),
        4096,
    )
    .unwrap();

    propose_write(
        &store,
        &policy,
        proposal(LayerKind::Agent, "nyx", "tz", "Africa/Lagos", "tool:system"),
        4096,
    )
    .unwrap();

    let stored = store.get(LayerKind::Agent, "nyx", "tz").unwrap().unwrap();
    assert_eq!(
        stored.value, "UTC",
        "an untrusted tool-origin write replaced a user record"
    );
    assert_eq!(
        stored.provenance.trust,
        pantheon_api::provenance::TrustTier::User
    );
}

/// A low-trust (model-origin) write must not be able to clobber the value of
/// a record a human confirmed, nor reset its tier. `put` is the write path
/// the model reaches; before the trust ceiling, `ON CONFLICT DO UPDATE SET
/// value=excluded.value, trust=excluded.trust` let a model-origin proposal
/// overwrite a user record outright.

#[test]
fn a_model_origin_write_cannot_overwrite_a_user_confirmed_record() {
    let store = MemoryStore::open_in_memory().unwrap();
    let p = proposal(LayerKind::Agent, "nyx", "city", "Kano", "user");
    let rec = store.put(&p).unwrap();
    assert_eq!(rec.value, "Kano");
    assert_eq!(
        rec.provenance.trust,
        pantheon_api::provenance::TrustTier::User
    );

    // Same key, model-origin, untrusted tier.
    let hostile = Proposal {
        layer: LayerKind::Agent,
        namespace: "nyx".into(),
        key: "city".into(),
        value: "the user lives in Lagos".into(),
        provenance: Provenance {
            source: "tool_output".into(),
            origin: "model".into(),
            trust: pantheon_api::provenance::TrustTier::Untrusted,
            recorded_at_ms: 2,
        },
    };
    let after = store.put(&hostile).unwrap();
    // The stored row is untouched, and `put` reports what is actually
    // stored rather than echoing the proposal back.
    assert_eq!(
        after.value, "Kano",
        "model-origin write clobbered a user record"
    );
    assert_eq!(
        after.provenance.trust,
        pantheon_api::provenance::TrustTier::User,
        "tier was downgraded"
    );
    assert_eq!(after.provenance.origin, "user", "origin was overwritten");
    let stored = store.get(LayerKind::Agent, "nyx", "city").unwrap().unwrap();
    assert_eq!(stored.value, "Kano");
}

/// A higher-trust write still wins, otherwise the ceiling above would freeze
/// every value a user ever wrote.

#[test]
fn a_user_write_still_overwrites_a_lower_tier_record() {
    let store = MemoryStore::open_in_memory().unwrap();
    store
        .put(&proposal(LayerKind::Agent, "nyx", "city", "Kano", "model"))
        .unwrap();
    let user = Proposal {
        layer: LayerKind::Agent,
        namespace: "nyx".into(),
        key: "city".into(),
        value: "Kaduna".into(),
        provenance: Provenance {
            source: "user".into(),
            origin: "user".into(),
            trust: pantheon_api::provenance::TrustTier::User,
            recorded_at_ms: 9,
        },
    };
    let rec = store.put(&user).unwrap();
    assert_eq!(rec.value, "Kaduna");
    assert_eq!(
        rec.provenance.trust,
        pantheon_api::provenance::TrustTier::User
    );
}

/// Same-tier rewrites must keep working: an agent re-asserting its own note
/// is a legitimate write, and the ceiling uses strict `>` for that reason.

#[test]
fn a_same_tier_write_still_updates_the_value() {
    let store = MemoryStore::open_in_memory().unwrap();
    store
        .put(&proposal(LayerKind::Agent, "nyx", "k", "v1", "model"))
        .unwrap();
    let rec = store
        .put(&proposal(LayerKind::Agent, "nyx", "k", "v2", "model"))
        .unwrap();
    assert_eq!(rec.value, "v2");
}

// ------------------------------------------------- layer-scoped promote/get
//
// The row identity is (layer, namespace, key). `promote`/`get` used to
// match on (namespace, key) alone, so confirming a key that existed in
// several layers promoted all of them at once and read back an
// arbitrary row. These tests fail against the old queries.

#[test]
fn promote_and_get_are_layer_scoped() {
    use pantheon_api::provenance::TrustTier;
    let store = MemoryStore::open_in_memory().unwrap();
    let policy = Policy::coder()
        .allow(Capability::MemoryWrite)
        .allow(Capability::MemoryConfirm);
    for layer in [LayerKind::Agent, LayerKind::Project] {
        propose_write(
            &store,
            &policy,
            proposal(layer, "nyx", "shared", "v", "model"),
            4096,
        )
        .unwrap();
    }
    let rec = confirm_write(&store, &policy, LayerKind::Project, "nyx", "shared").unwrap();
    assert_eq!(rec.layer, LayerKind::Project);
    assert_eq!(rec.provenance.trust, TrustTier::Memory);
    let agent = store
        .get(LayerKind::Agent, "nyx", "shared")
        .unwrap()
        .expect("agent row");
    assert_eq!(
        agent.provenance.trust,
        TrustTier::Untrusted,
        "promoting the project row must not touch the agent row"
    );
    let project = store
        .get(LayerKind::Project, "nyx", "shared")
        .unwrap()
        .expect("project row");
    assert_eq!(project.provenance.trust, TrustTier::Memory);
    // A layer that has no such row is a clean not-found, not a row
    // from another layer.
    assert!(store
        .get(LayerKind::Global, "nyx", "shared")
        .unwrap()
        .is_none());
}

/// Confirming under a policy that denies `memory.confirm` fails closed
/// with a structured error, even though `memory.write` is allowed.

#[test]
fn confirm_write_denies_without_memory_confirm() {
    let store = MemoryStore::open_in_memory().unwrap();
    let policy = Policy::coder()
        .allow(Capability::MemoryWrite)
        .deny(Capability::MemoryConfirm);
    propose_write(
        &store,
        &policy,
        proposal(LayerKind::Agent, "nyx", "k", "v", "model"),
        4096,
    )
    .unwrap();
    let err = confirm_write(&store, &policy, LayerKind::Agent, "nyx", "k").unwrap_err();
    assert_eq!(err.code, "MEM_NO_CAPABILITY");
}

// ------------------------------------------------------- secret scan
//
// The write path must refuse values that look like leaked credentials.
// The error names the pattern class, never the secret itself.

#[test]
fn secret_looking_value_is_refused_on_write() {
    let store = MemoryStore::open_in_memory().unwrap();
    let policy = writer_policy();
    let err = propose_write(
        &store,
        &policy,
        proposal(
            LayerKind::Agent,
            "nyx",
            "note",
            "my password = hunter2",
            "user",
        ),
        4096,
    )
    .unwrap_err();
    assert_eq!(err.code, "MEM_SECRET");
    assert!(err.cause.contains("labeled password"), "{}", err.cause);
    assert!(
        !err.cause.contains("hunter2"),
        "the secret itself must not appear in the error: {}",
        err.cause
    );
    assert!(
        store
            .get(LayerKind::Agent, "nyx", "note")
            .unwrap()
            .is_none(),
        "refused write must store nothing"
    );
}

#[test]
fn token_prefix_value_is_refused_on_write() {
    let store = MemoryStore::open_in_memory().unwrap();
    let policy = writer_policy();
    let err = propose_write(
        &store,
        &policy,
        proposal(
            LayerKind::Agent,
            "nyx",
            "deploy",
            "key is sk-abcdefgh12345678 done",
            "user",
        ),
        4096,
    )
    .unwrap_err();
    assert_eq!(err.code, "MEM_SECRET");
    assert!(err.cause.contains("api key prefix"), "{}", err.cause);
}

#[test]
fn prose_about_secrets_is_not_refused() {
    let store = MemoryStore::open_in_memory().unwrap();
    let policy = writer_policy();
    let rec = propose_write(
        &store,
        &policy,
        proposal(
            LayerKind::Agent,
            "nyx",
            "policy",
            "rotate your password regularly per the team policy",
            "user",
        ),
        4096,
    )
    .unwrap();
    assert_eq!(
        rec.value,
        "rotate your password regularly per the team policy"
    );
}

// ------------------------------------------------------- memory.md origin
//
// The MEMORY.md importer is human-authored: its tier survives the write
// gate (the old code clamped every import to Untrusted and silently
// dropped the {trust=user} footers).

#[test]
fn memory_md_origin_keeps_its_tier() {
    let store = MemoryStore::open_in_memory().unwrap();
    let policy = writer_policy();
    let rec = propose_write(
        &store,
        &policy,
        proposal(LayerKind::Agent, "nyx", "city", "Kano", "memory.md"),
        4096,
    )
    .unwrap();
    assert_eq!(
        rec.provenance.trust,
        pantheon_api::provenance::TrustTier::User,
        "memory.md is human-authored and must not be clamped to Untrusted"
    );
}

// ------------------------------------------------------- budgets & eviction

fn tiny_budgets() -> MemoryConfig {
    MemoryConfig {
        max_value_bytes: 4096,
        budgets: LayerBudgets {
            global: 100,
            agent: 100,
            project: 100,
            task_session: 100,
        },
        max_recall_per_layer: 20,
    }
}

fn tiered_proposal(key: &str, trust: pantheon_api::provenance::TrustTier, at: i64) -> Proposal {
    Proposal {
        layer: LayerKind::Agent,
        namespace: "nyx".into(),
        key: key.into(),
        // Distinct FTS token per key ("payload-k1") plus padding to ~30B.
        value: format!("payload-{key} {}", "x".repeat(20)),
        provenance: Provenance {
            source: "test".into(),
            origin: "cli".into(),
            trust,
            recorded_at_ms: at,
        },
    }
}

#[test]
fn put_evicts_lowest_trust_oldest_first_when_over_budget() {
    let store = MemoryStore::open_in_memory().unwrap();
    store.set_config(tiny_budgets()).unwrap();
    // 30 bytes each against a 100-byte Agent budget.
    store
        .put(&tiered_proposal("k1", TrustTier::Untrusted, 1))
        .unwrap();
    store
        .put(&tiered_proposal("k2", TrustTier::Memory, 2))
        .unwrap();
    store
        .put(&tiered_proposal("k3", TrustTier::User, 3))
        .unwrap();
    // 120 > 100: evict k1 (lowest trust, oldest of its tier).
    store
        .put(&tiered_proposal("k4", TrustTier::Untrusted, 4))
        .unwrap();
    assert!(store.get(LayerKind::Agent, "nyx", "k1").unwrap().is_none());
    assert!(store.get(LayerKind::Agent, "nyx", "k4").unwrap().is_some());
    // Evicted rows stop being searchable: the FTS index entry goes with them.
    let gone = store
        .search_scoped(&["nyx"], &[LayerKind::Agent], "payload-k1", 10)
        .unwrap();
    assert!(gone.is_empty(), "evicted row still searchable: {gone:?}");
    // Next over-budget write evicts k4 (untrusted, now the oldest of the
    // lowest tier) - never the row just written.
    store
        .put(&tiered_proposal("k5", TrustTier::Memory, 5))
        .unwrap();
    assert!(store.get(LayerKind::Agent, "nyx", "k4").unwrap().is_none());
    assert!(store.get(LayerKind::Agent, "nyx", "k5").unwrap().is_some());
    assert!(store.get(LayerKind::Agent, "nyx", "k2").unwrap().is_some());
    assert!(store.get(LayerKind::Agent, "nyx", "k3").unwrap().is_some());
}

#[test]
fn put_refuses_a_value_larger_than_the_layer_budget() {
    let store = MemoryStore::open_in_memory().unwrap();
    store.set_config(tiny_budgets()).unwrap();
    let mut p = tiered_proposal("big", TrustTier::User, 1);
    p.value = "x".repeat(101);
    let err = store.put(&p).unwrap_err();
    assert_eq!(err.code, "MEM_BUDGET_EXCEEDED");
    assert!(
        store.get(LayerKind::Agent, "nyx", "big").unwrap().is_none(),
        "over-budget write must store nothing"
    );
}

#[test]
fn memory_config_defaults_are_sane() {
    let cfg = MemoryConfig::default();
    assert_eq!(cfg.max_value_bytes, 4096);
    assert_eq!(cfg.budgets.global, 100 * 1024);
    assert_eq!(cfg.budgets.agent, 200 * 1024);
    assert_eq!(cfg.budgets.project, 200 * 1024);
    assert_eq!(cfg.budgets.task_session, 50 * 1024);
    assert!(cfg.max_recall_per_layer > 0);
    assert_eq!(cfg.budget_for(LayerKind::Agent), Some(200 * 1024));
    assert_eq!(cfg.budget_for(LayerKind::EphemeralTurn), None);
}

// ------------------------------------------------------- sqlite hygiene

#[test]
fn file_backed_store_enables_wal() {
    let dir = std::env::temp_dir().join(format!(
        "pantheon-wal-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("memory.db");
    let store = MemoryStore::open(&db).unwrap();
    drop(store);
    // journal_mode is persistent: a fresh connection sees what open() set.
    let conn = rusqlite::Connection::open(&db).unwrap();
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal", "memory.db must run in WAL mode");
    let _ = std::fs::remove_dir_all(&dir);
}

// ------------------------------------------------------- per-layer recall limits

#[test]
fn recall_enforces_per_layer_limits() {
    let store = MemoryStore::open_in_memory().unwrap();
    store
        .set_config(MemoryConfig {
            max_recall_per_layer: 2,
            ..MemoryConfig::default()
        })
        .unwrap();
    let policy = writer_policy();
    for i in 0..4 {
        propose_write(
            &store,
            &policy,
            proposal(
                LayerKind::Agent,
                "nyx",
                &format!("a{i}"),
                "common marker alpha",
                "user",
            ),
            4096,
        )
        .unwrap();
        propose_write(
            &store,
            &policy,
            proposal(
                LayerKind::Project,
                "nyx",
                &format!("p{i}"),
                "common marker beta",
                "user",
            ),
            4096,
        )
        .unwrap();
    }
    // limit 100 would return everything without the per-layer cap.
    let hits = recall(
        &store,
        &policy,
        &["nyx"],
        &[LayerKind::Agent, LayerKind::Project],
        "common",
        100,
    )
    .unwrap();
    let agent = hits
        .iter()
        .filter(|h| h.record.layer == LayerKind::Agent)
        .count();
    let project = hits
        .iter()
        .filter(|h| h.record.layer == LayerKind::Project)
        .count();
    assert_eq!(agent, 2, "agent layer exceeded its recall cap: {hits:?}");
    assert_eq!(
        project, 2,
        "project layer exceeded its recall cap: {hits:?}"
    );
}
