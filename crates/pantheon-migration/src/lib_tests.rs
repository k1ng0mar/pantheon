//! Tests for detect / analyze / plan / provenance — the read-only half.
use super::*;
use std::fs;
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "pantheon-migration-{}-{}-{}",
        name,
        std::process::id(),
        now_ms()
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn targets(d: &Path) -> Targets {
    Targets::new(d.join("data"), d.join("data").join("extensions"))
}

// ---------------------------------------------------------------------------
// detect
// ---------------------------------------------------------------------------

#[test]
fn detects_hermes_layout() {
    let d = tmp("hermes");
    fs::write(d.join("config.yaml"), "x: 1\n").unwrap();
    fs::create_dir_all(d.join("plugins/p1")).unwrap();
    assert!(detect(&d).contains(&SourceKind::Hermes));
}

#[test]
fn detects_openclaw_plugins() {
    let d = tmp("openclaw");
    fs::create_dir_all(d.join("plugins/soul")).unwrap();
    fs::write(d.join("plugins/soul/openclaw.plugin.json"), "{}").unwrap();
    assert!(detect(&d).contains(&SourceKind::OpenClaw));
}

#[test]
fn detects_omp_agent_dir() {
    let d = tmp("omp-detect");
    fs::create_dir_all(d.join("agent")).unwrap();
    fs::write(d.join("agent/config.yml"), "setupVersion: 2\n").unwrap();
    assert!(detect(&d).contains(&SourceKind::Omp));
}

#[test]
fn detects_omp_via_install_id_alone() {
    let d = tmp("omp-install-id");
    fs::create_dir_all(d.join("agent")).unwrap();
    fs::write(d.join("install-id"), "uuid").unwrap();
    assert!(detect(&d).contains(&SourceKind::Omp));
}

#[test]
fn hermes_root_is_not_mistaken_for_omp_or_openclaw() {
    let d = tmp("hermes-not-omp");
    fs::write(d.join("config.yaml"), "x: 1\n").unwrap();
    fs::create_dir_all(d.join("plugins/p1")).unwrap();
    fs::create_dir_all(d.join("skills/s1")).unwrap();
    let found = detect(&d);
    assert!(found.contains(&SourceKind::Hermes));
    assert!(
        !found.contains(&SourceKind::Omp),
        "agent/ is the omp anchor"
    );
    assert!(
        !found.contains(&SourceKind::OpenClaw),
        "needs openclaw.plugin.json"
    );
}

#[test]
fn source_kind_parses_aliases() {
    assert_eq!(SourceKind::parse("hermes"), Some(SourceKind::Hermes));
    assert_eq!(SourceKind::parse("OpenClaw"), Some(SourceKind::OpenClaw));
    assert_eq!(SourceKind::parse("omp"), Some(SourceKind::Omp));
    assert_eq!(SourceKind::parse("oh-my-pi"), Some(SourceKind::Omp));
    assert_eq!(SourceKind::parse("pi"), Some(SourceKind::Omp));
    assert_eq!(SourceKind::parse("nope"), None);
}

// ---------------------------------------------------------------------------
// secrets
// ---------------------------------------------------------------------------

#[test]
fn secret_paths_are_never_importable() {
    for n in [".env", "auth.json", "broker.token", "google_token.json"] {
        assert!(is_secret_path(Path::new(n)), "{n} must be a secret");
    }
    assert!(is_secret_path(Path::new("x/agent.db")));
    assert!(is_secret_path(Path::new("x/id_ed25519")));
    assert!(is_secret_path(Path::new("x/key.pem")));
    assert!(!is_secret_path(Path::new("SKILL.md")));
    assert!(!is_secret_path(Path::new("plugin.yaml")));
}

#[test]
fn config_files_that_may_hold_keys_are_flagged() {
    assert!(config_may_hold_keys(Path::new("models.yml")));
    assert!(config_may_hold_keys(Path::new("config.yaml")));
    assert!(!config_may_hold_keys(Path::new("SKILL.md")));
}

