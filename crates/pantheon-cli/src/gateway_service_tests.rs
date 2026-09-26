//! Tests for the gateway service unit. The unit body is a config artifact
//! the user will read and edit, so its contents are part of the contract:
//! a unit that restarts nothing, or that inlines a secret, is a bug even
//! though no Rust assertion would otherwise notice.
use super::*;

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    #[test]
    fn unit_restarts_and_survives_a_reboot() {
        let body = unit_body(
            std::path::Path::new("/usr/local/bin/pantheon"),
            std::path::Path::new("/home/u/.pantheon"),
        );
        assert!(
            body.contains("Restart=always"),
            "a crashed gateway stays down"
        );
        assert!(
            body.contains("WantedBy=default.target"),
            "nothing brings it back after a reboot"
        );
    }

    #[test]
    fn exec_start_is_absolute_and_runs_the_foreground_loop() {
        let body = unit_body(
            std::path::Path::new("/opt/pantheon/pantheon"),
            std::path::Path::new("/data"),
        );
        // A relative or bare name resolves against the unit's own PATH, which
        // is not the shell's: the classic "works by hand, dies as a service".
        assert!(body.contains("ExecStart=/opt/pantheon/pantheon gateway run"));
    }

    #[test]
    fn unit_inlines_no_secret_material() {
        let body = unit_body(
            std::path::Path::new("/usr/local/bin/pantheon"),
            std::path::Path::new("/data"),
        );
        for needle in ["TOKEN", "SECRET", "KEY", "BEARER"] {
            assert!(
                !body.to_uppercase().contains(needle),
                "unit must reference the data dir, never inline {needle}"
            );
        }
        // It should still point the binary at its config.
        assert!(body.contains("Environment=PANTHEON_DATA_DIR=/data"));
    }

    #[test]
    fn unit_path_follows_systemd_user_config_dir() {
        // `unit_path` reads the environment, so this shares the crate's env
        // lock with every other test that mutates one. Setting a var that
        // another test also reads is the same race as writing a config file
        // another test also writes, and it fails just as silently.
        let _lock = crate::dotenv::test_support::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("SYSTEMD_USER_CONFIG_DIR", "/tmp/pt-units-a7f3");
        let p = unit_path();
        assert!(p.ends_with("pantheon-gateway.service"), "{p:?}");
        assert!(
            p.starts_with("/tmp/pt-units-a7f3"),
            "relocated unit dir was ignored: {p:?}"
        );
        std::env::remove_var("SYSTEMD_USER_CONFIG_DIR");
    }
}
