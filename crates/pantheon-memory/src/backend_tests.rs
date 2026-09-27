//! Tests for `pantheon_memory::backend::tests` — sibling file so sources stay test-free.
use super::*;

#[test]
fn defaults_include_native_http_and_plugin_backends() {
    let r = BackendRegistry::with_defaults();
    let names: Vec<String> = r.list().iter().map(|b| b.name.clone()).collect();
    for want in [
        "native",
        "http",
        "galaxymem",
        "mnemosyne",
        "honcho",
        "hindsight",
        "openviking",
        // OMP's two real backends: local SQLite memory + decision memory.
        "mnemopi",
        "sharpshooter",
    ] {
        assert!(
            names.contains(&want.to_string()),
            "missing {want}: {names:?}"
        );
    }
    assert!(r.contains("native"));
    assert!(r.contains("http"));
}

#[test]
fn register_and_instantiate_round_trips() {
    let mut r = BackendRegistry::with_defaults();
    r.register(
        BackendInfo {
            name: "test".into(),
            label: "Test backend".into(),
            kind: BackendKind::Http,
            capabilities: vec!["memory.read".into()],
        },
        || Ok(Arc::new(MemoryStore::open_in_memory()?) as Arc<dyn MemoryBackend>),
    );
    let info = r.info("test").unwrap();
    assert_eq!(info.label, "Test backend");
    let backend = r.instantiate("test").unwrap();
    // It really is a working memory store.
    let _ = backend.list_agent("nyx").unwrap();
}

#[test]
fn unknown_backend_returns_structured_error() {
    let r = BackendRegistry::with_defaults();
    let err = r.instantiate("nope").unwrap_err();
    assert_eq!(err.code, "MEM_BACKEND_UNKNOWN");
}

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
fn plugin_factory_without_url_is_a_config_error() {
    let r = BackendRegistry::with_defaults();
    let sel = BackendSelection {
        name: "hindsight".into(),
        options: HashMap::new(),
    };
    let err = r.instantiate_selected(&sel).unwrap_err();
    assert_eq!(err.code, "MEM_BACKEND_CONFIG");
    assert!(err.cause.contains("hindsight"), "{}", err.cause);
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
