//! Profile resolution tests.
//!
//! These cover the semantics documented in `agent_profile.rs`: per-field
//! merge rules, origin provenance, and the four refusal cases (missing
//! parent, cycle, over-deep chain, namespace clash). A profile system whose
//! failure modes silently flatten are the exact defect class this repo has
//! been bitten by, so every rejection has a test that asserts the *specific*
//! error, not merely that an error happened.

use super::*;

fn prof() -> AgentProfile {
    AgentProfile::default()
}

fn with(mut p: AgentProfile, k: &str, v: &str) -> AgentProfile {
    match k {
        "display_name" => p.display_name = Some(v.into()),
        "soul_file" => p.soul_file = Some(v.into()),
        "agents_file" => p.agents_file = Some(v.into()),
        "inherits" => p.inherits = Some(v.into()),
        "memory_namespace" => p.memory_namespace = Some(v.into()),
        "policy" => p.policy = Some(v.into()),
        "model" => p.model = Some(v.into()),
        "provider" => p.provider = Some(v.into()),
        other => panic!("unknown key {other}"),
    }
    p
}

#[test]
fn resolve_without_a_registry_profile_is_refused_not_defaulted() {
    let reg = ProfileRegistry::new();
    // `resolve` on an undeclared name is an unknown-profile error, never a
    // silent fall back to the default profile.
    let err = reg.resolve("ghost", "coder").unwrap_err();
    assert_eq!(
        err,
        ProfileError::UnknownProfile {
            name: "ghost".into()
        }
    );
}

#[test]
fn a_standalone_profile_derives_agent_scoped_namespace() {
    let mut reg = ProfileRegistry::new();
    reg.insert("nyx", with(prof(), "display_name", "Nyx"))
        .unwrap();
    let eff = reg.resolve("nyx", "coder").unwrap();
    assert_eq!(eff.memory_namespace.value, "agent:nyx");
    // Derived, not authored: provenance must say so.
    assert!(matches!(
        eff.memory_namespace.origin,
        Origin::Runtime { .. }
    ));
    assert_eq!(eff.display_name.value, "Nyx");
    assert_eq!(eff.agent_id, "agent:nyx");
}

#[test]
fn child_overrides_parent_policy_and_records_the_origin() {
    let mut reg = ProfileRegistry::new();
    reg.insert("default", with(prof(), "policy", "reader"))
        .unwrap();
    reg.insert(
        "zeus",
        with(with(prof(), "inherits", "default"), "policy", "coder"),
    )
    .unwrap();
    let eff = reg.resolve("zeus", "coder").unwrap();
    assert_eq!(eff.policy.value, "coder");
    assert_eq!(
        eff.policy.origin,
        Origin::Own {
            profile: "zeus".into()
        }
    );
    assert_eq!(eff.parent.as_deref(), Some("default"));
}

#[test]
fn child_inherits_policy_when_it_declares_none_and_says_who_supplied_it() {
    let mut reg = ProfileRegistry::new();
    reg.insert("default", with(prof(), "policy", "reader"))
        .unwrap();
    reg.insert("athena", with(prof(), "inherits", "default"))
        .unwrap();
    let eff = reg.resolve("athena", "coder").unwrap();
    assert_eq!(eff.policy.value, "reader");
    assert_eq!(
        eff.policy.origin,
        Origin::Inherited {
            profile: "athena".into(),
            ancestor: "default".into()
        },
        "an inherited value must name the ancestor that supplied it"
    );
}

