//! Tests for `pantheon_exec::skills::tests` — sibling file so sources stay test-free.
use super::*;

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
fn parse_rejects_missing_frontmatter() {
    let dir = std::env::temp_dir().join(format!("sk-{}-{}", std::process::id(), 1));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("SKILL.md");
    std::fs::write(&p, "# no frontmatter").unwrap();
    let err = parse_skill("# no frontmatter", &p).unwrap_err();
    assert_eq!(err.code, "SKILL_NO_FRONTMATTER");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn discover_finds_both_scopes_and_dedups() {
    let dir = std::env::temp_dir().join(format!("sk-{}-{}", std::process::id(), 2));
    let data = dir.join("data");
    let proj = dir.join("proj");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&proj).unwrap();
    skill_dir(&data.join("skills"), "alpha", "first");
    skill_dir(&proj.join(".pantheon").join("skills"), "beta", "second");
    // Fake HOME so the developer machine's ~/.hermes etc. stay out.
    let home = dir.join(".fake-home");
    let found = scan_skills_with_home(&data, &proj, &[], &home).loaded;
    let names: Vec<&str> = found.iter().map(|s| s.meta.name.as_str()).collect();
    assert_eq!(names, vec!["alpha", "beta"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn body_strips_frontmatter() {
    let dir = std::env::temp_dir().join(format!("sk-{}-{}", std::process::id(), 3));
    std::fs::create_dir_all(&dir).unwrap();
    let p = skill_dir(&dir, "gamma", "d");
    let s = load_skill(&p).unwrap();
    let body = skill_body(&s).unwrap();
    assert!(body.starts_with("# gamma"), "{body}");
    assert!(!body.contains("description"), "{body}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn registry_tools_list_and_read() {
    let dir = std::env::temp_dir().join(format!("sk-{}-{}", std::process::id(), 4));
    std::fs::create_dir_all(&dir).unwrap();
    skill_dir(&dir, "delta", "the delta skill");
    let s = load_skill(&dir.join("delta").join("SKILL.md")).unwrap();
    let mut reg = crate::tools::ToolRegistry::new();
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

#[test]
fn ext_discovers_codex_omp_and_claude_user_roots() {
    let dir = std::env::temp_dir().join(format!("sk-{}-{}", std::process::id(), 10));
    let data = dir.join("data");
    let proj = dir.join("proj");
    let home = dir.join("home");
    for d in [&data, &proj, &home] {
        std::fs::create_dir_all(d).unwrap();
    }
    skill_dir(
        &home.join(".codex").join("skills").join(".system"),
        "codex-skill",
        "c",
    );
    skill_dir(
        &home.join(".omp").join("agent").join("skills"),
        "omp-skill",
        "o",
    );
    skill_dir(
        &home.join(".claude").join("skills"),
        "claude-user-skill",
        "u",
    );
    let found = discover_skills_ext_with_home(&data, &proj, &[], &home);
    let mut names: Vec<&str> = found.iter().map(|s| s.meta.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["claude-user-skill", "codex-skill", "omp-skill"]);
    let origin = |n: &str| {
        found
            .iter()
            .find(|s| s.meta.name == n)
            .unwrap()
            .meta
            .origin
            .clone()
    };
    assert_eq!(origin("codex-skill"), "codex");
    assert_eq!(origin("omp-skill"), "omp");
    assert_eq!(origin("claude-user-skill"), "claude-user");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ext_collects_explicit_codedot_dir_extra_root() {
    // Codex-style layout: a dot-dir explicitly passed as an extra root.
    let dir = std::env::temp_dir().join(format!("sk-{}-{}", std::process::id(), 11));
    let data = dir.join("data");
    let proj = dir.join("proj");
    let home = dir.join("home");
    for d in [&data, &proj, &home] {
        std::fs::create_dir_all(d).unwrap();
    }
    let dot_root = dir.join("fake-codex").join("skills").join(".system");
    skill_dir(&dot_root, "dot-skill", "d");
    let found = discover_skills_ext_with_home(&data, &proj, &[dot_root.clone()], &home);
    assert!(
        found.iter().any(|s| s.meta.name == "dot-skill"),
        "{found:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn collect_skills_does_not_skip_explicit_system_hidden_dir() {
    let dir = std::env::temp_dir().join(format!("sk-{}-{}", std::process::id(), 12));
    let sys = dir.join(".system");
    skill_dir(&sys, "sys-skill", "s");
    let mut out = Vec::new();
    collect_skills_rejecting(&sys, SkillSource::Codex, &mut out, &mut Vec::new());
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].meta.name, "sys-skill");
    assert_eq!(out[0].meta.origin, "codex");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scan_reports_a_broken_skill_that_discovery_would_drop() {
    // The bug this guards: a malformed SKILL.md vanished during discovery,
    // so `skills doctor` reported a healthy tree while the user had lost
    // the skill with no error anywhere.
    let dir = std::env::temp_dir().join(format!("sk-{}-{}", std::process::id(), 13));
    let root = dir.join("skills");
    skill_dir(&root, "good", "g");
    let bad = root.join("broken");
    std::fs::create_dir_all(&bad).unwrap();
    std::fs::write(
        bad.join("SKILL.md"),
        "---\ndescription: no name field\n---\nbody\n",
    )
    .unwrap();

    // Fake HOME so the developer machine's ~/.hermes etc. stay out.
    let home = dir.join(".fake-home");
    let loaded = scan_skills_with_home(&dir, &dir, &[], &home).loaded;
    assert!(
        loaded.iter().all(|s| s.meta.name != "broken"),
        "a skill with no name must not load"
    );

    let scan = scan_skills_with_home(&dir, &dir, &[], &home);
    assert_eq!(scan.loaded.len(), 1);
    assert_eq!(
        scan.rejected.len(),
        1,
        "the drop must be reported, not silent"
    );
    assert!(scan.rejected[0].path.ends_with("SKILL.md"));
    assert!(
        !scan.rejected[0].reason.is_empty(),
        "a rejection must carry a reason the user can act on"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scan_reports_a_duplicate_name_and_keeps_the_winner() {
    let dir = std::env::temp_dir().join(format!("sk-{}-{}", std::process::id(), 14));
    skill_dir(&dir.join("skills"), "shared", "first");
    skill_dir(&dir.join(".pantheon").join("skills"), "shared", "second");

    let home = dir.join(".fake-home");
    let scan = scan_skills_with_home(&dir, &dir, &[], &home);
    assert_eq!(scan.loaded.len(), 1, "first root wins the name");
    assert_eq!(scan.rejected.len(), 1);
    assert!(scan.rejected[0].reason.contains("duplicate"));
    let _ = std::fs::remove_dir_all(&dir);
}
