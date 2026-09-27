//! Tests for custom-provider detection and catalog reconciliation.
use super::*;

fn tmp(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("pantheon-prov-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

const REAL_HERMES: &str = r#"
model:
  default: router
  provider: custom:hp-llm-router
providers:
  hp-hype:
    name: hype
    api: https://hyper.charm.land/v1
    api_key: ${HERMES_CUSTOM_HYPER_CHARM_LAND_API_KEY}
    default_model: deepseek-v4.3-flash
    models:
      deepseek-v4.1-flash: {}
      deepseek-v4-flash: {}
      glm-5.3-flash: {}
      gemma-4-26b-a4b-it: {}
  hp-llm-router:
    name: Local Router
    api: http://127.0.0.1:8015/v1
    api_key: ${HERMES_CUSTOM_LLM_ROUTER_API_KEY}
    default_model: router
    models:
      router: {}
      code: {}
  hp-cavoti:
    name: cavoti
    api: https://cavoti.com
    api_key: ${HERMES_CUSTOM_CAVOTI_API_KEY}
  hp-anthropic-style:
    api_mode: anthropic
    api: https://api.example.com
    api_key: ${SOME_TOKEN}
other_block:
  x: 1
"#;

#[test]
fn parses_the_real_hermes_provider_block() {
    let p = parse_hermes_providers(REAL_HERMES);
    assert_eq!(p.len(), 4, "{p:#?}");
    // Sorted by id.
    assert_eq!(
        p.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
        vec![
            "hp-anthropic-style",
            "hp-cavoti",
            "hp-hype",
            "hp-llm-router"
        ]
    );

    let hype = p.iter().find(|x| x.id == "hp-hype").unwrap();
    assert_eq!(hype.label, "hype");
    assert_eq!(hype.base_url, "https://hyper.charm.land/v1");
    assert_eq!(hype.api_mode, "openai");
    assert_eq!(
        hype.key_env.as_deref(),
        Some("HERMES_CUSTOM_HYPER_CHARM_LAND_API_KEY")
    );
    assert_eq!(hype.default_model.as_deref(), Some("deepseek-v4.3-flash"));
    assert_eq!(hype.models.len(), 4);
    assert!(hype.models.iter().any(|m| m.id == "glm-5.3-flash"));
    // Hermes lists bare names, so no limits are invented.
    assert!(hype.models.iter().all(|m| m.context_limit.is_none()));
}

#[test]
fn an_explicit_anthropic_mode_survives() {
    let p = parse_hermes_providers(REAL_HERMES);
    let a = p.iter().find(|x| x.id == "hp-anthropic-style").unwrap();
    assert_eq!(a.api_mode, "anthropic");
}

#[test]
fn a_literal_key_is_flagged_and_never_becomes_a_key_env() {
    let yaml =
        "providers:\n  leaky:\n    api: https://x.example\n    api_key: sk-realsecretvalue12345\n";
    let p = parse_hermes_providers(yaml);
    assert_eq!(p.len(), 1);
    let k = p[0].key_env.as_deref().unwrap();
    assert!(k.starts_with('<'), "a literal must be marked, got {k:?}");
    assert!(!k.contains("sk-realsecretvalue12345"));
    // And the rendered section must not invent an env var for it.
    let out = render_custom_providers(&p);
    assert!(!out.contains("key_env ="), "{out}");
    assert!(!out.contains("sk-realsecretvalue12345"));
    assert!(out.contains("pantheon provider add"), "{out}");
}

#[test]
fn a_provider_with_no_base_url_is_dropped() {
    let yaml = "providers:\n  nameless:\n    name: nothing\n  ok:\n    api: https://x.example\n";
    let p = parse_hermes_providers(yaml);
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].id, "ok");
}

