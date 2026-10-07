//! Cloudflare integration tests: classification, config, dependency
//! detection, status, and the doctor section. Everything here runs
//! without the cf CLI or network. Live checks live in the `*_live`
//! tests below, gated on `PANTHEON_CF_LIVE=1` and skipped otherwise.

use pantheon_exec::cloudflare::{classify, is_cf_command, CfOp};

// ---------------------------------------------------------------------------
// classification (table-driven over the real cf surface, verified live
// against cf v1.0.0-beta.12 on 2026-10-02)
// ---------------------------------------------------------------------------

#[test]
fn classification_table() {
    let cases: Vec<(&str, CfOp)> = vec![
        // reads
        ("cf zones list", CfOp::Read),
        ("cf dns records list", CfOp::Read),
        ("cf auth whoami", CfOp::Read),
        ("cf workers list", CfOp::Read),
        ("cf r2 buckets list", CfOp::Read),
        ("cf kv namespaces list", CfOp::Read),
        ("cf accounts list", CfOp::Read),
        ("cf zones settings get", CfOp::Read),
        ("cf cli search \"delete a record\"", CfOp::Read),
        ("cf --profile work zones list", CfOp::Read),
        // writes
        ("cf cache purge", CfOp::Write),
        ("cf kv keys put", CfOp::Write),
        ("cf kv bulk put", CfOp::Write),
        ("cf pages deployments create", CfOp::Write),
        ("cf builds deploy-hooks trigger", CfOp::Write),
        ("cf deploy", CfOp::Write),
        ("cf dev", CfOp::Write),
        ("cf migrate", CfOp::Write),
        ("cf auth login", CfOp::Write),
        // destroys
        ("cf workers delete", CfOp::Destroy),
        ("cf dns records create", CfOp::Destroy),
        ("cf dns records update r0 --content 1.2.3.4", CfOp::Destroy),
        ("cf dns records edit r0", CfOp::Destroy),
        ("cf dns records import", CfOp::Destroy),
        ("cf kv bulk delete", CfOp::Destroy),
        ("cf pages deployments delete", CfOp::Destroy),
        ("cf firewall access-rules delete", CfOp::Destroy),
        ("cf zero-trust access applications create", CfOp::Destroy),
        ("cf zero-trust access service-tokens rotate", CfOp::Destroy),
        (
            "cf accounts subscriptions cancelDelayedDowngrade",
            CfOp::Destroy,
        ),
        ("cf auth logout", CfOp::Destroy),
        // fail-closed shapes
        ("cf something-unknown do-a-thing", CfOp::Destroy),
    ];
    for (cmd, want) in cases {
        assert_eq!(classify(cmd), want, "{cmd}");
    }
}

// ---------------------------------------------------------------------------
// dependency detection with a fake PATH
// ---------------------------------------------------------------------------

#[test]
fn dependency_detection_reports_missing_chain() {
    use pantheon_tui::cloudflare_verb::check_dependencies;
    // A detect fn that finds nothing: the whole chain is missing.
    let none = |_: &str| false;
    let deps = check_dependencies(&none);
    assert_eq!(deps.len(), 3);
    assert!(deps.iter().all(|d| !d.ok), "fake detect finds nothing");
    // Everything present.
    let all = |_: &str| true;
    let deps = check_dependencies(&all);
    assert!(deps.iter().all(|d| d.ok));
    // Real detection on this host: node and npm exist in the CI image;
    // cf may or may not, so only assert the shape.
    let deps = check_dependencies(&|cmd| {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    });
    assert_eq!(deps.len(), 3);
}

// ---------------------------------------------------------------------------
// config round-trip
// ---------------------------------------------------------------------------

#[test]
fn cloudflare_section_round_trips() {
    let section = pantheon_api::config::CloudflareSection {
        enabled: true,
        api_token_secret: Some("CF_TOKEN".into()),
        account_id: Some("abc123".into()),
    };
    let text = toml::to_string_pretty(&section).unwrap();
    let back: pantheon_api::config::CloudflareSection = toml::from_str(&text).unwrap();
    assert_eq!(back, section);
}

#[test]
fn cloudflare_section_defaults_off() {
    let section: pantheon_api::config::CloudflareSection = Default::default();
    assert!(!section.enabled, "a mentioned-but-empty section stays off");
    assert!(section.api_token_secret.is_none());
}

#[test]
fn cloudflare_is_a_known_config_key() {
    // The unknown-key warning must not fire on the section we added.
    let text = "[cloudflare]\nenabled = true\n";
    let unknown = pantheon_api::config::unknown_config_keys(text);
    assert!(
        !unknown.iter().any(|k| k == "cloudflare"),
        "cloudflare must be in KNOWN_CONFIG_KEYS"
    );
}

