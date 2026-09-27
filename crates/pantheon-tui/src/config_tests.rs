//! Tests for `crate::config::tests` — sibling file so sources stay test-free.
use super::*;

#[test]
fn round_trips_through_toml() {
    let dir = std::env::temp_dir().join(format!("pantheon-cfg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = Config {
        profile: Some("dev".into()),
        agent: None,
        model: Some(ModelSection {
            reasoning_budget: None,
            provider: "hp-llm-router".into(),
            model: "longcat".into(),
            api_key_env: Some("PANTHEON_API_KEY".into()),
            fallbacks: vec![FallbackEntry {
                provider: "local".into(),
                model: "llama3.2".into(),
            }],
            reasoning: None,
        }),
        judge: Some(JudgeSection {
            provider: "local".into(),
            model: "qwen2.5:1.5b".into(),
            api_key_env: None,
        }),
        compression: Some(CompressionSection {
            provider: "local".into(),
            model: "summarizer".into(),
            api_key_env: None,
        }),
        title_gen: Some(TitleGenSection {
            provider: "local".into(),
            model: "namer".into(),
            api_key_env: None,
        }),
        stt: Some(VoiceSection {
            backend: "command".into(),
            options: [("cmd".into(), "whisper-cli".into())].into_iter().collect(),
        }),
        tts: None,
        policy: Some(PolicyPreset::Coder),
        memory: Some(MemorySection {
            backend: "native".into(),
            options: Default::default(),
        }),
        tools: None,
        server: Some(ServerSection {
            port: 18789,
            host: "127.0.0.1".into(),
        }),
        custom_providers: Default::default(),
        agents: Default::default(),
        embeddings: None,
        search_synthesis: None,
        vision: None,
        scheduled: None,
        mcp_synthesis: None,
    };
    cfg.save(&dir).unwrap();
    let loaded = Config::load(&dir).unwrap();
    assert_eq!(loaded, cfg);
    // The file must never contain a raw key, only the env var name.
    let text = std::fs::read_to_string(Config::path(&dir)).unwrap();
    assert!(!text.contains("sk-"));
    assert!(text.contains("PANTHEON_API_KEY"));
}

#[test]
fn validate_reports_missing_model_and_unset_env() {
    let cfg = Config {
        model: Some(ModelSection {
            provider: "p".into(),
            model: "m".into(),
            api_key_env: Some("PANTHEON_DEFINITELY_UNSET_VAR_42".into()),
            fallbacks: vec![],
            reasoning: None,
            reasoning_budget: None,
        }),
        ..Default::default()
    };
    let problems = cfg.validate();
    assert!(problems
        .iter()
        .any(|p| p.contains("PANTHEON_DEFINITELY_UNSET_VAR_42")));
    let empty = Config::default();
    assert!(empty.validate().iter().any(|p| p.contains("[model]")));
}

#[test]
fn judge_section_parses_from_toml() {
    let cfg: Config = toml::from_str(
        "[model]\nprovider = \"local\"\nmodel = \"llama3.2\"\n\
             [judge]\nprovider = \"local\"\nmodel = \"qwen2.5:1.5b\"\n",
    )
    .unwrap();
    let d = cfg.judge.as_ref().expect("judge section parsed");
    assert_eq!(d.provider, "local");
    assert_eq!(d.model, "qwen2.5:1.5b");
    assert_eq!(cfg.validate(), Vec::<String>::new());
    // Configs without [judge] still parse (back-compat).
    let old: Config = toml::from_str("[model]\nprovider = \"p\"\nmodel = \"m\"\n").unwrap();
    assert!(old.judge.is_none());
    assert!(old.compression.is_none());
}

#[test]
fn aux_target_resolves_field_wise() {
    // One test for the shared generic (previously triplicated per kind).
    let sec = AuxSection {
        provider: "openai".into(),
        model: "gpt-4o-mini".into(),
        api_key_env: None,
    };
    let pm: for<'a> fn(&'a AuxSection) -> Option<(&'a String, &'a String)> =
        |s| Some((&s.provider, &s.model));
    // No env: config wins.
    assert_eq!(
        aux_target(pm(&sec), None, None),
        Some(("openai".into(), "gpt-4o-mini".into()))
    );
    // Env overrides one field, config fills the other.
    assert_eq!(
        aux_target(pm(&sec), Some("anthropic".into()), None),
        Some(("anthropic".into(), "gpt-4o-mini".into()))
    );
    // Env alone activates (no section at all).
    assert_eq!(
        aux_target(
            None,
            Some("http://127.0.0.1:8016/v1".into()),
            Some("typed".into())
        ),
        Some(("http://127.0.0.1:8016/v1".into(), "typed".into()))
    );
    // Partial env with no section stays off.
    assert_eq!(aux_target(None, Some("openai".into()), None), None);
    // Whitespace-only values count as absent.
    assert_eq!(
        aux_target(pm(&sec), Some("  ".into()), None),
        Some(("openai".into(), "gpt-4o-mini".into()))
    );
    // Nothing configured = None.
    assert_eq!(aux_target(None, None, None), None);
}

