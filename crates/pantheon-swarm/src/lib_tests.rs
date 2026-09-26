//! Tests for `pantheon_swarm::tests` — sibling file so sources stay test-free.
use super::*;

#[test]
fn depth_cap_blocks_level_two() {
    let mut s = Swarm::new(Caps {
        max_depth: 1,
        max_concurrent: 8,
        max_total_agents: 16,
        ..Caps::default()
    });
    assert!(s.spawn("researcher", 0, "sonnet").is_ok());
    let err = s.spawn("source-hunter", 1, "sonnet").unwrap_err();
    assert_eq!(err.code, "SWARM_MAX_DEPTH");
}

#[test]
fn concurrency_cap_blocks_extra_agents() {
    let mut s = Swarm::new(Caps {
        max_concurrent: 2,
        max_depth: 5,
        max_total_agents: 16,
        ..Caps::default()
    });
    s.spawn("a", 0, "sonnet").unwrap();
    s.spawn("b", 0, "sonnet").unwrap();
    let err = s.spawn("c", 0, "sonnet").unwrap_err();
    assert_eq!(err.code, "SWARM_MAX_CONCURRENT");
}

#[test]
fn budget_cap_fires_once_usage_is_folded_in() {
    let mut s = Swarm::new(Caps {
        token_budget: 1_000,
        max_total_agents: 16,
        ..Caps::default()
    });
    s.spawn("a", 0, "sonnet").unwrap();
    s.complete("a", 1_000, 0, 0);
    let err = s.spawn("b", 0, "sonnet").unwrap_err();
    assert_eq!(err.code, "SWARM_TOKEN_BUDGET");
}

#[test]
fn model_restriction_is_enforced() {
    let mut s = Swarm::new(Caps {
        allowed_models: vec!["kimi".into()],
        ..Caps::default()
    });
    let err = s.spawn("a", 0, "sonnet").unwrap_err();
    assert_eq!(err.code, "SWARM_MODEL_NOT_ALLOWED");
    assert!(s.spawn("b", 0, "kimi").is_ok());
}

#[test]
fn complete_frees_a_slot() {
    let mut s = Swarm::new(Caps {
        max_concurrent: 1,
        max_total_agents: 4,
        max_depth: 4,
        ..Caps::default()
    });
    s.spawn("a", 0, "sonnet").unwrap();
    assert!(s.spawn("b", 0, "sonnet").is_err());
    s.complete("a", 0, 0, 0);
    assert!(s.spawn("c", 0, "sonnet").is_ok());
}
