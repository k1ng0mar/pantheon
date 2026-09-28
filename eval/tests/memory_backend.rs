//! Behavioral / integration tests moved out of the crate per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval`.
use pantheon_memory::backend::{BackendRegistry, BackendSelection};
use pantheon_memory::Provenance;
use pantheon_memory::{
    load_selection, open_selected, save_selection, write_via, LayerKind, Proposal,
};

#[test]
fn plugin_factory_reads_url_from_selection_options() {
    let r = BackendRegistry::with_defaults();
    let sel = BackendSelection {
        name: "honcho".into(),
        options: [("url".to_string(), "http://127.0.0.1:9/v1".to_string())]
            .into_iter()
            .collect(),
    };
    // Construction performs no I/O; only the URL check runs here.
    let backend = r.instantiate_selected(&sel).unwrap();
    // list_agent would need the service; expect a structured conn error,
    // not a panic — proves the adapter is wired.
    let err = backend.list_agent("nyx").unwrap_err();
    assert_eq!(err.code, "MEM_HTTP_CONN");
}

#[test]
fn selection_round_trips_through_toml() {
    let dir = std::env::temp_dir().join(format!(
        "pantheon-sel-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    assert_eq!(load_selection(&dir), BackendSelection::default());
    let sel = BackendSelection {
        name: "honcho".into(),
        options: [("url".to_string(), "http://x".to_string())]
            .into_iter()
            .collect(),
    };
    save_selection(&dir, &sel).unwrap();
    assert_eq!(load_selection(&dir), sel);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn open_selected_defaults_to_persistent_native() {
    let dir = std::env::temp_dir().join(format!(
        "pantheon-opensel-{}-{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let backend = open_selected(&dir).unwrap();
    // Persistent file exists after first write.
    let policy = pantheon_api::capability::Policy::coder_with_memory();
    let prov = crate::Provenance {
        source: "test".into(),
        origin: "user".into(),
        trust: pantheon_api::provenance::TrustTier::User,
        recorded_at_ms: 0,
    };
    crate::write_via(
        backend.as_ref(),
        &policy,
        crate::Proposal {
            layer: crate::LayerKind::Agent,
            namespace: "nyx".into(),
            key: "k".into(),
            value: "v".into(),
            provenance: prov,
        },
        4096,
    )
    .unwrap();
    assert!(dir.join("memory.db").exists());
    let _ = std::fs::remove_dir_all(&dir);
}
