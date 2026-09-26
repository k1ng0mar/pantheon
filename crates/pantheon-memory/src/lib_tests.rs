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
    let rec = propose_write(
        &store,
        &policy,
        proposal(LayerKind::Agent, "nyx", "tz", "Africa/Lagos", "tool:system"),
        4096,
    )
    .unwrap();
    assert_eq!(rec.value, "Africa/Lagos");
    assert_eq!(rec.provenance.origin, "tool:system");
    let hits = recall(&store, &policy, &[LayerKind::Agent], "tz", 10).unwrap();
    assert_eq!(hits.len(), 1);
}