#[test]
fn aux_slots_cover_all_capabilities() {
    // The table is the contract: 8 rows, env/vault names, auto flags.
    // Adding a capability means adding one row here and in AUX_SLOTS.
    let rows: Vec<(&str, &str, &str, bool)> = AUX_SLOTS
        .iter()
        .map(|s| (s.name, s.env_prefix, s.vault_name, s.auto))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("judge", "JUDGE", "PANTHEON_JUDGE_API_KEY", true),
            (
                "compression",
                "COMPRESSION",
                "PANTHEON_COMPRESSION_API_KEY",
                true
            ),
            ("title_gen", "TITLEGEN", "PANTHEON_TITLEGEN_API_KEY", true),
            (
                "embeddings",
                "EMBEDDINGS",
                "PANTHEON_EMBEDDINGS_API_KEY",
                false
            ),
            (
                "search_synthesis",
                "SEARCH_SYNTHESIS",
                "PANTHEON_SEARCH_SYNTHESIS_API_KEY",
                true
            ),
            ("vision", "VISION", "PANTHEON_VISION_API_KEY", true),
            ("scheduled", "SCHEDULED", "PANTHEON_SCHEDULED_API_KEY", true),
            (
                "mcp_synthesis",
                "MCP_SYNTHESIS",
                "PANTHEON_MCP_SYNTHESIS_API_KEY",
                true
            ),
        ]
    );
    // Every slot resolves against a matching section.
    let cfg = Config {
        judge: Some(AuxSection {
            provider: "p".into(),
            model: "m".into(),
            api_key_env: None,
        }),
        ..Default::default()
    };
    let kinds: Vec<String> = auxiliaries(
        Some(&cfg),
        &pantheon_api::model::DefaultModel {
            provider: "d".into(),
            model: "dm".into(),
        },
    )
    .iter()
    .map(|a| format!("{:?}", a.kind))
    .collect();
    assert!(kinds.contains(&"Judge".to_string()), "{kinds:?}");
}

#[test]
fn auxiliaries_combine_judge_and_compression() {
    let cfg: Config = toml::from_str(
        "[model]\nprovider = \"local\"\nmodel = \"llama3.2\"\n\
             [judge]\nprovider = \"local\"\nmodel = \"qwen2.5:1.5b\"\n\
             [compression]\nprovider = \"local\"\nmodel = \"summarizer\"\n",
    )
    .unwrap();
    let default = pantheon_api::model::DefaultModel {
        provider: "local".into(),
        model: "llama3.2".into(),
    };
    let aux = auxiliaries(Some(&cfg), &default);
    // Judge, compression, title, search synthesis, vision, scheduled,
    // MCP synthesis: seven auto-defaulting kinds. Embeddings is the
    // documented exception (absent = local embedder) so it is absent.
    assert_eq!(aux.len(), 7, "seven auto aux kinds; embeddings unpinned");
    assert!(
        !aux.iter()
            .any(|a| matches!(a.kind, pantheon_api::model::AuxiliaryKind::Embeddings)),
        "unpinned embeddings must not resolve to the chat model"
    );
    assert!(aux
        .iter()
        .any(|a| matches!(a.kind, pantheon_api::model::AuxiliaryKind::Judge)));
    assert!(aux
        .iter()
        .any(|a| matches!(a.kind, pantheon_api::model::AuxiliaryKind::Compression)));
    // Pinned sections keep their own target …
    let dec = aux
        .iter()
        .find(|a| matches!(a.kind, pantheon_api::model::AuxiliaryKind::Judge))
        .unwrap();
    assert_eq!(dec.model, "qwen2.5:1.5b");
    // … while the unconfigured aux (title) is `auto` = default model.
    let title = aux
        .iter()
        .find(|a| matches!(a.kind, pantheon_api::model::AuxiliaryKind::TitleGen))
        .unwrap();
    assert_eq!(
        (title.provider.as_str(), title.model.as_str()),
        ("local", "llama3.2")
    );
}