#[test]
fn every_bridge_plan_target_is_the_path_the_writer_produces() {
    // The plan target and the bridge writer must agree, or `validate` checks a
    // path nothing ever creates. This bit twice: the credentials target was
    // derived from the `.env` filename, and the mcp target from `config.yaml`.
    let d = tmp("bridge-targets");
    fs::write(
        d.join("config.yaml"),
        "mcp_servers:\n  a:\n    command: /bin/x\n",
    )
    .unwrap();
    fs::write(d.join(".env"), "OPENAI_API_KEY=sk-value12345678\n").unwrap();
    fs::create_dir_all(d.join("sessions")).unwrap();
    fs::write(d.join("sessions/t.jsonl"), "{}\n").unwrap();

    let t = targets(&d);
    let p = plan(&d, SourceKind::Hermes, &t);
    let dd = t.data_dir.to_string_lossy().to_string();

    let mcp = p.items.iter().find(|i| i.kind == ItemKind::Mcp).unwrap();
    assert_eq!(
        mcp.target().unwrap(),
        format!("{dd}/mcp/hermes.json"),
        "mcp plan target must match write_mcp_declaration"
    );
    let sess = p
        .items
        .iter()
        .find(|i| i.kind == ItemKind::Session)
        .unwrap();
    assert_eq!(
        sess.target().unwrap(),
        format!("{dd}/imported-sessions/hermes/manifest.json"),
        "session plan target must match write_session_import"
    );
    let cred = p
        .items
        .iter()
        .find(|i| i.kind == ItemKind::Credentials)
        .unwrap();
    assert_eq!(cred.target().unwrap(), format!("{dd}/.env"));

    // And the assertion that actually matters: after a real apply, nothing the
    // plan named is missing.
    let m = backup(&p, &t).unwrap();
    apply(&p, &t, &m).unwrap();
    let v = validate(&p);
    assert!(v.complete, "{}", v.render());
}

#[test]
fn every_bridge_plan_target_is_produced_by_its_writer() {
    // Regression guard. Three bridges have each shipped a plan target that
    // disagreed with what the writer actually created, which `validate` caught
    // only at run time. This asserts the agreement directly, for every kind
    // the copier does not handle.
    let d = tmp("bridge-agreement");
    fs::write(
        d.join("config.yaml"),
        "mcp_servers:\n  a:\n    command: /bin/x\nproviders:\n  p1:\n    name: P1\n    api: https://x.example/v1\n    api_key: ${SOME_KEY}\n",
    )
    .unwrap();
    fs::write(d.join(".env"), "SOME_KEY=sk-value12345678\n").unwrap();
    fs::create_dir_all(d.join("sessions")).unwrap();
    fs::write(d.join("sessions/t.jsonl"), "{}\n").unwrap();

    let t = targets(&d);
    let p = plan(&d, SourceKind::Hermes, &t);
    let m = backup(&p, &t).unwrap();
    apply(&p, &t, &m).unwrap();

    // Nothing the plan named may be missing after a real apply.
    let v = validate(&p);
    assert!(v.complete, "{}", v.render());

    // And the two file-shaped bridges must match their writers by name.
    for kind in [ItemKind::Mcp, ItemKind::Provider] {
        let it = p.items.iter().find(|i| i.kind == kind).unwrap();
        let target = it.target().unwrap();
        assert!(
            std::path::Path::new(target).exists(),
            "{kind} plan target {target} was never written"
        );
    }
}

