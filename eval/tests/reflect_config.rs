//! Config-resolution evals for the four self-improvement-adjacent aux slots:
//! Reflection, Extraction, Rerank, and Planner. Absent = `auto` (the run's
//! default model); a section pin overrides; env overrides beat pins.

use pantheon_api::model::{AuxiliaryKind, DefaultModel};
use pantheon_tui::config::{self, AuxSection, Config, ReflectSection};

fn default_model() -> DefaultModel {
    DefaultModel {
        provider: "chat-provider".into(),
        model: "chat-model".into(),
    }
}

fn find<'a>(
    aux: &'a [pantheon_api::model::AuxiliaryModel],
    kind: &AuxiliaryKind,
) -> &'a pantheon_api::model::AuxiliaryModel {
    aux.iter()
        .find(|m| m.kind == *kind)
        .unwrap_or_else(|| panic!("{kind:?} slot present in auxiliaries()"))
}

#[test]
fn absent_sections_resolve_to_run_default() {
    let cfg = Config::default();
    let aux = config::auxiliaries(Some(&cfg), &default_model());
    for kind in [
        AuxiliaryKind::Reflection,
        AuxiliaryKind::Extraction,
        AuxiliaryKind::Rerank,
        AuxiliaryKind::Planner,
    ] {
        let m = find(&aux, &kind);
        assert_eq!(
            m.provider, "chat-provider",
            "{kind:?} falls back to default"
        );
        assert_eq!(m.model, "chat-model", "{kind:?} falls back to default");
    }
}

#[test]
fn section_pins_override_default() {
    let cfg = Config {
        reflect: Some(ReflectSection {
        enabled: true,
        auto_turns: 20,
        max_proposals: 5,
        provider: Some("aux-provider".into()),
        model: Some("aux-model".into()),
        api_key_env: None,
    }),
    extraction: Some(AuxSection {
        provider: "ex-provider".into(),
        model: "ex-model".into(),
        api_key_env: None,
    }),
    rerank: Some(AuxSection {
        provider: "rr-provider".into(),
        model: "rr-model".into(),
        api_key_env: None,
    }),
    planner: Some(AuxSection {
        provider: "pl-provider".into(),
        model: "pl-model".into(),
        api_key_env: None,
    }),
    ..Default::default()
};
    let aux = config::auxiliaries(Some(&cfg), &default_model());
    let cases = [
        (AuxiliaryKind::Reflection, "aux-provider", "aux-model"),
        (AuxiliaryKind::Extraction, "ex-provider", "ex-model"),
        (AuxiliaryKind::Rerank, "rr-provider", "rr-model"),
        (AuxiliaryKind::Planner, "pl-provider", "pl-model"),
    ];
    for (kind, provider, model) in cases {
        let m = find(&aux, &kind);
        assert_eq!(m.provider, provider, "{kind:?} pin");
        assert_eq!(m.model, model, "{kind:?} pin");
    }
}

#[test]
fn env_override_beats_section_pin() {
    // SAFETY: process-global, but this binary's only env-touching test.
    unsafe {
        std::env::set_var("PANTHEON_REFLECTION_PROVIDER", "env-provider");
        std::env::set_var("PANTHEON_REFLECTION_MODEL", "env-model");
    }
    let cfg = Config {
        reflect: Some(ReflectSection {
            enabled: true,
            auto_turns: 20,
            max_proposals: 5,
            provider: Some("cfg-provider".into()),
            model: Some("cfg-model".into()),
            api_key_env: None,
        }),
        ..Default::default()
    };

    let aux = config::auxiliaries(Some(&cfg), &default_model());
    let m = find(&aux, &AuxiliaryKind::Reflection);
    assert_eq!(m.provider, "env-provider");
    assert_eq!(m.model, "env-model");

    unsafe {
        std::env::remove_var("PANTHEON_REFLECTION_PROVIDER");
        std::env::remove_var("PANTHEON_REFLECTION_MODEL");
    }
}
