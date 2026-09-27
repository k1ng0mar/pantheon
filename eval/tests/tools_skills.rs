//! Behavioral / integration tests moved out of the crate per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval`.
//! Tests for `pantheon_tools::skill_tools` — the registration half that
//! split out of `pantheon-exec::skills_tests` (capability ≠ tool).
use pantheon_exec::skills::load_skill;
use pantheon_tools::skill_tools::register_skill_tools;
use pantheon_tools::tools::ToolRegistry;
use std::path::{Path, PathBuf};

fn skill_dir(base: &Path, name: &str, desc: &str) -> PathBuf {
    let d = base.join(name);
    std::fs::create_dir_all(&d).unwrap();
    let p = d.join("SKILL.md");
    std::fs::write(
        &p,
        format!("---\nname: {name}\ndescription: \"{desc}\"\n---\n\n# {name}\n\nBody of {name}.\n"),
    )
    .unwrap();
    p
}
#[test]
fn registry_tools_list_and_read() {
    let dir = std::env::temp_dir().join(format!("sk-{}-{}", std::process::id(), 4));
    std::fs::create_dir_all(&dir).unwrap();
    skill_dir(&dir, "delta", "the delta skill");
    let s = load_skill(&dir.join("delta").join("SKILL.md")).unwrap();
    let mut reg = ToolRegistry::new();
    register_skill_tools(&mut reg, vec![s]);
    let listed = reg.execute("skills_list", "{}").unwrap();
    assert!(listed.contains("delta"), "{listed}");
    assert!(listed.contains("the delta skill"), "{listed}");
    let body = reg.execute("skill_read", r#"{"name":"delta"}"#).unwrap();
    assert!(body.starts_with("# delta"), "{body}");
    let err = reg.execute("skill_read", r#"{"name":"nope"}"#).unwrap_err();
    assert_eq!(err.code, "SKILL_UNKNOWN");
    let _ = std::fs::remove_dir_all(&dir);
}