#[test]
fn instructions_layer_parent_first_and_name_the_contributor() {
    let mut reg = ProfileRegistry::new();
    reg.insert("default", with(prof(), "agents_file", "AGENTS.md"))
        .unwrap();
    reg.insert(
        "nyx",
        with(with(prof(), "inherits", "default"), "agents_file", "nyx.md"),
    )
    .unwrap();
    let eff = reg.resolve("nyx", "coder").unwrap();
    assert_eq!(
        eff.agents_files,
        vec![
            ("default".to_string(), "AGENTS.md".to_string()),
            ("nyx".to_string(), "nyx.md".to_string()),
        ],
        "AGENTS.md layers, so both must be present parent-first"
    );
    let read = |p: &str| match p {
        "AGENTS.md" => Some("base rules".to_string()),
        "nyx.md" => Some("extra rules".to_string()),
        _ => None,
    };
    let block = eff.instructions_block(&read);
    assert!(block.find("base rules").unwrap() < block.find("extra rules").unwrap());
    assert!(block.contains("instructions from profile: default"));
    assert!(block.contains("instructions from profile: nyx"));
}

#[test]
fn memory_namespace_is_never_inherited() {
    let mut reg = ProfileRegistry::new();
    reg.insert("default", with(prof(), "memory_namespace", "agent:shared"))
        .unwrap();
    reg.insert("zeus", with(prof(), "inherits", "default"))
        .unwrap();
    let eff = reg.resolve("zeus", "coder").unwrap();
    assert_eq!(
        eff.memory_namespace.value, "agent:zeus",
        "inheriting a namespace would merge one agent's memory into another"
    );
    assert!(matches!(
        eff.memory_namespace.origin,
        Origin::Runtime { .. }
    ));
}

#[test]
fn explicitly_claiming_an_ancestors_namespace_is_refused_by_name() {
    let mut reg = ProfileRegistry::new();
    reg.insert("default", with(prof(), "memory_namespace", "agent:shared"))
        .unwrap();
    reg.insert(
        "zeus",
        with(
            with(prof(), "inherits", "default"),
            "memory_namespace",
            "agent:shared",
        ),
    )
    .unwrap();
    // A child pointing at its parent's namespace is caught by the specific
    // error rather than the generic set-level clash, because naming the
    // offending inheritance is what tells an operator which line to delete.
    let err = reg.resolve("zeus", "coder").unwrap_err();
    assert_eq!(
        err,
        ProfileError::InheritedNamespace {
            profile: "zeus".into(),
            namespace: "agent:shared".into()
        }
    );
    assert!(
        err.to_string().contains("never inherited"),
        "the message must state the rule, not just the collision: {err}"
    );
}

#[test]
fn two_unrelated_profiles_sharing_a_namespace_is_a_clash() {
    let mut reg = ProfileRegistry::new();
    // Neither inherits the other, so the per-profile check cannot fire and
    // the set-level clash check is the only thing that catches this.
    reg.insert("zeus", with(prof(), "memory_namespace", "agent:shared"))
        .unwrap();
    reg.insert("athena", with(prof(), "memory_namespace", "agent:shared"))
        .unwrap();
    let err = reg.validate_all("coder").unwrap_err();
    match err {
        ProfileError::NamespaceClash { a, b, namespace } => {
            assert_eq!(namespace, "agent:shared");
            assert_ne!(a, b, "both sides of a clash must be named");
        }
        other => panic!("expected a namespace clash, got {other:?}"),
    }
}

#[test]
fn a_child_claiming_the_parents_derived_namespace_is_a_clash() {
    let mut reg = ProfileRegistry::new();
    // `default` declares no namespace, so it derives `agent:default`.
    reg.insert("default", prof()).unwrap();
    // `zeus` claims that derived value: legal per-profile (it is not the
    // same literal string the parent wrote), but a data leak in effect.
    reg.insert(
        "zeus",
        with(
            with(prof(), "inherits", "default"),
            "memory_namespace",
            "agent:default",
        ),
    )
    .unwrap();
    let err = reg.validate_all("coder").unwrap_err();
    assert!(
        matches!(err, ProfileError::NamespaceClash { .. }),
        "claiming the parent's derived namespace must still be refused, got {err:?}"
    );
}

#[test]
fn missing_parent_is_a_named_error() {
    let mut reg = ProfileRegistry::new();
    reg.insert("nyx", with(prof(), "inherits", "nosuch"))
        .unwrap();
    let err = reg.resolve("nyx", "coder").unwrap_err();
    assert_eq!(
        err,
        ProfileError::MissingParent {
            profile: "nyx".into(),
            parent: "nosuch".into()
        }
    );
}