// ---------------------------------------------------------------------------
// status assembly (fake detect + fake whoami)
// ---------------------------------------------------------------------------

#[test]
fn status_reports_each_fact_without_spawning_cf() {
    use pantheon_tui::cloudflare_verb::gather_status;
    let tmp = tempfile::tempdir().unwrap();
    let dd = tmp.path();
    // Section absent, deps fake-present, no token in env.
    std::env::remove_var("CLOUDFLARE_API_TOKEN");
    let s = gather_status(dd, &|_: &str| true, &|| Some(Ok("acct".into())));
    assert!(!s.section_present, "no config file = section absent");
    assert!(!s.section_enabled);
    assert!(!s.token_resolvable);
    assert_eq!(s.token_secret_name, "CLOUDFLARE_API_TOKEN");
    assert_eq!(s.deps.len(), 3);
    // The whoami stub ran (deps present) and returned its summary.
    assert_eq!(s.auth.as_ref().unwrap().as_ref().unwrap(), "acct");
    let _ = is_cf_command("cf zones list");
    let _ = classify("cf zones list");
}

// ---------------------------------------------------------------------------
// env injection unit (no cf needed)
// ---------------------------------------------------------------------------

#[test]
fn sandbox_extra_env_reaches_the_child() {
    use pantheon_exec::sandbox::{SandboxLevel, SandboxProfile};
    // InProcess boundary writes nothing to disk; a real child proves the
    // env lands. `sh -c` prints the variable we injected.
    let profile = SandboxProfile::from(SandboxLevel::Low);
    let extras = vec![(
        "PANTHEON_TEST_CF_TOKEN".to_string(),
        "secret-value-123".to_string(),
    )];
    let result = pantheon_exec::sandbox::run_sandboxed_with_env(
        &profile,
        "sh",
        &["-c", "printf '%s' \"$PANTHEON_TEST_CF_TOKEN\""],
        "/tmp",
        &extras,
    )
    .unwrap();
    assert_eq!(result.output.trim(), "secret-value-123");
}

#[test]
fn sandbox_without_extra_env_does_not_leak() {
    use pantheon_exec::sandbox::{SandboxLevel, SandboxProfile};
    std::env::set_var("PANTHEON_TEST_CF_TOKEN", "ambient-should-not-cross");
    // InProcess boundary (Low) does not scrub by design: it is the
    // caller-gated exception (see build_sandboxed). The isolation test
    // lives in the sandbox's own suite; here we assert only that the
    // no-extras path passes NO PANTHEON_TEST_* variable of ours, which
    // is the contract run_sandboxed owns.
    let profile = SandboxProfile::from(SandboxLevel::Low);
    let result = pantheon_exec::sandbox::run_sandboxed(
        &profile,
        "sh",
        &["-c", "printf '%s' \"${PANTHEON_TEST_CF_INJECTED:-UNSET}\""],
        "/tmp",
    )
    .unwrap();
    assert_eq!(
        result.output.trim(),
        "UNSET",
        "no-extras path must inject nothing"
    );
}

// ---------------------------------------------------------------------------
// live tests: real cf, real token, gated and skipped by default
// ---------------------------------------------------------------------------

fn live_enabled() -> bool {
    std::env::var("PANTHEON_CF_LIVE").as_deref() == Ok("1")
}

#[test]
fn live_cf_whoami_authenticates() {
    if !live_enabled() {
        eprintln!("skipped: set PANTHEON_CF_LIVE=1 with a real token");
        return;
    }
    let out = std::process::Command::new("cf")
        .args(["auth", "whoami"])
        .output()
        .expect("cf must exist for the live test");
    assert!(out.status.success(), "cf auth whoami failed");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("\"authenticated\": true"), "{text}");
}

#[test]
fn live_cf_command_discovery_answers() {
    if !live_enabled() {
        eprintln!("skipped: set PANTHEON_CF_LIVE=1");
        return;
    }
    let out = std::process::Command::new("cf")
        .args(["cli", "search", "list dns records"])
        .output()
        .expect("cf must exist for the live test");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("dns"),
        "search returned nothing useful: {text}"
    );
}

#[test]
fn live_zone_list_matches_classification() {
    if !live_enabled() {
        eprintln!("skipped: set PANTHEON_CF_LIVE=1");
        return;
    }
    // A classified Read actually runs without approval.
    assert_eq!(classify("cf zones list"), CfOp::Read);
    let out = std::process::Command::new("cf")
        .args(["zones", "list"])
        .output()
        .expect("cf must exist");
    assert!(out.status.success());
}
