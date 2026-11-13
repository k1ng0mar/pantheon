//! The file tools cannot read or write Pantheon's secret stores, even
//! when the workspace contains the data dir and PANTHEON_DATA_DIR is not
//! exported (audit SEC-02). Before the fix only `$PANTHEON_DATA_DIR/
//! secrets/**` was denied, and only when that variable was set, so a
//! default install with workspace = $HOME left `~/.pantheon/.env`
//! writable - and that file is loaded into the environment on the next
//! launch.

use pantheon_exec::confine::confine;

#[test]
fn default_data_dir_secrets_are_denied_with_workspace_at_home() {
    let home = tempfile::tempdir().unwrap();
    // One test in this binary: env mutation is safe here.
    std::env::set_var("HOME", home.path());
    std::env::remove_var("PANTHEON_DATA_DIR");
    let data = home.path().join(".pantheon");
    std::fs::create_dir_all(data.join("secrets")).unwrap();
    for f in [
        ".env",
        "logins.env",
        "logins.json",
        ".secrets.key",
        "gateway.env",
    ] {
        std::fs::write(data.join(f), "x").unwrap();
    }
    std::fs::create_dir_all(home.path().join("project")).unwrap();

    for rel in [
        ".pantheon/.env",
        ".pantheon/logins.env",
        ".pantheon/logins.json",
        ".pantheon/.secrets.key",
        ".pantheon/gateway.env",
        ".pantheon/secrets/vault",
        ".ssh/id_ed25519",
        ".netrc",
        ".kube/config",
    ] {
        let e = confine(&home.path().join(rel), home.path())
            .expect_err(&format!("{rel} must be denied"));
        assert!(
            e.code.starts_with("CONFINE") || e.code.contains("DENY"),
            "{rel}: {e:?}"
        );
    }
    // Ordinary workspace files, and non-secret data-dir files, still work.
    assert!(confine(&home.path().join("project/main.rs"), home.path()).is_ok());
    assert!(confine(
        &home.path().join(".pantheon/skills/x/SKILL.md"),
        home.path()
    )
    .is_ok());
}