#[test]
fn circular_inheritance_is_refused_at_insert_time() {
    let mut reg = ProfileRegistry::new();
    reg.insert("a", with(prof(), "inherits", "b")).unwrap();
    let err = reg.insert("b", with(prof(), "inherits", "a")).unwrap_err();
    match err {
        ProfileError::Cycle { chain } => {
            assert!(chain.len() >= 2, "cycle must name the path, got {chain:?}");
        }
        other => panic!("expected a cycle, got {other:?}"),
    }
}

#[test]
fn self_inheritance_is_a_cycle() {
    let mut reg = ProfileRegistry::new();
    let err = reg.insert("a", with(prof(), "inherits", "a")).unwrap_err();
    assert!(matches!(err, ProfileError::Cycle { .. }), "got {err:?}");
}

#[test]
fn an_over_deep_chain_is_refused_rather_than_walked() {
    let mut reg = ProfileRegistry::new();
    reg.insert("p0", prof()).unwrap();
    for i in 1..(MAX_INHERIT_DEPTH + 3) {
        reg.insert(
            &format!("p{i}"),
            with(prof(), "inherits", &format!("p{}", i - 1)),
        )
        .unwrap();
    }
    let deepest = format!("p{}", MAX_INHERIT_DEPTH + 2);
    let err = reg.resolve(&deepest, "coder").unwrap_err();
    assert!(matches!(err, ProfileError::TooDeep { .. }), "got {err:?}");
}

#[test]
fn invalid_names_are_refused_at_insert_time() {
    let mut reg = ProfileRegistry::new();
    for bad in ["", "not a slug", "has/slash", "sp ace"] {
        let err = reg.insert(bad, prof()).unwrap_err();
        assert!(
            matches!(err, ProfileError::InvalidName { .. }),
            "expected {bad:?} to be refused, got {err:?}"
        );
    }
}

#[test]
fn unknown_policy_is_refused_at_insert_time() {
    let mut reg = ProfileRegistry::new();
    let err = reg
        .insert("zeus", with(prof(), "policy", "overlord"))
        .unwrap_err();
    assert_eq!(
        err,
        ProfileError::UnknownPolicy {
            profile: "zeus".into(),
            policy: "overlord".into()
        }
    );
}

#[test]
fn a_deep_but_legal_chain_resolves() {
    let mut reg = ProfileRegistry::new();
    reg.insert("p0", with(prof(), "policy", "reader")).unwrap();
    for i in 1..MAX_INHERIT_DEPTH {
        reg.insert(
            &format!("p{i}"),
            with(prof(), "inherits", &format!("p{}", i - 1)),
        )
        .unwrap();
    }
    let deepest = format!("p{}", MAX_INHERIT_DEPTH - 1);
    let eff = reg.resolve(&deepest, "coder").unwrap();
    assert_eq!(
        eff.policy.value, "reader",
        "the root's policy must still reach the leaf"
    );
    assert_eq!(
        eff.policy.origin,
        Origin::Inherited {
            profile: deepest.clone(),
            ancestor: "p0".into()
        }
    );
}

#[test]
fn three_generation_chain_accumulates_instructions_in_order() {
    let mut reg = ProfileRegistry::new();
    reg.insert("default", with(prof(), "agents_file", "base.md"))
        .unwrap();
    reg.insert(
        "nyx",
        with(with(prof(), "inherits", "default"), "agents_file", "nyx.md"),
    )
    .unwrap();
    reg.insert(
        "zeus",
        with(with(prof(), "inherits", "nyx"), "agents_file", "zeus.md"),
    )
    .unwrap();
    let eff = reg.resolve("zeus", "coder").unwrap();
    let names: Vec<&str> = eff.agents_files.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(names, vec!["default", "nyx", "zeus"]);
}