#[test]
fn unconfigured_aux_default_to_auto_on_the_default_model() {
    // Aux models default to `auto`: nothing configured still resolves
    // to a target (the run's default model), never "off".
    let bare: Config = toml::from_str("[model]\nprovider = \"p\"\nmodel = \"m\"\n").unwrap();
    let default = pantheon_api::model::DefaultModel {
        provider: "openai".into(),
        model: "gpt-4o-mini".into(),
    };
    let aux = auxiliaries(Some(&bare), &default);
    assert_eq!(aux.len(), 7, "every auto-kind aux always has a target");
    for a in &aux {
        assert_eq!(
            (a.provider.as_str(), a.model.as_str()),
            ("openai", "gpt-4o-mini"),
            "auto aux points at the default model"
        );
    }
    // No config file at all behaves the same.
    assert_eq!(auxiliaries(None, &default).len(), 7);
    // Env pin still wins over auto.
    std::env::set_var("PANTHEON_TITLEGEN_MODEL", "namer-env");
    std::env::set_var("PANTHEON_TITLEGEN_PROVIDER", "openai");
    let aux = auxiliaries(None, &default);
    let title = aux
        .iter()
        .find(|a| matches!(a.kind, pantheon_api::model::AuxiliaryKind::TitleGen))
        .unwrap();
    assert_eq!(title.model, "namer-env");
    std::env::remove_var("PANTHEON_TITLEGEN_MODEL");
    std::env::remove_var("PANTHEON_TITLEGEN_PROVIDER");
}

#[test]
fn title_gen_section_parses_from_toml_and_reaches_auxiliaries() {
    let cfg: Config = toml::from_str(
        "[model]\nprovider = \"local\"\nmodel = \"llama3.2\"\n\
             [title_gen]\nprovider = \"local\"\nmodel = \"namer\"\n",
    )
    .unwrap();
    let t = cfg.title_gen.as_ref().expect("title_gen section parsed");
    assert_eq!(t.model, "namer");
    assert_eq!(cfg.validate(), Vec::<String>::new());
    let default = pantheon_api::model::DefaultModel {
        provider: "local".into(),
        model: "llama3.2".into(),
    };
    let aux = auxiliaries(Some(&cfg), &default);
    let title = aux
        .iter()
        .find(|a| matches!(a.kind, pantheon_api::model::AuxiliaryKind::TitleGen))
        .expect("title entry present");
    assert_eq!(title.model, "namer", "pinned [title_gen] beats auto");
    // Absent section parses (back-compat) and adds no pin — the aux
    // resolves to `auto` (the default model), never to "off".
    let old: Config = toml::from_str("[model]\nprovider = \"p\"\nmodel = \"m\"\n").unwrap();
    assert!(old.title_gen.is_none());
    // Absent section adds no pin: auxiliaries() resolves `auto`
    // (asserted below), never "off".
    let auto = auxiliaries(Some(&old), &default)
        .iter()
        .find(|a| matches!(a.kind, pantheon_api::model::AuxiliaryKind::TitleGen))
        .unwrap()
        .clone();
    assert_eq!(auto.model, "llama3.2");
}