#[test]
fn a_credential_file_is_never_an_import_target() {
    let d = tmp("secret-plan");
    fs::write(d.join("config.yaml"), "x: 1\n").unwrap();
    fs::write(d.join(".env"), "OPENAI_API_KEY=zzz\n").unwrap();
    fs::write(d.join("auth.json"), "{}\n").unwrap();
    fs::create_dir_all(d.join("skills/s1")).unwrap();
    fs::write(d.join("skills/s1/SKILL.md"), "---\nname: s1\n---\n").unwrap();

    let t = targets(&d);
    let p = plan(&d, SourceKind::Hermes, &t);
    let src_env = d.join(".env").to_string_lossy().to_string();

    for item in &p.items {
        // A real `Secret` is never imported and never archived.
        if item.kind == ItemKind::Secret {
            assert!(
                matches!(item.action, Action::Skip { .. }),
                "a credential file must be a skip, got {:?}",
                item.action
            );
        }
        // No import may point at a *source* credential file. The one allowed
        // `.env` target is pantheon's own key store under the data dir.
        if let Some(target) = item.target() {
            assert!(
                !target.contains("auth.json"),
                "credential store became a target: {target}"
            );
            if target.ends_with(".env") {
                assert_eq!(
                    target,
                    t.data_dir.join(".env").to_string_lossy(),
                    "a .env import must land in pantheon's own key store"
                );
                assert_ne!(target, src_env, "the source .env must not be a target");
            }
        }
    }
    // auth.json is a skip; the .env is a carried credential.
    assert!(p.skipped() >= 1, "auth.json must be skipped");
    assert_eq!(
        p.imports_of(ItemKind::Credentials).len(),
        1,
        ".env should import as a credential carry"
    );
}

#[test]
fn omp_models_yml_is_skipped_not_archived() {
    let d = tmp("omp-models");
    fs::create_dir_all(d.join("agent")).unwrap();
    fs::write(d.join("agent/config.yml"), "setupVersion: 2\n").unwrap();
    fs::write(
        d.join("agent/models.yml"),
        "providers:\n  x:\n    apiKey: zz\n",
    )
    .unwrap();
    let t = targets(&d);
    let p = plan(&d, SourceKind::Omp, &t);
    let m = p
        .items
        .iter()
        .find(|i| i.path.ends_with("models.yml"))
        .expect("models.yml must be detected");
    assert_eq!(m.kind, ItemKind::Secret);
    assert!(matches!(m.action, Action::Skip { .. }));
}

// ---------------------------------------------------------------------------
// analyze
// ---------------------------------------------------------------------------

#[test]
fn plan_imports_python_plugins_and_archives_missing_entries() {
    let d = tmp("plan");
    let good = d.join("plugins/good");
    fs::create_dir_all(&good).unwrap();
    fs::write(good.join("plugin.yaml"), "name: good\n").unwrap();
    fs::write(good.join("__init__.py"), "def register(ctx): pass\n").unwrap();
    let bad = d.join("plugins/bad");
    fs::create_dir_all(&bad).unwrap();
    fs::write(bad.join("plugin.yaml"), "name: bad\n").unwrap();
    let t = targets(&d);
    let p = plan(&d, SourceKind::Hermes, &t);
    assert_eq!(p.imports_of(ItemKind::Extension).len(), 1);
    assert_eq!(p.archived(), 1);
    assert!(render(&p).contains("archive:"));
}

#[test]
fn typescript_plugin_is_archived_not_half_imported() {
    let d = tmp("ts-plugin");
    let p1 = d.join("plugins/ts-only");
    fs::create_dir_all(&p1).unwrap();
    fs::write(p1.join("index.ts"), "export default {}\n").unwrap();
    let t = targets(&d);
    let p = plan(&d, SourceKind::Hermes, &t);
    let it = p
        .items
        .iter()
        .find(|i| i.path.ends_with("ts-only"))
        .unwrap();
    assert!(matches!(it.action, Action::Archive { .. }));
    assert!(p.imports_of(ItemKind::Extension).is_empty());
}

