//! Live streaming integration tests against the local llm-router
//! (`127.0.0.1:8015`, OpenAI-compatible, Bearer auth via `router_key`).
//!
//! Contract pinned here:
//! - streaming turns emit `Attempt{streaming:true}` → `TextDelta`+ →
//!   `Usage` → `Completed`, in that order, with concatenated deltas
//!   equal to the returned text;
//! - single-shot turns emit one full-text delta plus the same bookends;
//! - the router models resolve through the catalog (context limits,
//!   streaming capability) rather than hard-coded provider metadata.
//!
//! Auth: set `PANTHEON_KEY_ROUTER`; otherwise the test skips so `cargo
//! test` stays green on machines without the local router.

use pantheon_core::message::Message;
use pantheon_core::model::{DefaultModel, FallbackChain, ModelPolicy};
use pantheon_core::model_event::{ModelEvent, ModelEventSink, ModelUsage};
use pantheon_providers::{HttpTransport, ProviderChain};
use std::cell::RefCell;
use std::time::Duration;

const ROUTER_BASE: &str = "http://127.0.0.1:8015/v1";

struct Collect(RefCell<Vec<ModelEvent>>);
impl ModelEventSink for Collect {
    fn emit(&self, event: ModelEvent) {
        self.0.borrow_mut().push(event);
    }
}

/// Chain pointed at the local router, or `None` when no key is configured.
fn router_chain() -> Option<ProviderChain<HttpTransport>> {
    let key = std::env::var("PANTHEON_KEY_ROUTER").ok()?;
    let pol = ModelPolicy {
        default: DefaultModel {
            provider: "router".into(),
            model: "chat".into(),
        },
        fallbacks: FallbackChain::default(),
        auxiliaries: vec![],
    };
    Some(ProviderChain::new(
        pol,
        HttpTransport {
            timeout: Duration::from_secs(60),
        },
        vec![],
        key,
    ))
}

fn require_router() -> Option<ProviderChain<HttpTransport>> {
    match router_chain() {
        Some(c) => Some(c),
        None => {
            eprintln!("skipping: PANTHEON_KEY_ROUTER not set (local router test)");
            None
        }
    }
}

fn text_deltas(evs: &[ModelEvent]) -> String {
    evs.iter()
        .filter_map(|e| match e {
            ModelEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn streaming_turn_emits_ordered_normalized_events() {
    let Some(chain) = require_router() else {
        return;
    };
    let c = Collect(RefCell::new(vec![]));
    let out = chain
        .turn_stream(&[Message::user("Reply with exactly: pong")], &c)
        .expect("streaming turn against local router");
    let evs = c.0.borrow();

    // Bookends: Attempt (streaming, default index) first, Completed last.
    match &evs[0] {
        ModelEvent::Attempt {
            provider,
            model,
            chain_index,
            streaming,
        } => {
            assert_eq!(provider, "router");
            assert_eq!(model, "chat");
            assert_eq!(*chain_index, 0);
            assert!(*streaming, "router/chat must be cataloged as streaming");
        }
        other => panic!("first event must be Attempt, got {other:?}"),
    }
    assert!(
        matches!(evs.last(), Some(ModelEvent::Completed { .. })),
        "last event must be Completed, got {:?}",
        evs.last()
    );

    // At least one TextDelta strictly before Completed.
    let delta_at = evs
        .iter()
        .position(|e| matches!(e, ModelEvent::TextDelta { .. }))
        .expect("streaming turn must emit at least one TextDelta");
    let completed_at = evs.len() - 1;
    assert!(delta_at < completed_at);

    // Concatenated deltas equal the returned text.
    let text = match &out {
        pantheon_agent::TurnOutcome::Text(t) => t.clone(),
        other => panic!("expected text outcome, got {other:?}"),
    };
    assert_eq!(text_deltas(&evs), text);
    assert!(!text.is_empty(), "router should return a non-empty reply");

    // Usage arrives before Completed; cost stays None (pool pricing unknown).
    let usage_at = evs
        .iter()
        .position(|e| matches!(e, ModelEvent::Usage { .. }));
    match usage_at {
        Some(u) if u < completed_at => {
            let cost = match &evs[u] {
                ModelEvent::Usage {
                    usage: ModelUsage { cost_usd, .. },
                } => *cost_usd,
                _ => unreachable!(),
            };
            assert!(cost.is_none(), "router pool models have no catalog price");
        }
        Some(u) => panic!("Usage must precede Completed (at {u}, completed {completed_at})"),
        None => panic!("streaming turn must emit Usage"),
    }

    // Catalog context limit is represented for the pool model.
    let meta = pantheon_core::catalog::model_meta("router", "chat");
    assert_eq!(meta.context_limit, Some(256_000));
    assert!(meta.streaming && meta.tools && !meta.vision);
}

#[test]
fn single_shot_turn_emits_one_full_text_delta() {
    let Some(chain) = require_router() else {
        return;
    };
    let c = Collect(RefCell::new(vec![]));
    let out = chain
        .turn_with_sink(&[Message::user("Reply with exactly: pong")], &c)
        .expect("single-shot turn against local router");
    let evs = c.0.borrow();

    assert!(matches!(
        &evs[0],
        ModelEvent::Attempt {
            streaming: false,
            ..
        }
    ));
    let deltas: Vec<&str> = evs
        .iter()
        .filter_map(|e| match e {
            ModelEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas.len(), 1, "single-shot must emit exactly one delta");
    match &out {
        pantheon_agent::TurnOutcome::Text(t) => assert_eq!(deltas[0], t.as_str()),
        other => panic!("expected text outcome, got {other:?}"),
    }
    assert!(matches!(evs.last(), Some(ModelEvent::Completed { .. })));
}

#[test]
fn missing_key_skips_instead_of_failing() {
    // Guard for CI machines: without PANTHEON_KEY_ROUTER the helper returns
    // None (the two tests above then skip), never a hard failure.
    if std::env::var("PANTHEON_KEY_ROUTER").is_err() {
        assert!(router_chain().is_none());
    } else {
        assert!(router_chain().is_some());
    }
}