#[test]
fn voice_sections_parse_and_validate() {
    let cfg: Config = toml::from_str(
            "[model]\nprovider = \"local\"\nmodel = \"llama3.2\"\n\
             [stt]\nbackend = \"command\"\n[stt.options]\ncmd = \"whisper-cli\"\n\
             args = \"-m m.bin -f {file} -nt\"\n\
             [tts]\nbackend = \"openai\"\n[tts.options]\nprovider = \"openai\"\nmodel = \"tts-1\"\n",
        )
        .unwrap();
    let stt = cfg.stt.as_ref().expect("stt section");
    assert_eq!(stt.backend, "command");
    assert_eq!(stt.options["cmd"], "whisper-cli");
    assert_eq!(cfg.tts.as_ref().unwrap().backend, "openai");
    // Fully specified voice config validates clean.
    assert_eq!(cfg.validate(), Vec::<String>::new());

    // Backend-specific required options are checked.
    let bad: Config = toml::from_str(
        "[model]\nprovider = \"p\"\nmodel = \"m\"\n\
             [stt]\nbackend = \"command\"\n\
             [tts]\nbackend = \"openai\"\n",
    )
    .unwrap();
    let problems = bad.validate();
    assert!(problems.iter().any(|p| p.contains("stt.options.cmd")));
    assert!(problems.iter().any(|p| p.contains("tts.options.provider")));
    // Absent sections = no voice capability, no complaints.
    let none: Config = toml::from_str("[model]\nprovider = \"p\"\nmodel = \"m\"\n").unwrap();
    assert!(none.stt.is_none() && none.tts.is_none());
    assert_eq!(none.validate(), Vec::<String>::new());
}

#[test]
fn model_key_env_reads_config_not_the_key() {
    let cfg: Config =
        toml::from_str("[model]\nprovider = \"p\"\nmodel = \"m\"\napi_key_env = \"MY_KEY_VAR\"\n")
            .unwrap();
    assert_eq!(model_key_env(Some(&cfg)).as_deref(), Some("MY_KEY_VAR"));
    // Configured without api_key_env (or no config at all) = the
    // default PANTHEON_API_KEY path, never a missing key.
    let bare: Config = toml::from_str("[model]\nprovider = \"p\"\nmodel = \"m\"\n").unwrap();
    assert_eq!(model_key_env(Some(&bare)), None);
    assert_eq!(model_key_env(None), None);
}

#[test]
fn chat_secrets_resolves_the_config_named_model_key() {
    // Unique var name so parallel tests cannot collide; the
    // config-named var must win over the plain PANTHEON_API_KEY
    // fallback (this is the chat path bug: run honored it, chat
    // passed None).
    std::env::set_var("PANTHEON_TEST_CHAT_KEY_A7F3", "sk-from-config-env");
    let cfg: Config = toml::from_str(
        "[model]\nprovider = \"p\"\nmodel = \"m\"\n\
             api_key_env = \"PANTHEON_TEST_CHAT_KEY_A7F3\"\n",
    )
    .unwrap();
    let broker = chat_secrets(Some(&cfg));
    let k = broker
        .resolve("PANTHEON_API_KEY")
        .unwrap()
        .expect("config-named key env resolves into PANTHEON_API_KEY");
    assert_eq!(k.expose(), "sk-from-config-env");
    std::env::remove_var("PANTHEON_TEST_CHAT_KEY_A7F3");
}

