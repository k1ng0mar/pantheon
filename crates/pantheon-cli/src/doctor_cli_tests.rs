//! Tests for `pantheon_cli::doctor_cli::tests` — sibling file so sources stay test-free.
use super::*;

#[test]
fn empty_data_dir_fails_config_but_reports_the_fix() {
    let dir = std::env::temp_dir().join(format!("pantheon-doctor-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let rep = run_system_doctor(&dir);
    assert!(!rep.ok);
    let cfg_check = rep.checks.iter().find(|c| c.section == "config").unwrap();
    assert_eq!(cfg_check.status, "fail");
    assert!(cfg_check.fix.contains("setup"));
    // Ledger and memory still get checked even without config.
    assert!(rep.checks.iter().any(|c| c.section == "ledger"));
    assert!(rep.checks.iter().any(|c| c.section == "memory"));
}

#[test]
fn configured_data_dir_passes() {
    let dir = std::env::temp_dir().join(format!("pantheon-doctor-ok-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    crate::setup_cli::run_setup(
        &dir,
        crate::setup_cli::SetupAnswers {
            provider: Some("local".into()),
            model: Some("llama3.2".into()),
            ..Default::default()
        },
        true,
    );
    let rep = run_system_doctor(&dir);
    let cfg_check = rep.checks.iter().find(|c| c.section == "config").unwrap();
    assert_eq!(cfg_check.status, "ok");
    assert!(rep.ok, "checks: {:?}", rep.checks);
}

#[test]
fn doctor_lists_agent_identities_by_display_name() {
    let dir = std::env::temp_dir().join(format!("pantheon-doctor-agents-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    crate::setup_cli::run_setup(
        &dir,
        crate::setup_cli::SetupAnswers {
            provider: Some("local".into()),
            model: Some("llama3.2".into()),
            ..Default::default()
        },
        true,
    );
    // No agents: anonymous, reported once.
    let rep = run_system_doctor(&dir);
    assert!(rep
        .checks
        .iter()
        .any(|c| c.section == "agents" && c.detail.contains("anonymous")));
    // One agent: listed by effective name.
    let mut cfg = crate::config_doc::Config::load(&dir).unwrap();
    cfg.agents.insert(
        "nyx".into(),
        crate::config_doc::AgentIdentity {
            display_name: Some("Nyx".into()),
            policy: Some("coder".into()),
            ..Default::default()
        },
    );
    cfg.save(&dir).unwrap();
    let rep = run_system_doctor(&dir);
    let agent_check = rep
        .checks
        .iter()
        .find(|c| c.section == "agents" && c.detail.contains("nyx"))
        .unwrap();
    assert!(
        agent_check.detail.contains("Nyx"),
        "detail: {}",
        agent_check.detail
    );
    assert!(
        agent_check.detail.contains("agent:nyx"),
        "detail: {}",
        agent_check.detail
    );
    let _ = std::fs::remove_dir_all(&dir);
}
