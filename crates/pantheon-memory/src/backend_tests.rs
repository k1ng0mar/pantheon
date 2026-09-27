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