#[test]
fn chat_secrets_seeds_every_aux_key() {
    // with_aux_keys was the documented session-builder entry point but
    // had zero callers: the compression key never seeded on any path.
    std::env::set_var("PANTHEON_TEST_DEC_KEY_A7F3", "dec-1");
    std::env::set_var("PANTHEON_TEST_CMP_KEY_A7F3", "cmp-1");
    std::env::set_var("PANTHEON_TEST_TTL_KEY_A7F3", "ttl-1");
    let cfg: Config = toml::from_str(
        "[model]\nprovider = \"p\"\nmodel = \"m\"\n\
             [judge]\nprovider = \"p\"\nmodel = \"d\"\n\
             api_key_env = \"PANTHEON_TEST_DEC_KEY_A7F3\"\n\
             [compression]\nprovider = \"p\"\nmodel = \"c\"\n\
             api_key_env = \"PANTHEON_TEST_CMP_KEY_A7F3\"\n\
             [title_gen]\nprovider = \"p\"\nmodel = \"t\"\n\
             api_key_env = \"PANTHEON_TEST_TTL_KEY_A7F3\"\n",
    )
    .unwrap();
    let broker = chat_secrets(Some(&cfg));
    assert_eq!(
        broker
            .resolve("PANTHEON_JUDGE_API_KEY")
            .unwrap()
            .map(|s| s.expose().to_string()),
        Some("dec-1".into())
    );
    assert_eq!(
        broker
            .resolve("PANTHEON_COMPRESSION_API_KEY")
            .unwrap()
            .map(|s| s.expose().to_string()),
        Some("cmp-1".into())
    );
    assert_eq!(
        broker
            .resolve("PANTHEON_TITLEGEN_API_KEY")
            .unwrap()
            .map(|s| s.expose().to_string()),
        Some("ttl-1".into())
    );
    std::env::remove_var("PANTHEON_TEST_DEC_KEY_A7F3");
    std::env::remove_var("PANTHEON_TEST_CMP_KEY_A7F3");
    std::env::remove_var("PANTHEON_TEST_TTL_KEY_A7F3");
}