#[test]
fn personality_and_model_pins_override_and_prove_their_origin() {
    let mut reg = ProfileRegistry::new();
    reg.insert(
        "default",
        with(
            with(prof(), "soul_file", "default-soul.md"),
            "policy",
            "reader",
        ),
    )
    .unwrap();
    reg.insert(
        "nyx",
        with(
            with(prof(), "inherits", "default"),
            "soul_file",
            "nyx-soul.md",
        ),
    )
    .unwrap();
    let eff = reg.resolve("nyx", "coder").unwrap();
    assert_eq!(eff.soul_file.value.as_deref(), Some("nyx-soul.md"));
    assert_eq!(eff.soul_file.supplied_by(), Some("nyx"));
    // Policy was inherited, persona overridden: the two coexist.
    assert_eq!(eff.policy.value, "reader");
    assert_eq!(eff.policy.supplied_by(), Some("default"));
}

#[test]
fn model_pin_is_inherited_but_a_profile_may_not_route() {
    let mut reg = ProfileRegistry::new();
    reg.insert(
        "default",
        with(with(prof(), "model", "big"), "policy", "coder"),
    )
    .unwrap();
    reg.insert("nyx", with(prof(), "inherits", "default"))
        .unwrap();
    let eff = reg.resolve("nyx", "coder").unwrap();
    assert_eq!(eff.model.value.as_deref(), Some("big"));
    // An unpinned model resolves to None, meaning "use the runtime
    // default" — never "the agent chose one".
    let other = reg.resolve("default", "coder").unwrap();
    assert_eq!(other.model.value.as_deref(), Some("big"));
    let mut bare = ProfileRegistry::new();
    bare.insert("solo", prof()).unwrap();
    assert_eq!(bare.resolve("solo", "coder").unwrap().model.value, None);
}

#[test]
fn a_missing_instruction_file_is_reported_not_fatal() {
    let mut reg = ProfileRegistry::new();
    reg.insert("nyx", with(prof(), "agents_file", "gone.md"))
        .unwrap();
    let eff = reg.resolve("nyx", "coder").unwrap();
    let block = eff.instructions_block(&|_| None);
    assert!(block.contains("could not be read"), "got: {block}");
    assert!(block.contains("gone.md"), "the path must be named: {block}");
}

#[test]
fn identity_line_names_the_parent() {
    let mut reg = ProfileRegistry::new();
    reg.insert("default", prof()).unwrap();
    reg.insert("nyx", with(prof(), "inherits", "default"))
        .unwrap();
    assert_eq!(
        reg.resolve("nyx", "coder").unwrap().identity(),
        "nyx (inherits default)"
    );
    assert_eq!(
        reg.resolve("default", "coder").unwrap().identity(),
        "default"
    );
}

#[test]
fn validate_all_accepts_a_healthy_fleet() {
    let mut reg = ProfileRegistry::new();
    reg.insert("default", with(prof(), "policy", "coder_memory"))
        .unwrap();
    for (name, parent) in [("nyx", "default"), ("zeus", "default"), ("athena", "nyx")] {
        reg.insert(
            name,
            with(with(prof(), "display_name", name), "inherits", parent),
        )
        .unwrap();
    }
    reg.validate_all("coder").unwrap();
    assert_eq!(reg.names(), vec!["athena", "default", "nyx", "zeus"]);
    assert_eq!(reg.len(), 4);
}

#[test]
fn registry_clone_and_supplied_by_survive_serialization() {
    let mut reg = ProfileRegistry::new();
    reg.insert("nyx", with(prof(), "display_name", "Nyx"))
        .unwrap();
    let eff = reg.resolve("nyx", "coder").unwrap();
    let json = serde_json::to_string(&eff).unwrap();
    let back: EffectiveProfile = serde_json::from_str(&json).unwrap();
    assert_eq!(
        back, eff,
        "an effective profile must round-trip through the ledger"
    );
    assert_eq!(back.memory_namespace.supplied_by(), None);
}