#[test]
fn omp_skills_agents_rules_and_commands_are_imported() {
    let d = tmp("omp-items");
    let a = d.join("agent");
    fs::create_dir_all(a.join("skills/demo")).unwrap();
    fs::write(a.join("skills/demo/SKILL.md"), "---\nname: demo\n---\n").unwrap();
    fs::create_dir_all(a.join("agents")).unwrap();
    fs::write(a.join("agents/scout.md"), "---\nname: scout\n---\n").unwrap();
    fs::create_dir_all(a.join("rules")).unwrap();
    fs::write(a.join("rules/style.md"), "be terse\n").unwrap();
    fs::write(a.join("rules/legacy.mdc"), "legacy\n").unwrap();
    fs::create_dir_all(a.join("commands")).unwrap();
    fs::write(a.join("commands/reindex.md"), "reindex\n").unwrap();
    fs::create_dir_all(a.join("prompts")).unwrap();
    fs::write(a.join("prompts/summary.md"), "summarise\n").unwrap();

    let t = targets(&d);
    let p = plan(&d, SourceKind::Omp, &t);
    assert_eq!(p.imports_of(ItemKind::Skill).len(), 1);
    assert_eq!(p.imports_of(ItemKind::Agent).len(), 1);
    assert_eq!(
        p.imports_of(ItemKind::Rule).len(),
        2,
        ".md and .mdc both count"
    );
    assert_eq!(p.imports_of(ItemKind::Command).len(), 1);
    assert_eq!(p.imports_of(ItemKind::Prompt).len(), 1);
}

#[test]
fn omp_nested_category_skills_are_found() {
    let d = tmp("omp-nested");
    let a = d.join("agent");
    fs::create_dir_all(a.join("skills/productivity/xlsx")).unwrap();
    fs::write(
        a.join("skills/productivity/xlsx/SKILL.md"),
        "---\nname: xlsx\n---\n",
    )
    .unwrap();
    let t = targets(&d);
    let p = plan(&d, SourceKind::Omp, &t);
    assert_eq!(p.imports_of(ItemKind::Skill).len(), 1);
}