/// A migrated custom provider's model rows must survive the real config
/// parser and land in the catalog.
///
/// The schema gap this closes: `CustomProviderSection` had no `models` field,
/// so `register_custom_providers` built every migrated endpoint with
/// `models: Vec::new()`. The operator got a provider with no models listed and
/// had to type a model id they could not see.
#[test]
fn a_custom_provider_carries_its_models_into_the_catalog() {
    let dir = std::env::temp_dir().join(format!("pantheon-cfgmodels-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        r#"
[custom_providers.hp-llm-router]
base_url = "http://127.0.0.1:8015/v1"
api_mode = "openai"
key_env = "HERMES_CUSTOM_LLM_ROUTER_API_KEY"
models = [
  { id = "router" },
  { id = "code", context_limit = 500000, max_output_tokens = 32000 },
  { id = "chat", context_limit = 256000 },
]
"#,
    )
    .unwrap();

    let cfg = Config::load(&dir).expect("migrated config must parse");
    let sec = cfg
        .custom_providers
        .get("hp-llm-router")
        .expect("provider row present");
    assert_eq!(sec.models.len(), 3);
    assert_eq!(sec.models[1].context_limit, Some(500000));
    assert_eq!(sec.models[1].max_output_tokens, Some(32000));
    // A row with no limit stays unknown rather than being guessed.
    assert_eq!(sec.models[0].context_limit, None);

    register_custom_providers(&cfg);
    let all = pantheon_providers::catalog::all_providers();
    let p = all
        .iter()
        .find(|p| p.id == "hp-llm-router")
        .expect("registered into the catalog");
    assert_eq!(
        p.models.len(),
        3,
        "models must reach the catalog, not just config"
    );
    let code = p.models.iter().find(|m| m.model == "code").unwrap();
    assert_eq!(code.context_limit, Some(500000));
    // Conservative defaults for anything undeclared.
    assert!(code.tools, "unknown models default tools on");
    assert!(code.streaming, "unknown models default streaming on");
    assert!(!code.vision, "unknown models do not claim vision");
    let router = p.models.iter().find(|m| m.model == "router").unwrap();
    assert_eq!(router.context_limit, None, "no limit invented");
    assert!(router.tools);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Back-compat: a config written before the `models` field existed still
/// loads, and still registers a usable endpoint.
#[test]
fn a_custom_provider_without_models_still_registers() {
    let dir = std::env::temp_dir().join(format!("pantheon-cfgnomodels-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        "[custom_providers.legacy]\nbase_url = \"https://x.example/v1\"\n",
    )
    .unwrap();
    let cfg = Config::load(&dir).expect("an old config must still load");
    assert!(cfg.custom_providers["legacy"].models.is_empty());
    register_custom_providers(&cfg);
    let all = pantheon_providers::catalog::all_providers();
    let p = all
        .iter()
        .find(|p| p.id == "legacy")
        .expect("still registered");
    assert!(p.models.is_empty());
    assert_eq!(p.base_url, "https://x.example/v1");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A hand-named model is written down; a picked-from-fetch one is not.
///
/// This is the only path that adds a row to `[custom_providers.*].models`. A
/// model list copied from another agent's config is deliberately never written:
/// it is a stale snapshot of a third-party endpoint.
#[test]
fn a_hand_named_model_is_recorded_and_is_idempotent() {
    let dir = std::env::temp_dir().join(format!("pantheon-remember-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        "[custom_providers.p]\nbase_url = \"https://x.example/v1\"\n",
    )
    .unwrap();

    assert!(remember_custom_model(&dir, "p", "my-model").unwrap());
    // Second time is a no-op, not a duplicate.
    assert!(!remember_custom_model(&dir, "p", "my-model").unwrap());
    remember_custom_model(&dir, "p", "another").unwrap();

    let cfg = Config::load(&dir).unwrap();
    let ids: Vec<&str> = cfg.custom_providers["p"]
        .models
        .iter()
        .map(|m| m.id.as_str())
        .collect();
    assert_eq!(ids, vec!["another", "my-model"], "sorted, no dupes");
    // An unnamed limit stays unknown rather than being invented.
    assert!(cfg.custom_providers["p"]
        .models
        .iter()
        .all(|m| m.context_limit.is_none()));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn recording_a_model_on_an_unknown_provider_is_a_clear_error() {
    let dir = std::env::temp_dir().join(format!("pantheon-remember-bad-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("config.toml"), "[core]\n").unwrap();
    let e = remember_custom_model(&dir, "nope", "m").unwrap_err();
    assert!(e.contains("no custom provider"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn re_adding_a_provider_keeps_the_models_you_named() {
    // Regression: `upsert_custom_row` replaced the whole section, so
    // re-picking a provider silently emptied its model list.
    let dir = std::env::temp_dir().join(format!("pantheon-upsert-keep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        "[custom_providers.p]\nbase_url = \"https://old.example/v1\"\n",
    )
    .unwrap();
    remember_custom_model(&dir, "p", "kept-model").unwrap();

    upsert_custom_row(
        &dir,
        "p",
        "https://new.example/v1",
        pantheon_providers::catalog::ApiMode::OpenAi,
        "PANTHEON_KEY_P",
    )
    .unwrap();

    let cfg = Config::load(&dir).unwrap();
    let sec = &cfg.custom_providers["p"];
    assert_eq!(sec.base_url, "https://new.example/v1", "the url updates");
    assert_eq!(sec.key_env.as_deref(), Some("PANTHEON_KEY_P"));
    assert_eq!(sec.models.len(), 1, "the named model must survive");
    assert_eq!(sec.models[0].id, "kept-model");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn agent_identity_defaults_isolate_namespaces() {
    use super::*;
    let a = AgentIdentity::default();
    assert_eq!(a.name("nyx"), "nyx");
    assert_eq!(a.namespace("nyx"), "agent:nyx");
    assert_ne!(
        a.namespace("nyx"),
        AgentIdentity::default().namespace("ero")
    );
}

#[test]
fn agent_registry_rejects_bad_names_policies_and_namespace_clashes() {
    use super::*;
    let default_policy = "coder";
    let mut reg = ProfileRegistry::new();
    reg.insert("nyx", AgentIdentity::default())
        .expect("nyx is valid");
    assert!(reg.validate_all(default_policy).is_ok());

    // A name that is not a slug is refused at insert time, so an invalid
    // config can never be written and then used.
    assert!(reg.insert("not a slug!", AgentIdentity::default()).is_err());

    // An unknown policy is refused when the profile is declared, and the
    // error names both the profile and the value it got, so a doctor message
    // can tell the user which line of which table to fix.
    let mut bad_policy = ProfileRegistry::new();
    let err = bad_policy
        .insert(
            "ero",
            AgentIdentity {
                policy: Some("yolo".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("ero") && msg.contains("yolo"),
        "the message must name the profile and the bad value: {msg}"
    );
    assert!(bad_policy.validate_all(default_policy).is_err());

    // Two profiles may not share a memory namespace: that is one agent's
    // memories leaking into another's. Each insert is valid on its own, so
    // this is caught by validating the set, not by inserting.
    let mut clash = ProfileRegistry::new();
    clash
        .insert("nyx", AgentIdentity::default())
        .expect("valid");
    clash
        .insert(
            "ero",
            AgentIdentity {
                memory_namespace: Some("agent:nyx".into()),
                ..Default::default()
            },
        )
        .expect("the name and policy are fine; the clash is the problem");
    assert!(clash.validate_all(default_policy).is_err());
}
#[test]
fn agents_table_round_trips_and_old_configs_stay_empty() {
    use super::*;
    let dir = std::env::temp_dir().join(format!("pantheon-agents-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        "[agents.nyx]\ndisplay_name = \"Nyx\"\nsoul_file = \"soul/nyx.md\"\npolicy = \"coder\"\n",
    )
    .unwrap();
    let cfg = Config::load(&dir).unwrap();
    assert_eq!(cfg.agents["nyx"].name("nyx"), "Nyx");
    // Every declared profile in this config must validate.
    for (name, profile) in &cfg.agents {
        let mut reg = ProfileRegistry::new();
        reg.insert(name, profile.clone())
            .expect("a written config is valid");
        assert!(reg.validate_all("coder").is_ok());
    }
    // No [agents] table at all: anonymous, as before.
    std::fs::write(dir.join("config.toml"), "[core]\n").unwrap();
    let old = Config::load(&dir).unwrap();
    assert!(old.agents.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn config_validate_surfaces_bad_agent_tables() {
    use super::*;
    // model: None keeps the focus on agent problems, which must surface
    // regardless of what the model section says.
    let mut cfg = Config {
        model: None,
        ..Default::default()
    };
    cfg.agents.insert(
        "ero".into(),
        AgentIdentity {
            policy: Some("yolo".into()),
            ..Default::default()
        },
    );
    let problems = cfg.validate();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("ero") && p.contains("policy")),
        "unknown agent policy surfaces: {problems:?}"
    );
    // A clash between two tables surfaces too.
    cfg.agents.insert(
        "nyx".into(),
        AgentIdentity {
            memory_namespace: Some("agent:ero".into()),
            ..Default::default()
        },
    );
    let problems = cfg.validate();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("share memory namespace")),
        "namespace clash surfaces: {problems:?}"
    );
}

/// A config written by an older Pantheon can contain a `[tools]` table with
/// `packs` and `plugins`. Those keys are inert today (tool registration is
/// unconditional and nothing reads them), so loading such a file must
/// succeed rather than fail with a parse error. This is the compatibility
/// guarantee that let the typed `ToolSection` be dropped.
#[test]
fn a_legacy_tools_table_still_loads() {
    let dir = std::env::temp_dir().join(format!("pantheon-cfg-legacy-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        r#"
[model]
provider = "local"
model = "llama3.2"

[tools]
packs = ["core"]
plugins = ["my-plugin"]
"#,
    )
    .unwrap();

    let cfg = Config::load(&dir).expect("legacy [tools] table must not be a parse error");
    assert!(cfg.tools.is_some(), "the table should be retained verbatim");
    let model = cfg.model.expect("model section should still parse");
    assert_eq!(model.model, "llama3.2");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn validate_flags_unknown_reasoning_but_keeps_parsing() {
    let cfg = Config {
        model: Some(ModelSection {
            provider: "p".into(),
            model: "m".into(),
            api_key_env: None,
            fallbacks: vec![],
            reasoning: Some("ultra".into()),
            reasoning_budget: None,
        }),
        ..Default::default()
    };
    let problems = cfg.validate();
    assert!(
        problems.iter().any(|p| p.contains("model.reasoning")),
        "unknown level flagged: {problems:?}"
    );
}
