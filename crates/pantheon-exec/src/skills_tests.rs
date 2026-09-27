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
    let found = discover_skills_ext_with_home(&data, &proj, std::slice::from_ref(&dot_root), &home);
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

// ---- path traversal / size cap regression tests ----

#[test]
fn valid_slug_accepts_safe_names() {
    assert!(valid_slug("alpha"));
    assert!(valid_slug("my-skill_2"));
    assert!(valid_slug("A"));
    assert!(valid_slug(&"x".repeat(64)));
}

#[test]
fn valid_slug_rejects_traversal_and_junk() {
    for bad in [
        "",
        ".",
        "..",
        "../evil",
        "..\\evil",
        "a/b",
        "/abs",
        "evil!",
        "has space",
        "ünïcödé",
        "a..b/c",
    ] {
        assert!(!valid_slug(bad), "{bad:?} must be rejected");
    }
    assert!(!valid_slug(&"x".repeat(65)), "over 64 chars must be rejected");
}

#[test]
fn import_skill_dir_rejects_traversal_name() {
    let dir = std::env::temp_dir().join(format!("skimp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("SKILL.md"), "---\nname: x\n---\nbody\n").unwrap();
    let data = dir.join("data");
    for bad in ["../evil", "..", "/abs", "a/b", ""] {
        let err = import_skill_dir(&data, &src, bad).unwrap_err();
        assert_eq!(err.code, "SKILL_BAD_NAME", "{bad:?}");
    }
    // Nothing escaped the data dir: the rejection happens before any write.
    assert!(!dir.join("evil").exists());
    assert!(!data.join("skills").join("evil").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn import_skill_rejects_bad_meta_name() {
    let dir = std::env::temp_dir().join(format!("skimp2-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let skill = Skill {
        meta: SkillMeta {
            name: "../../evil".into(),
            description: String::new(),
            origin: String::new(),
        },
        path: dir.join("x"),
    };
    let err = import_skill(&dir.join("data"), &skill).unwrap_err();
    assert_eq!(err.code, "SKILL_BAD_NAME");
    assert!(!dir.join("evil").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn repo_subpath_rejects_traversal() {
    let dir = std::env::temp_dir().join(format!("sksub-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    assert!(resolve_repo_subpath(&dir, Some("sub")).is_ok());
    assert_eq!(resolve_repo_subpath(&dir, None).unwrap(), dir);
    for bad in ["../evil", "../../evil", "/abs", "sub/../../evil"] {
        let err = resolve_repo_subpath(&dir, Some(bad)).unwrap_err();
        assert_eq!(err.code, "SKILL_BAD_SUBPATH", "{bad:?}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hermes_repo_path_rejects_dotdot() {
    assert!(check_repo_path("skills/creative/claude-design").is_ok());
    // Leading/trailing slashes are normalized (pre-existing behavior), so
    // "/abs/" is fine; `..` components anywhere are rejected.
    assert_eq!(check_repo_path("/abs/").unwrap(), "abs");
    for bad in ["../../etc", "..", "skills/../../x", ""] {
        assert!(check_repo_path(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn read_capped_rejects_oversize_stream() {
    let err = read_capped(std::io::Cursor::new(vec![7u8; 16]), 8, "test").unwrap_err();
    assert_eq!(err.code, "SKILL_FETCH_TOO_LARGE");
    let out = read_capped(std::io::Cursor::new(vec![7u8; 8]), 8, "test").unwrap();
    assert_eq!(out.len(), 8);
}

#[cfg(unix)]
#[test]
fn contained_skill_dir_rejects_symlink_escape() {
    let dir = std::env::temp_dir().join(format!("sklink-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let root = dir.join("skills");
    std::fs::create_dir_all(&root).unwrap();
    let outside = dir.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
    let err = contained_skill_dir(&root, "link").unwrap_err();
    assert_eq!(err.code, "SKILL_BAD_NAME");
    let _ = std::fs::remove_dir_all(&dir);
}

fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write;
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, data) in entries {
        w.start_file(*name, opts).unwrap();
        w.write_all(data).unwrap();
    }
    w.finish().unwrap().into_inner()
}

#[test]
fn unzip_rejects_dotdot_and_absolute_entries() {
    for evil in ["../evil.txt", "sub/../../evil.txt", "/abs.txt"] {
        let bytes = make_zip(&[(evil, b"evil")]);
        let err = unzip_skill(&bytes).unwrap_err();
        assert!(
            err.contains("unsafe entry path"),
            "{evil:?}: got {err}"
        );
    }
    // A benign nested entry still inflates fine.
    let bytes = make_zip(&[("refs/guide.md", b"hello")]);
    let out = unzip_skill(&bytes).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].0, PathBuf::from("refs/guide.md"));
}

#[test]
fn unzip_rejects_too_many_entries() {
    let entries: Vec<(String, Vec<u8>)> =
        (0..5).map(|i| (format!("f{i}.txt"), b"x".to_vec())).collect();
    let refs: Vec<(&str, &[u8])> = entries
        .iter()
        .map(|(n, d)| (n.as_str(), d.as_slice()))
        .collect();
    let bytes = make_zip(&refs);
    let limits = ZipLimits {
        max_entries: 3,
        max_total_bytes: 1024 * 1024,
    };
    let err = unzip_skill_with_limits(&bytes, &limits).unwrap_err();
    assert!(err.contains("entries"), "{err}");
}

#[test]
fn unzip_rejects_inflated_bomb() {
    // 4 KiB of zeros compresses to almost nothing but inflates past the cap.
    let data = vec![0u8; 4096];
    let bytes = make_zip(&[("bomb.bin", &data[..])]);
    let limits = ZipLimits {
        max_entries: 1000,
        max_total_bytes: 100,
    };
    let err = unzip_skill_with_limits(&bytes, &limits).unwrap_err();
    assert!(err.contains("inflates past"), "{err}");
}
