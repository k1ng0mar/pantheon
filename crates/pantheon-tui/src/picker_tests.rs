//! Tests for the pickers.

use super::*;

#[test]
fn the_local_router_is_never_offered_as_a_provider() {
    // It exists so the dev loop and the e2e probe can reach a live model. It
    // is not part of the product, and a user picking a provider should never
    // see a localhost development target.
    assert!(is_dev_provider("router"));
    let items = provider_items();
    assert!(
        !items.iter().any(|i| i.value == "router"),
        "the router leaked into the provider picker"
    );
}

#[test]
fn the_provider_list_is_not_empty() {
    assert!(
        provider_items().len() > 10,
        "the catalog has dozens of providers; an empty picker means the filter ate them"
    );
}

#[test]
fn a_provider_with_no_curated_models_says_so_in_its_tag() {
    let items = provider_items();
    assert!(
        items.iter().any(|i| i.tag == "no curated models"),
        "expected at least one provider with an empty model list"
    );
}

#[test]
fn model_rows_carry_their_context_window() {
    let items = model_items("openai");
    assert!(!items.is_empty(), "openai has curated models");
    assert!(
        items.iter().any(|i| i.desc.contains("context")),
        "model rows must state the context window, because the compactor bounds on it"
    );
}

#[test]
fn a_model_row_flags_reasoning_when_the_catalog_says_so() {
    let items = model_items("anthropic");
    assert!(
        items.iter().any(|i| i.tag.contains("reasoning")),
        "a reasoning model must be visible as one before a user picks an effort for it"
    );
}

#[test]
fn an_unknown_provider_yields_no_models_rather_than_panicking() {
    assert!(model_items("definitely-not-a-provider").is_empty());
}

#[test]
fn a_provider_with_no_models_explains_itself() {
    // `nous` ships an empty model list in the catalog. The screen must say so
    // instead of rendering an empty box that looks broken.
    let s = model_screen("nous");
    assert!(
        !s.empty_reason.is_empty() && s.empty_reason != "(no matches)",
        "the empty state must name the reason, got {:?}",
        s.empty_reason
    );
}

#[test]
fn session_rows_prefer_a_title_and_fall_back_to_the_run_id() {
    let runs = vec![
        (
            "run_1700000000000_ab12cd34".into(),
            "completed".into(),
            0i64,
            Some("Banking".into()),
        ),
        (
            "run_1700000000001_zz99yy88".into(),
            "failed".into(),
            0i64,
            None,
        ),
    ];
    let s = session_screen(&runs);
    let items = s.list.items();
    assert_eq!(items[0].label, "Banking");
    assert!(
        !items[1].label.is_empty(),
        "an untitled run still needs a label"
    );
    assert!(items[1].desc.contains("failed"), "status is on the row");
}

#[test]
fn an_empty_ledger_says_no_runs_yet() {
    let s = session_screen(&[]);
    assert_eq!(s.empty_reason, "no runs yet");
}