#[test]
fn omp_extension_needs_omp_or_pi_field() {
    let d = tmp("omp-ext");
    let a = d.join("agent/extensions");
    fs::create_dir_all(a.join("good")).unwrap();
    fs::write(a.join("good/package.json"), r#"{"name":"g","omp":{}}"#).unwrap();
    fs::create_dir_all(a.join("bad")).unwrap();
    fs::write(a.join("bad/package.json"), r#"{"name":"b"}"#).unwrap();
    let t = targets(&d);
    let p = plan(&d, SourceKind::Omp, &t);
    let names: Vec<String> = p
        .imports_of(ItemKind::Extension)
        .iter()
        .map(|i| i.path.clone())
        .collect();
    assert_eq!(names.len(), 1, "only the omp-declared package imports");
    assert!(names[0].ends_with("good"));
}

#[test]
fn hermes_persona_and_memory_route_to_the_memory_plane() {
    let d = tmp("hermes-persona");
    fs::write(d.join("config.yaml"), "x: 1\n").unwrap();
    fs::write(d.join("SOUL.md"), "You are Nyx.\n").unwrap();
    fs::write(d.join("profile.yaml"), "display_name: Nyx\n").unwrap();
    fs::write(d.join("MEMORY.md"), "identity\n").unwrap();
    let t = targets(&d);
    let p = plan(&d, SourceKind::Hermes, &t);
    for kind in [ItemKind::Persona, ItemKind::Memory] {
        let items = p.imports_of(kind);
        assert!(!items.is_empty(), "{kind} should be detected");
        for i in items {
            assert!(
                i.target().unwrap().starts_with("memory://"),
                "{kind} must target the memory plane, got {}",
                i.target().unwrap()
            );
        }
    }
}

#[test]
fn mcp_servers_bridge_to_a_declaration_and_carry_no_token() {
    let d = tmp("hermes-mcp");
    fs::write(
        d.join("config.yaml"),
        "mcp_servers:\n  a:\n    command: x\n  b:\n    command: y\n  c:\n    command: z\n",
    )
    .unwrap();
    let t = targets(&d);
    let p = plan(&d, SourceKind::Hermes, &t);
    let mcp = p.items.iter().find(|i| i.kind == ItemKind::Mcp).unwrap();
    // MCP is now a real import, not an archive.
    assert!(mcp.target().is_some(), "mcp should import a declaration");
    let note = match &mcp.action {
        Action::Import { .. } => &mcp.kind,
        other => panic!("expected import, got {other:?}"),
    };
    assert_eq!(*note, ItemKind::Mcp);
}

#[test]
fn credential_names_import_but_the_env_file_itself_does_not() {
    let d = tmp("hermes-creds");
    fs::write(d.join("config.yaml"), "x: 1\n").unwrap();
    fs::write(
        d.join(".env"),
        "OPENAI_API_KEY=sk-realsecretvalue12345\nTELEGRAM_BOT_TOKEN=999:abc\nHOME=/root\n",
    )
    .unwrap();
    let t = targets(&d);
    let p = plan(&d, SourceKind::Hermes, &t);

    // One importable credential item.
    let cred = p
        .items
        .iter()
        .find(|i| i.kind == ItemKind::Credentials && i.target().is_some())
        .expect("credential names must be an importable item");
    assert!(cred.note.contains("2 credential name(s)"), "{}", cred.note);
    assert!(cred.note.contains("1 provider"), "{}", cred.note);
    assert!(cred.note.contains("1 channel"), "{}", cred.note);

    // The target is pantheon's own key store, and no value appears in the
    // plan itself — the plan describes the carry, it does not perform it.
    let target = cred.target().unwrap();
    assert_eq!(target, t.data_dir.join(".env").to_string_lossy());
    for i in &p.items {
        if let Some(t) = i.target() {
            assert_ne!(t, &d.join(".env").to_string_lossy().to_string());
        }
    }
    let json = plan_json(&p);
    assert!(
        !json.contains("sk-realsecretvalue12345"),
        "secret leaked into plan"
    );
    assert!(!json.contains("999:abc"), "token leaked into plan");
}

#[test]
fn sessions_import_to_a_quarantine_target() {
    let d = tmp("hermes-sessions");
    fs::write(d.join("config.yaml"), "x: 1\n").unwrap();
    fs::create_dir_all(d.join("sessions")).unwrap();
    fs::write(d.join("sessions/a.jsonl"), "{\"role\":\"user\"}\n").unwrap();
    fs::write(d.join("sessions/b.jsonl"), "{\"role\":\"assistant\"}\n").unwrap();
    fs::write(d.join("sessions/notes.txt"), "ignore me\n").unwrap();
    let t = targets(&d);
    let p = plan(&d, SourceKind::Hermes, &t);
    let sess = p
        .items
        .iter()
        .find(|i| i.kind == ItemKind::Session)
        .expect("transcripts must be covered");
    assert!(sess.target().is_some(), "{:?}", sess.action);
    assert!(sess.note.contains("2 transcript file"), "{}", sess.note);
    assert!(
        sess.target()
            .unwrap()
            .ends_with("imported-sessions/hermes/manifest.json"),
        "{}",
        sess.target().unwrap()
    );
}

// ---------------------------------------------------------------------------
// plan targets
// ---------------------------------------------------------------------------

#[test]
fn targets_respect_ext_dir_override() {
    let t = Targets::new(PathBuf::from("/d"), PathBuf::from("/e"));
    assert_eq!(t.dir_for(ItemKind::Skill), Some(PathBuf::from("/d/skills")));
    assert_eq!(
        t.dir_for(ItemKind::Extension),
        Some(PathBuf::from("/e")),
        "extensions honour PANTHEON_EXT_DIR"
    );
    assert_eq!(t.dir_for(ItemKind::Secret), None);
    assert_eq!(t.dir_for(ItemKind::Memory), None);
}

#[test]
fn backup_root_lives_under_the_data_dir() {
    let t = Targets::new(PathBuf::from("/d"), PathBuf::from("/d/extensions"));
    assert_eq!(t.backup_root(), PathBuf::from("/d/migrate-backups"));
}

#[test]
fn item_name_uses_dir_name_or_file_stem() {
    let d = Detected {
        kind: ItemKind::Skill,
        path: "/src/skills/demo".into(),
        mappable: true,
        note: String::new(),
    };
    assert_eq!(item_name(&d), "demo");
    let f = Detected {
        kind: ItemKind::Rule,
        path: "/src/rules/style.md".into(),
        mappable: true,
        note: String::new(),
    };
    assert_eq!(item_name(&f), "style");
}

#[test]
fn omp_source_version_reads_changelog_cursor() {
    let d = tmp("omp-version");
    fs::create_dir_all(d.join("agent")).unwrap();
    fs::write(d.join("agent/last-changelog-version"), "18.2.4\n").unwrap();
    assert_eq!(
        source_version(&d, SourceKind::Omp),
        Some("18.2.4".to_string())
    );
}

#[test]
fn hermes_source_version_reads_install_stamp() {
    let d = tmp("hermes-version");
    fs::create_dir_all(d.join("hermes-agent")).unwrap();
    fs::write(
        d.join("hermes-agent/install-stamp.json"),
        r#"{"displayVersion":"0.21.5+2144.g7b761da"}"#,
    )
    .unwrap();
    assert_eq!(
        source_version(&d, SourceKind::Hermes),
        Some("0.21.5+2144.g7b761da".to_string())
    );
}

// ---------------------------------------------------------------------------
// provenance
// ---------------------------------------------------------------------------

#[test]
fn provenance_records_source_version_and_timestamp() {
    let d = tmp("prov");
    let pr = provenance(&d, SourceKind::Hermes);
    assert_eq!(pr.source, "hermes");
    assert!(pr.imported_at_ms > 0);
}

#[test]
fn plan_json_is_serialisable() {
    let d = tmp("json");
    fs::create_dir_all(d.join("skills/s")).unwrap();
    fs::write(d.join("skills/s/SKILL.md"), "---\nname: s\n---\n").unwrap();
    let t = targets(&d);
    let p = plan(&d, SourceKind::Hermes, &t);
    let j = plan_json(&p);
    assert!(j.contains("\"source\": \"hermes\""));
    assert!(j.contains("\"items\""));
}

#[test]
fn a_skill_with_broken_frontmatter_is_archived_not_imported() {
    // `modal-model-deployment` in the real Hermes install is exactly this:
    // a SKILL.md whose frontmatter has no `name`.
    let d = tmp("bad-skill");
    let bad = d.join("skills/modal");
    fs::create_dir_all(&bad).unwrap();
    fs::write(
        bad.join("SKILL.md"),
        "---\ndescription: no name field\n---\nbody\n",
    )
    .unwrap();
    let t = targets(&d);
    let p = plan(&d, SourceKind::Hermes, &t);
    let it = p.items.iter().find(|i| i.path.ends_with("/modal")).unwrap();
    assert!(
        p.imports_of(ItemKind::Skill).is_empty(),
        "a skill the runtime rejects must not import"
    );
    match &it.action {
        Action::Archive { reason } => assert!(reason.contains("no name"), "{reason}"),
        other => panic!("expected archive, got {other:?}"),
    }
}

#[test]
fn skill_validation_matches_the_runtime_rules() {
    let d = tmp("skill-rules");
    let cases: Vec<(&str, &str, bool)> = vec![
        ("ok", "---\nname: ok\n---\n", true),
        ("nofm", "just a body\n", false),
        ("unclosed", "---\nname: x\n", false),
        ("noname", "---\ndescription: d\n---\n", false),
        ("emptyname", "---\nname: \"  \"\n---\n", false),
        ("badyaml", "---\nname: [unclosed\n---\n", false),
    ];
    for (n, body, want_ok) in cases {
        let p = d.join(format!("{n}.md"));
        fs::write(&p, body).unwrap();
        assert_eq!(validate_skill_md(&p).is_ok(), want_ok, "case {n}: {body:?}");
    }
    assert!(!validate_skill_md(&d.join("missing.md")).is_ok());
}

#[test]
fn memory_plane_targets_are_unique_per_file() {
    let d = tmp("mem-unique");
    fs::write(d.join("config.yaml"), "x: 1\n").unwrap();
    fs::write(d.join("SOUL.md"), "persona\n").unwrap();
    fs::write(d.join("profile.yaml"), "display_name: Nyx\n").unwrap();
    let t = targets(&d);
    let p = plan(&d, SourceKind::Hermes, &t);
    let mut seen = std::collections::HashSet::new();
    for i in p.imports_of(ItemKind::Persona) {
        let target = i.target().unwrap();
        assert!(target.starts_with("memory://persona/"), "{target}");
        assert!(seen.insert(target.to_string()), "duplicate record {target}");
    }
    assert_eq!(seen.len(), 2, "SOUL.md and profile.yaml are two records");
}

#[test]
fn duplicate_skill_names_collapse_to_one_import_with_a_reason() {
    // Hermes mirrors its marketing family into a `marketingskills/` subdir.
    let d = tmp("dupe-skills");
    fs::create_dir_all(d.join("skills/ads")).unwrap();
    fs::write(d.join("skills/ads/SKILL.md"), "---\nname: ads\n---\nroot\n").unwrap();
    fs::create_dir_all(d.join("skills/marketingskills/ads")).unwrap();
    fs::write(
        d.join("skills/marketingskills/ads/SKILL.md"),
        "---\nname: ads\n---\nmirror\n",
    )
    .unwrap();

    let t = targets(&d);
    let p = plan(&d, SourceKind::Hermes, &t);

    let imports: Vec<&PlanItem> = p.imports_of(ItemKind::Skill);
    assert_eq!(imports.len(), 1, "one name, one import target");
    assert!(
        imports[0].path.ends_with("skills/ads"),
        "the shallower path wins: {}",
        imports[0].path
    );

    // The loser is archived with a reason, not dropped and not overwritten.
    let loser = p
        .items
        .iter()
        .find(|i| i.path.ends_with("marketingskills/ads"))
        .unwrap();
    match &loser.action {
        Action::Archive { reason } => assert!(reason.contains("collides"), "{reason}"),
        other => panic!("expected archive, got {other:?}"),
    }
    assert_eq!(
        p.items.len(),
        analyze(&d, SourceKind::Hermes).len(),
        "the collision is resolved, not hidden"
    );
}

#[test]
fn plan_targets_are_unique() {
    let d = tmp("uniq");
    for n in ["a", "b", "c"] {
        fs::create_dir_all(d.join(format!("skills/{n}"))).unwrap();
        fs::write(
            d.join(format!("skills/{n}/SKILL.md")),
            format!("---\nname: {n}\n---\n"),
        )
        .unwrap();
        fs::create_dir_all(d.join(format!("skills/mirror/{n}"))).unwrap();
        fs::write(
            d.join(format!("skills/mirror/{n}/SKILL.md")),
            format!("---\nname: {n}\n---\n"),
        )
        .unwrap();
    }
    let t = targets(&d);
    let p = plan(&d, SourceKind::Hermes, &t);
    let mut seen = std::collections::HashSet::new();
    for i in &p.items {
        if let Some(target) = i.target() {
            assert!(seen.insert(target.to_string()), "duplicate target {target}");
        }
    }
}

#[test]
fn every_detected_item_appears_in_the_plan() {
    // The "never silently dropped" invariant, asserted directly.
    let d = tmp("no-drop");
    fs::write(d.join("config.yaml"), "x: 1\n").unwrap();
    fs::create_dir_all(d.join("skills/keep")).unwrap();
    fs::write(d.join("skills/keep/SKILL.md"), "---\nname: keep\n---\n").unwrap();
    fs::create_dir_all(d.join("skills/broken")).unwrap();
    fs::write(d.join("skills/broken/README.md"), "no skill md\n").unwrap();
    fs::write(d.join(".env"), "K=v\n").unwrap();

    let t = targets(&d);
    let detected = analyze(&d, SourceKind::Hermes);
    let p = plan(&d, SourceKind::Hermes, &t);
    assert_eq!(
        p.items.len(),
        detected.len(),
        "every detected item must be planned"
    );
    for d0 in &detected {
        assert!(
            p.items.iter().any(|i| i.path == d0.path),
            "dropped {}",
            d0.path
        );
    }
}
