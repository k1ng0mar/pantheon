//! Behavioral / integration tests moved out of the crate per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval`.
//! Env-sensitive tests use the `byterover` entry (no default URL) so they
//! never race with tests that instantiate other entries.
use pantheon_memory::backend::{BackendRegistry, BackendSelection};
use pantheon_memory::Provenance;
use pantheon_memory::{
    load_selection, open_selected, save_selection, write_via, LayerKind, Proposal,
};

#[test]
fn plugin_factory_reads_url_from_selection_options() {
    let r = BackendRegistry::with_defaults();
    let sel = BackendSelection {
        name: "honcho-cloud".into(),
        options: [("url".to_string(), "http://127.0.0.1:9/v1".to_string())]
            .into_iter()
            .collect(),
    };
    // Construction performs no I/O; only the URL check runs here.
    let backend = r.instantiate_selected(&sel).unwrap();
    // list_agent would need the service; expect a structured conn error,
    // not a panic - proves the adapter is wired. The cause carries the
    // resolved URL, so this also proves options.url won.
    let err = backend.list_agent("nyx").unwrap_err();
    assert_eq!(err.code, "MEM_HTTP_CONN");
    assert!(err.cause.contains("http://127.0.0.1:9/v1"), "{}", err.cause);
}

#[test]
fn bridge_factory_env_fallback_resolution_order() {
    // options.url > PANTHEON_MEMORY_<NAME>_URL > config error (no entry
    // ships a default URL anymore: no vendor speaks Pantheon's protocol).
    const ENV: &str = "PANTHEON_MEMORY_BYTEROVER_URL";
    std::env::remove_var(ENV);

    let r = BackendRegistry::with_defaults();
    let no_opts = || BackendSelection {
        name: "byterover".into(),
        options: std::collections::HashMap::new(),
    };

    // No default, no env, no options: fails closed naming the env var.
    let err = r.instantiate_selected(&no_opts()).unwrap_err();
    assert_eq!(err.code, "MEM_BACKEND_CONFIG");
    assert!(
        err.cause.contains("PANTHEON_MEMORY_BYTEROVER_URL"),
        "{}",
        err.cause
    );

    // Env var is picked up when options are absent.
    std::env::set_var(ENV, "http://127.0.0.1:9/from-env");
    let backend = r.instantiate_selected(&no_opts()).unwrap();
    let err = backend.list_agent("nyx").unwrap_err();
    assert_eq!(err.code, "MEM_HTTP_CONN");
    assert!(
        err.cause.contains("http://127.0.0.1:9/from-env"),
        "{}",
        err.cause
    );

    // options.url wins over the env var.
    let sel = BackendSelection {
        name: "byterover".into(),
        options: [(
            "url".to_string(),
            "http://127.0.0.1:9/from-opts".to_string(),
        )]
        .into_iter()
        .collect(),
    };
    let backend = r.instantiate_selected(&sel).unwrap();
    let err = backend.list_agent("nyx").unwrap_err();
    assert!(
        err.cause.contains("http://127.0.0.1:9/from-opts"),
        "{}",
        err.cause
    );

    std::env::remove_var(ENV);
}

#[test]
fn bridge_backends_fail_closed_without_config() {
    // The docs audit (2026-09-29) removed every vendor default URL: no
    // vendor speaks Pantheon's /v1/memory protocol, so every bridge
    // backend fails closed with MEM_BACKEND_CONFIG until a thin-bridge
    // URL is configured.
    let r = BackendRegistry::with_defaults();
    for name in ["hindsight-local", "supermemory-local", "honcho-local"] {
        let err = r.instantiate(name).unwrap_err();
        assert_eq!(err.code, "MEM_BACKEND_CONFIG", "{name}");
        assert!(err.cause.contains("thin bridge"), "{name}: {}", err.cause);
    }
}

#[test]
fn catalog_is_native_first_then_alphabetical() {
    let r = BackendRegistry::with_defaults();
    let names: Vec<String> = r.catalog().iter().map(|b| b.name.clone()).collect();
    assert_eq!(names[0], "native");
    let mut rest = names[1..].to_vec();
    rest.sort();
    assert_eq!(names[1..].to_vec(), rest);
    assert!(r.catalog().iter().all(|b| !b.recommended));
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
        name: "honcho-cloud".into(),
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
