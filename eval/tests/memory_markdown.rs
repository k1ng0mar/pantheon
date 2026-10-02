//! Behavioral / integration tests moved out of the crate per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval`.
use pantheon_memory::markdown::{detect_conflict, export_agent, import_agent, sync};
use pantheon_memory::Provenance;
use pantheon_memory::{LayerKind, MemoryStore, Proposal};

/// The laundering regression: an Untrusted record exported to
/// MEMORY.md and reimported must come back Untrusted, not User.
#[test]
fn untrusted_record_survives_markdown_round_trip() {
    let store = MemoryStore::open_in_memory().unwrap();
    let policy = pantheon_api::capability::Policy::coder()
        .allow(pantheon_api::capability::Capability::MemoryWrite);
    // Store one untrusted record (as the model would).
    let p = Proposal {
        layer: LayerKind::Agent,
        namespace: "nyx".to_string(),
        key: "injected".into(),
        value: "suspicious claim".into(),
        provenance: Provenance {
            source: "native".into(),
            origin: "model".into(),
            trust: pantheon_api::provenance::TrustTier::Untrusted,
            recorded_at_ms: 1,
        },
    };
    pantheon_memory::propose_write(&store, &policy, p, 4096).unwrap();
    // Export -> import round trip.
    let tmp = std::env::temp_dir().join(format!(
        "pantheon-launder-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&tmp).unwrap();
    let path = tmp.join("MEMORY.md");
    export_agent(&store, "nyx", &path).unwrap();
    // Wipe and reimport (fresh store, same file).
    let store2 = MemoryStore::open_in_memory().unwrap();
    let n = import_agent(&store2, &policy, "nyx", &path).unwrap();
    assert_eq!(n, 1);
    let rec = store2
        .get(LayerKind::Agent, "nyx", "injected")
        .unwrap()
        .unwrap();
    assert_eq!(
        rec.provenance.trust,
        pantheon_api::provenance::TrustTier::Untrusted,
        "markdown round-trip must not launder an untrusted record to user trust"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn import_proposals_go_through_policy_gate() {
    // Use a fresh in-memory store and a policy that grants MemoryWrite.
    let store = MemoryStore::open_in_memory().unwrap();
    let policy = pantheon_api::capability::Policy::coder()
        .allow(pantheon_api::capability::Capability::MemoryWrite);
    let tmp = std::env::temp_dir().join(format!(
        "pantheon-mem-md-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&tmp, "# city\n\nKano\n\n# tz\n\nAfrica/Lagos\n").unwrap();
    let n = import_agent(&store, &policy, "nyx", &tmp).unwrap();
    assert_eq!(n, 2);
    let layers = [LayerKind::Agent];
    let hits = pantheon_memory::recall(&store, &policy, &["nyx"], &layers, "city", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].record.value, "Kano");
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn import_refuses_without_write_capability() {
    let store = MemoryStore::open_in_memory().unwrap();
    // Default `coder()` does NOT grant MemoryWrite.
    let policy = pantheon_api::capability::Policy::coder();
    let tmp = std::env::temp_dir().join(format!(
        "pantheon-mem-md-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&tmp, "# city\n\nKano\n").unwrap();
    let err = import_agent(&store, &policy, "nyx", &tmp).unwrap_err();
    assert_eq!(err.code, "MEM_NO_CAPABILITY");
    let _ = std::fs::remove_file(&tmp);
}

fn writer_policy() -> pantheon_api::capability::Policy {
    use pantheon_api::capability::Capability as C;
    pantheon_api::capability::Policy::coder().allow(C::MemoryWrite)
}

fn fresh_md(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pantheon-mdsync-{}-{}-{}",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn sync_creates_file_on_fresh_install() {
    let dir = fresh_md("fresh");
    let store = MemoryStore::open_in_memory().unwrap();
    let md = dir.join("MEMORY.md");
    let report = sync(&store, &writer_policy(), "nyx", &md, None).unwrap();
    assert!(!report.imported);
    assert!(report.exported);
    assert!(md.exists());
    let text = std::fs::read_to_string(&md).unwrap();
    assert!(text.contains("# Agent memory"));
}

#[test]
fn sync_imports_when_file_changes() {
    let dir = fresh_md("import");
    let store = MemoryStore::open_in_memory().unwrap();
    let md = dir.join("MEMORY.md");
    // Initial sync seeds the file from the (empty) store.
    let r1 = sync(&store, &writer_policy(), "nyx", &md, None).unwrap();
    assert!(r1.exported);
    // User edits the file outside the harness.
    std::fs::write(&md, "# city\n\nKano\n").unwrap();
    // Next sync imports the new content and re-exports.
    let r2 = sync(&store, &writer_policy(), "nyx", &md, Some(&r1.file_hash)).unwrap();
    assert!(r2.imported);
    assert!(r2.exported);
    let layers = [LayerKind::Agent];
    let hits =
        pantheon_memory::recall(&store, &writer_policy(), &["nyx"], &layers, "city", 10).unwrap();
    assert_eq!(hits.len(), 1);
}

#[test]
fn sync_noop_when_nothing_changed() {
    let dir = fresh_md("noop");
    let store = MemoryStore::open_in_memory().unwrap();
    let md = dir.join("MEMORY.md");
    let r1 = sync(&store, &writer_policy(), "nyx", &md, None).unwrap();
    let r2 = sync(&store, &writer_policy(), "nyx", &md, Some(&r1.file_hash)).unwrap();
    assert!(!r2.imported);
    assert!(!r2.exported);
}

#[test]
fn detect_conflict_when_both_sides_changed() {
    let dir = fresh_md("conflict");
    let store = MemoryStore::open_in_memory().unwrap();
    let md = dir.join("MEMORY.md");
    let r1 = sync(&store, &writer_policy(), "nyx", &md, None).unwrap();
    // File changes outside.
    std::fs::write(&md, "# from_file\n\nX\n").unwrap();
    // Store changes inside (a new record through propose_write).
    pantheon_memory::propose_write(
        &store,
        &writer_policy(),
        crate::Proposal {
            layer: LayerKind::Agent,
            namespace: "nyx".into(),
            key: "from_store".into(),
            value: "Y".into(),
            provenance: Provenance {
                source: "test".into(),
                origin: "test".into(),
                trust: pantheon_api::provenance::TrustTier::User,
                recorded_at_ms: 0,
            },
        },
        4096,
    )
    .unwrap();
    let conflict = detect_conflict(&store, "nyx", &md, Some(&r1.file_hash)).unwrap();
    assert!(conflict.is_some(), "expected conflict to be detected");
}

#[test]
fn sync_no_conflict_when_only_store_changed() {
    let dir = fresh_md("store_only");
    let store = MemoryStore::open_in_memory().unwrap();
    let md = dir.join("MEMORY.md");
    let r1 = sync(&store, &writer_policy(), "nyx", &md, None).unwrap();
    pantheon_memory::propose_write(
        &store,
        &writer_policy(),
        crate::Proposal {
            layer: LayerKind::Agent,
            namespace: "nyx".into(),
            key: "new".into(),
            value: "value".into(),
            provenance: Provenance {
                source: "test".into(),
                origin: "test".into(),
                trust: pantheon_api::provenance::TrustTier::User,
                recorded_at_ms: 0,
            },
        },
        4096,
    )
    .unwrap();
    // File is unchanged from the last sync.
    let conflict = detect_conflict(&store, "nyx", &md, Some(&r1.file_hash)).unwrap();
    assert!(conflict.is_none());
    let r2 = sync(&store, &writer_policy(), "nyx", &md, Some(&r1.file_hash)).unwrap();
    assert!(!r2.imported);
    assert!(r2.exported);
    // After sync, the file reflects the new record.
    let text = std::fs::read_to_string(&md).unwrap();
    assert!(text.contains("new"));
}

/// A v2 file's {trust=user} footer survives import on a fresh store: the
/// old gate clamped every import to Untrusted and silently dropped the
/// file's tiers.

#[test]
fn import_keeps_file_tier_for_new_rows() {
    let store = MemoryStore::open_in_memory().unwrap();
    let policy = writer_policy();
    let dir = fresh_md("keeptier");
    let path = dir.join("MEMORY.md");
    std::fs::write(
        &path,
        "<!-- pantheon:agent-memory v2 -->\n\n# Agent memory\n\n# city\n\n{trust=user}\nKano\n",
    )
    .unwrap();
    let n = import_agent(&store, &policy, "nyx", &path).unwrap();
    assert_eq!(n, 1);
    let rec = store.get(LayerKind::Agent, "nyx", "city").unwrap().unwrap();
    assert_eq!(
        rec.provenance.trust,
        pantheon_api::provenance::TrustTier::User,
        "human-authored import must keep the file's tier"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// An import never upgrades trust: the file claims `user` but the store
/// already has the row at `memory`, so the row stays `memory` while the
/// human's edited value still lands.

#[test]
fn import_caps_file_tier_at_existing_store_tier() {
    let store = MemoryStore::open_in_memory().unwrap();
    let policy = writer_policy();
    let seed = Proposal {
        layer: LayerKind::Agent,
        namespace: "nyx".to_string(),
        key: "city".into(),
        value: "Old".into(),
        provenance: Provenance {
            source: "test".into(),
            origin: "cli".into(),
            trust: pantheon_api::provenance::TrustTier::Memory,
            recorded_at_ms: 1,
        },
    };
    store.put(&seed).unwrap();
    let dir = fresh_md("capup");
    let path = dir.join("MEMORY.md");
    std::fs::write(
        &path,
        "<!-- pantheon:agent-memory v2 -->\n\n# Agent memory\n\n# city\n\n{trust=user}\nKano\n",
    )
    .unwrap();
    import_agent(&store, &policy, "nyx", &path).unwrap();
    let rec = store.get(LayerKind::Agent, "nyx", "city").unwrap().unwrap();
    assert_eq!(
        rec.provenance.trust,
        pantheon_api::provenance::TrustTier::Memory,
        "import must not upgrade the stored tier"
    );
    assert_eq!(rec.value, "Kano", "the human's edited value still lands");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The reverse is also safe: a file claiming a lower tier cannot clobber
/// a higher-trust row - the store's anti-clobber rule holds the value and
/// the tier.

#[test]
fn import_does_not_downgrade_a_higher_trust_row() {
    let store = MemoryStore::open_in_memory().unwrap();
    let policy = writer_policy();
    let seed = Proposal {
        layer: LayerKind::Agent,
        namespace: "nyx".to_string(),
        key: "city".into(),
        value: "Confirmed".into(),
        provenance: Provenance {
            source: "test".into(),
            origin: "cli".into(),
            trust: pantheon_api::provenance::TrustTier::User,
            recorded_at_ms: 1,
        },
    };
    store.put(&seed).unwrap();
    let dir = fresh_md("capdown");
    let path = dir.join("MEMORY.md");
    std::fs::write(
        &path,
        "<!-- pantheon:agent-memory v2 -->\n\n# Agent memory\n\n# city\n\n{trust=memory}\nEdited\n",
    )
    .unwrap();
    import_agent(&store, &policy, "nyx", &path).unwrap();
    let rec = store.get(LayerKind::Agent, "nyx", "city").unwrap().unwrap();
    assert_eq!(
        rec.provenance.trust,
        pantheon_api::provenance::TrustTier::User
    );
    assert_eq!(rec.value, "Confirmed");
    let _ = std::fs::remove_dir_all(&dir);
}
