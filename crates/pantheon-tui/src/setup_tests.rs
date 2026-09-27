//! Tests for the setup section graph and the provisioner contract.

use super::*;

fn full() -> Answers {
    Answers {
        mode: Some(Mode::Full),
        execution_is_docker: true,
        browser_enabled: true,
        web_search_enabled: true,
        tts_enabled: true,
        tools_enabled: true,
        memory_enabled: true,
        gateways_enabled: true,
        extensions_enabled: true,
        fallback_enabled: true,
        model_supports_reasoning: true,
    }
}

#[test]
fn blank_slate_configures_no_agent() {
    let list = sections(&Answers {
        mode: Some(Mode::Blank),
        ..Default::default()
    });
    for absent in [
        Section::Provider,
        Section::Model,
        Section::Permissions,
        Section::Tools,
    ] {
        assert!(
            !list.contains(&absent),
            "{:?} must not appear in blank slate",
            absent
        );
    }
    assert!(list.contains(&Section::Done));
}

#[test]
fn quick_is_shorter_than_full() {
    let quick = sections(&Answers {
        mode: Some(Mode::Quick),
        ..Default::default()
    });
    let full = sections(&full());
    assert!(
        quick.len() < full.len(),
        "quick had {} sections, full had {}",
        quick.len(),
        full.len()
    );
}

#[test]
fn docker_adds_exactly_one_screen() {
    let a = Answers {
        mode: Some(Mode::Full),
        tools_enabled: true,
        memory_enabled: true,
        ..Default::default()
    };
    let mut docker = a.clone();
    docker.execution_is_docker = true;
    let with = sections(&docker);
    let without = sections(&a);
    assert_eq!(
        with.len(),
        without.len() + 1,
        "the network screen appears only under docker"
    );
    assert!(with.contains(&Section::DockerNetwork));
}

#[test]
fn the_network_screen_comes_after_execution() {
    let list = sections(&full());
    let e = list.iter().position(|s| *s == Section::Execution).unwrap();
    let n = list
        .iter()
        .position(|s| *s == Section::DockerNetwork)
        .unwrap();
    assert!(
        e < n,
        "network policy cannot precede the backend it restricts"
    );
}

#[test]
fn a_model_without_reasoning_gets_no_reasoning_screen() {
    let a = Answers {
        mode: Some(Mode::Full),
        model_supports_reasoning: false,
        ..Default::default()
    };
    assert!(
        !sections(&a).contains(&Section::Reasoning),
        "offering an effort for a model that cannot use it is fake configuration"
    );
}

#[test]
fn a_tool_group_that_is_off_removes_its_provider_screen() {
    let a = Answers {
        mode: Some(Mode::Full),
        tools_enabled: true,
        browser_enabled: false,
        ..Default::default()
    };
    let list = sections(&a);
    assert!(!list.contains(&Section::Browser));
    // The screen must be gone, not just empty.
    assert!(
        list.contains(&Section::Tools),
        "the tools screen itself stays"
    );
}

#[test]
fn the_count_always_matches_the_resolved_branch() {
    // The spec's rule: never print a hardcoded "7 of 12". Every pair of
    // answers must produce a self-consistent count.
    for a in [
        full(),
        Answers {
            mode: Some(Mode::Blank),
            ..Default::default()
        },
        Answers {
            mode: Some(Mode::Quick),
            ..Default::default()
        },
        Answers {
            mode: Some(Mode::Full),
            ..Default::default()
        },
    ] {
        let list = sections(&a);
        for s in &list {
            let p = progress(*s, &list);
            let total = p.rsplit(" of ").next().unwrap();
            assert_eq!(
                total,
                list.len().to_string(),
                "progress lied: {p} for a branch of {}",
                list.len()
            );
        }
    }
}

#[test]
fn the_section_indicator_names_the_section() {
    assert_eq!(indicator(Section::Tools), "SETUP · TOOLS");
    assert_eq!(indicator(Section::DockerNetwork), "SETUP · NETWORK");
}

#[test]
fn review_provision_and_done_are_always_last() {
    let list = sections(&full());
    let tail: Vec<_> = list.iter().rev().take(3).rev().copied().collect();
    assert_eq!(
        tail,
        vec![Section::Review, Section::Provision, Section::Done],
        "every branch ends the same way"
    );
}

#[test]
fn a_skipped_step_does_not_abort_provisioning() {
    let steps = vec![
        ("profile".to_string(), StepOutcome::Done),
        (
            "browser".to_string(),
            StepOutcome::Skipped {
                why: "no provider selected".into(),
            },
        ),
    ];
    assert!(
        provisioning_ok(&steps),
        "an agent without a browser is still a usable agent"
    );
}

#[test]
fn a_retryable_failure_does_not_abort_either() {
    let steps = vec![(
        "key".to_string(),
        StepOutcome::Failed {
            why: "401".into(),
            action: StepAction::Retry,
        },
    )];
    assert!(
        provisioning_ok(&steps),
        "retry means the user chose to continue"
    );
}

#[test]
fn an_abort_is_the_only_failure_that_stops_the_run() {
    let steps = vec![(
        "workspace".to_string(),
        StepOutcome::Failed {
            why: "path is not writable".into(),
            action: StepAction::Abort,
        },
    )];
    assert!(!provisioning_ok(&steps));
}

#[test]
fn a_skipped_step_keeps_its_reason_for_doctor_to_report() {
    // The record is what stops a skipped capability from being invisible: a
    // later health check reads it and says the browser is not configured.
    let mut rec: ProvisionRecord = BTreeMap::new();
    rec.insert(
        "browser".to_string(),
        StepOutcome::Skipped {
            why: "no provider selected".into(),
        },
    );
    let outcome = rec.get("browser").unwrap();
    assert!(matches!(outcome, StepOutcome::Skipped { .. }));
    assert!(rec.get("memory").is_none(), "only real steps are recorded");
}