#[test]
fn a_key_declared_after_the_models_block_is_not_swallowed() {
    // Regression from the live config: `hp-llm-router` declares
    //   models: { router: {} }
    //   api_key: ${HERMES_CUSTOM_LLM_ROUTER_API_KEY}
    // A `models:` block that never ends swallowed the key, leaving the
    // provider registered with no way to authenticate.
    let yaml = r#"
providers:
  hp-llm-router:
    name: LLM-Router
    base_url: http://127.0.0.1:8015/v1
    model: router
    discover_models: true
    models:
      router: {}
    api_key: ${HERMES_CUSTOM_LLM_ROUTER_API_KEY}
"#;
    let p = parse_hermes_providers(yaml);
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].base_url, "http://127.0.0.1:8015/v1");
    assert_eq!(
        p[0].key_env.as_deref(),
        Some("HERMES_CUSTOM_LLM_ROUTER_API_KEY"),
        "the key after models: was dropped"
    );
    assert_eq!(p[0].default_model.as_deref(), Some("router"));
    assert_eq!(p[0].models.len(), 1);
    assert_eq!(p[0].models[0].id, "router");
}

#[test]
fn base_url_and_model_spellings_are_both_accepted() {
    // hp-hype uses `api:`, hp-llm-router uses `base_url:`.
    let api = parse_hermes_providers("providers:\n  a:\n    api: https://a.example\n");
    let base = parse_hermes_providers("providers:\n  b:\n    base_url: https://b.example\n");
    assert_eq!(api[0].base_url, "https://a.example");
    assert_eq!(base[0].base_url, "https://b.example");
}

#[test]
fn an_absent_or_empty_providers_block_is_empty_not_an_error() {
    assert!(parse_hermes_providers("model: x\n").is_empty());
    assert!(parse_hermes_providers("").is_empty());
    assert!(parse_hermes_providers("providers:\n").is_empty());
}

#[test]
fn nested_keys_inside_models_are_not_mistaken_for_provider_fields() {
    // `models:` entries have their own `api:`-looking keys in the wild; a stray
    // one must not overwrite the endpoint's wire mode or base URL.
    let yaml = r#"
providers:
  a:
    api: https://real.example
    api_mode: openai
    models:
      m1: {}
      m2:
        api: https://should-not-win.example
"#;
    let p = parse_hermes_providers(yaml);
    assert_eq!(p[0].base_url, "https://real.example");
    assert_eq!(p[0].api_mode, "openai");
    assert!(p[0].models.iter().any(|m| m.id == "m1"));
}

#[test]
fn rendered_sections_are_shaped_like_pantheons_own() {
    let p = parse_hermes_providers(REAL_HERMES);
    let out = render_custom_providers(&p);
    assert!(out.contains("[custom_providers.hp-llm-router]"));
    assert!(out.contains("base_url = \"http://127.0.0.1:8015/v1\""));
    assert!(out.contains("api_mode = \"openai\""));
    assert!(out.contains("key_env = \"HERMES_CUSTOM_LLM_ROUTER_API_KEY\""));
    // No harvested model list, and the reason is stated in the file itself.
    assert!(
        !out.contains("models = ["),
        "must not bake a model list: {out}"
    );
    assert!(
        !out.contains("glm-5.3-flash"),
        "must not name source models: {out}"
    );
    assert!(out.contains("not written down"), "{out}");
    assert!(out.contains("pantheon provider models hp-hype"), "{out}");
}

#[test]
fn an_id_needing_toml_quoting_is_quoted() {
    assert_eq!(toml_key("hp-llm-router"), "hp-llm-router");
    assert_eq!(toml_key("has space"), "\"has space\"");
    assert_eq!(toml_key(""), "\"\"");
}

#[test]
fn summary_counts_keys_and_models() {
    let p = parse_hermes_providers(REAL_HERMES);
    let s = summarise(&p);
    assert!(s.contains("4 custom provider"), "{s}");
    assert!(s.contains("4 with a key variable"), "{s}");
    assert!(summarise(&[]) == "no custom providers");
}

// ---------------------------------------------------------------------------
// catalog reconciliation
// ---------------------------------------------------------------------------

#[test]
fn a_key_matching_a_catalog_key_env_is_catalogued() {
    let cat = catalog_key_envs();
    assert!(!cat.is_empty(), "the catalog should expose key_envs");
    // Every catalog provider's own key_env must reconcile to itself.
    for (id, kenv) in &cat {
        let r = reconcile_key(kenv, &cat);
        assert_eq!(
            r.match_kind,
            KeyMatch::Catalogued,
            "{id}: {kenv} did not reconcile to itself"
        );
        assert_eq!(r.provider.as_deref(), Some(id.as_str()));
    }
}

