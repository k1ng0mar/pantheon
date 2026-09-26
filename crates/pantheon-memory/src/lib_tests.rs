//! Tests for `pantheon_memory::tests` — sibling file so sources stay test-free.
use super::*;

fn prov(origin: &str) -> Provenance {
    Provenance {
        source: "import".into(),
        origin: origin.into(),
        trust: pantheon_core::provenance::TrustTier::User,
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
    let hits = recall(&store, &policy, &layers, "rust", 10).unwrap();
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
    let hits = recall(&store, &policy, &[LayerKind::Agent], "tz", 10).unwrap();
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

    let stored = store.get("nyx", "tz").unwrap().unwrap();
    assert_eq!(
        stored.value, "UTC",
        "an untrusted tool-origin write replaced a user record"
    );
    assert_eq!(
        stored.provenance.trust,
        pantheon_core::provenance::TrustTier::User
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
        pantheon_core::provenance::TrustTier::User
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
            trust: pantheon_core::provenance::TrustTier::Untrusted,
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
        pantheon_core::provenance::TrustTier::User,
        "tier was downgraded"
    );
    assert_eq!(after.provenance.origin, "user", "origin was overwritten");
    let stored = store.get("nyx", "city").unwrap().unwrap();
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
            trust: pantheon_core::provenance::TrustTier::User,
            recorded_at_ms: 9,
        },
    };
    let rec = store.put(&user).unwrap();
    assert_eq!(rec.value, "Kaduna");
    assert_eq!(
        rec.provenance.trust,
        pantheon_core::provenance::TrustTier::User
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
