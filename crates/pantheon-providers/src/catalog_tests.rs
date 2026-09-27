//! Tests for `crate::catalog::tests` — sibling file so sources stay test-free.
use super::*;

#[test]
fn providers_resolve_with_wire_modes() {
    assert!(provider("anthropic").is_some());
    assert_eq!(provider("anthropic").unwrap().api_mode, ApiMode::Anthropic);
    assert_eq!(provider("openai").unwrap().api_mode, ApiMode::OpenAi);
    assert!(provider("router").is_some());
    assert!(provider("nope").is_none());
}

#[test]
fn model_capabilities_and_cost() {
    let m = model("openai", "gpt-4o").unwrap();
    assert_eq!(m.context_limit, Some(128_000));
    assert!(m.tools && m.vision && m.streaming && !m.reasoning);
    let cost = m.cost.estimate(1_000_000, 1_000_000).unwrap();
    assert!((cost - 12.50).abs() < 1e-9);

    let c = model("anthropic", "claude-sonnet-4").unwrap();
    assert!(c.reasoning && c.vision && c.tools);
}

#[test]
fn unknown_model_gets_conservative_defaults() {
    let m = model_meta("local", "llama3.2");
    assert_eq!(m.context_limit, None);
    assert!(m.tools && m.streaming && !m.vision && !m.reasoning);
    assert!(m.cost.estimate(10, 10).is_none());
}

#[test]
fn unknown_provider_id_passthrough_is_base_url() {
    assert_eq!(
        base_url_for("https://proxy.example/v1"),
        "https://proxy.example/v1"
    );
}

#[test]
fn a_bare_word_is_not_mistaken_for_a_base_url() {
    // A typo like `provider = "http"` used to become the literal base URL
    // "http". The request then went to the relative path
    // "http/chat/completions", failed as a retryable network error, and the
    // chain reported PROVIDER_EXHAUSTED, so the user never learned the
    // provider name was wrong. It must be a config error instead.
    assert_eq!(base_url_for("http"), "");
    let err = resolve_base_url("http").unwrap_err();
    assert!(err.contains("http"), "error must name the bad id: {err}");
    assert!(
        err.contains("PANTHEON_BASE_HTTP"),
        "error must name the env var to set: {err}"
    );
    // Every catalog provider still resolves.
    for id in ["openai", "anthropic", "groq", "openrouter"] {
        assert!(
            resolve_base_url(id).is_ok(),
            "catalog provider {id} must still resolve"
        );
    }
}

#[test]
fn template_vars_parse_and_resolve() {
    assert!(template_vars("https://api.openai.com/v1").is_empty());
    assert_eq!(
        template_vars("https://{resource}.x.com/{ver}/"),
        vec!["resource", "ver"]
    );
    // Duplicates collapse, empties ignored.
    assert_eq!(template_vars("{a}/{a}/{}"), vec!["a"]);
    assert_eq!(
        config_env_name("azure", "resource"),
        "PANTHEON_AZURE_RESOURCE"
    );
    assert_eq!(
        config_env_name("my-llm", "account_id"),
        "PANTHEON_MY_LLM_ACCOUNT_ID"
    );
}

#[test]
fn resolve_interpolates_or_names_every_missing_var() {
    // Unique custom id so parallel tests can't collide on the global.
    let id = format!("test-tpl-{}", std::process::id());
    register_custom_provider(ProviderMeta {
        id: id.clone(),
        label: id.clone(),
        base_url: "https://{region}.example.com/{account_id}/v1".into(),
        api_mode: ApiMode::OpenAi,
        base_env: String::new(),
        key_env: String::new(),
        key_header: default_auth_header(),
        models: Vec::new(),
        prominent: false,
        dev: false,
        tag: String::new(),
    });
    assert_eq!(
        required_config_vars(&id),
        vec!["region".to_string(), "account_id".to_string()]
    );
    let region_env = config_env_name(&id, "region");
    let acct_env = config_env_name(&id, "account_id");
    std::env::remove_var(&region_env);
    std::env::remove_var(&acct_env);
    let err = resolve_base_url(&id).unwrap_err();
    assert!(err.contains(&region_env), "names region: {err}");
    assert!(err.contains(&acct_env), "names account: {err}");
    std::env::set_var(&region_env, "us-east-1");
    std::env::set_var(&acct_env, "abc123");
    assert_eq!(
        resolve_base_url(&id).unwrap(),
        "https://us-east-1.example.com/abc123/v1"
    );
    std::env::remove_var(&region_env);
    std::env::remove_var(&acct_env);
    // Plain providers resolve untouched.
    assert_eq!(
        resolve_base_url("openai").unwrap(),
        "https://api.openai.com/v1"
    );
}

#[test]
fn registry_covers_the_builtin_set_with_correct_auth_paths() {
    // Count: 8 majors+clouds + 10 aggregators + 11 labs + 7 inference + 3 local = 39.
    assert_eq!(providers().len(), 39);
    // Vendor-native key envs.
    assert_eq!(provider("openai").unwrap().key_env, "OPENAI_API_KEY");
    assert_eq!(provider("huggingface").unwrap().key_env, "HF_TOKEN");
    assert_eq!(
        provider("bedrock").unwrap().key_env,
        "AWS_BEARER_TOKEN_BEDROCK"
    );
    assert_eq!(
        provider("novita").unwrap().base_url,
        "https://api.novita.ai/openai/v1"
    );
    assert_eq!(
        provider("nebius").unwrap().base_url,
        "https://api.tokenfactory.nebius.com/v1"
    );
    assert_eq!(
        provider("upstage").unwrap().base_url,
        "https://api.upstage.ai/v1"
    );
    assert_eq!(
        provider("mimo").unwrap().base_url,
        "https://api.xiaomimimo.com/v1"
    );
    // Templates stay templated in the catalog; `resolve_base_url`
    // interpolates them from PANTHEON_<PROVIDER>_<VAR> values.
    assert!(provider("azure").unwrap().base_url.contains("{resource}"));
    assert!(provider("vertex").unwrap().base_url.contains("{project}"));
    assert!(provider("cloudflare")
        .unwrap()
        .base_url
        .contains("{account_id}"));
    // Auth header paths: Bearer default, raw api-key for MiMo.
    assert_eq!(key_header_for("openai"), "Authorization");
    assert_eq!(key_header_for("mimo"), "api-key");
    assert_eq!(key_header_for("no-such-provider"), "Authorization");
}

#[test]
fn prominent_providers_include_curated_labs() {
    let prom: Vec<&str> = providers()
        .iter()
        .filter(|p| p.prominent)
        .map(|p| p.id.as_str())
        .collect();
    // Should include the major labs from the catalog.
    assert!(prom.contains(&"anthropic"), "anthropic prominent: {prom:?}");
    assert!(prom.contains(&"openai"), "openai prominent: {prom:?}");
    assert!(prom.contains(&"google"), "google prominent: {prom:?}");
    assert!(prom.contains(&"deepseek"), "deepseek prominent: {prom:?}");
    assert!(prom.contains(&"groq"), "groq prominent: {prom:?}");
    assert!(prom.contains(&"xai"), "xai prominent: {prom:?}");
}