#[test]
fn an_unrelated_service_token_is_unmatched_not_guessed() {
    let cat = catalog_key_envs();
    let r = reconcile_key("TELEGRAM_BOT_TOKEN", &cat);
    assert_eq!(r.match_kind, KeyMatch::Unmatched, "{r:?}");
    assert!(r.provider.is_none());
}

#[test]
fn a_key_with_the_right_stem_but_wrong_suffix_reports_the_rename() {
    let cat = vec![("acme".to_string(), "ACME_API_KEY".to_string())];
    let r = reconcile_key("ACME_TOKEN", &cat);
    match r.match_kind {
        KeyMatch::NeedsRename { catalog_env } => assert_eq!(catalog_env, "ACME_API_KEY"),
        other => panic!("expected NeedsRename, got {other:?}"),
    }
    assert_eq!(r.provider.as_deref(), Some("acme"));
}

#[test]
fn an_exact_match_wins_over_a_stem_match() {
    let cat = vec![
        ("one".to_string(), "SHARED_API_KEY".to_string()),
        ("two".to_string(), "SHARED_KEY".to_string()),
    ];
    // Both are exact `key_env` entries, so both are catalogued — the stem path
    // must not be reached for either, and must not pick the other provider.
    assert_eq!(
        reconcile_key("SHARED_KEY", &cat),
        KeyReport {
            env_var: "SHARED_KEY".into(),
            match_kind: KeyMatch::Catalogued,
            provider: Some("two".into()),
        }
    );
    assert_eq!(
        reconcile_key("SHARED_API_KEY", &cat).match_kind,
        KeyMatch::Catalogued
    );
    // A suffix variant that is in neither row is a rename, resolved to the
    // provider whose stem matches.
    assert_eq!(
        reconcile_key("SHARED_TOKEN", &cat),
        KeyReport {
            env_var: "SHARED_TOKEN".into(),
            match_kind: KeyMatch::NeedsRename {
                catalog_env: "SHARED_API_KEY".into()
            },
            provider: Some("one".into()),
        }
    );
}

#[test]
fn a_short_stem_is_not_matched_to_avoid_false_positives() {
    let cat = vec![("x".to_string(), "X_API_KEY".to_string())];
    // "A_KEY" -> stem "A", too short to be evidence of anything.
    assert_eq!(reconcile_key("A_KEY", &cat).match_kind, KeyMatch::Unmatched);
}

#[test]
fn reconciling_the_real_hermes_keys_finds_the_catalogued_ones() {
    // The live `.env` on the reference machine.
    let names: Vec<String> = vec![
        "OPENROUTER_API_KEY".into(),
        "GROQ_API_KEY".into(),
        "NEBIUS_API_KEY".into(),
        "NVIDIA_API_KEY".into(),
        "CLOUDFLARE_API_TOKEN".into(),
        "DASHSCOPE_API_KEY".into(),
        "HERMES_CUSTOM_LLM_ROUTER_API_KEY".into(),
        "TELEGRAM_BOT_TOKEN".into(),
    ];
    let reports = reconcile_keys(&names);
    let by = |n: &str| reports.iter().find(|r| r.env_var == n).unwrap().clone();
    assert_eq!(by("OPENROUTER_API_KEY").match_kind, KeyMatch::Catalogued);
    assert_eq!(by("GROQ_API_KEY").match_kind, KeyMatch::Catalogued);
    assert_eq!(by("TELEGRAM_BOT_TOKEN").match_kind, KeyMatch::Unmatched);
    // A custom-provider key has no catalog entry by design.
    assert_eq!(
        by("HERMES_CUSTOM_LLM_ROUTER_API_KEY").match_kind,
        KeyMatch::Unmatched
    );
}

#[test]
fn reading_a_missing_config_yields_no_providers() {
    assert!(read_providers(&tmp("missing").join("nope.yaml")).is_empty());
}
