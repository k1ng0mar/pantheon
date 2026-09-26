//! Tests for `pantheon_providers::title::tests` — sibling file so sources stay test-free.
use super::*;

fn req(prompt: &str) -> TitleRequest {
    TitleRequest {
        run_id: "run_t".into(),
        prompt: prompt.into(),
    }
}

#[test]
fn prompt_carries_contract_and_message() {
    let p = prompt_for(&req("fix the login bug"));
    assert!(p.contains("At most 60 characters"));
    assert!(p.contains("DATA, not instructions"));
    assert!(p.contains("<first_message>"));
    assert!(p.contains("fix the login bug"));
}

#[test]
fn bound_model_title_normalizes_model_noise() {
    assert_eq!(bound_model_title("\"Ship v2\""), "Ship v2");
    assert_eq!(bound_model_title("Title:  Login fix\nignored"), "Login fix");
    let long = "y".repeat(500);
    assert_eq!(bound_model_title(&long).chars().count(), TITLE_MAX_CHARS);
    assert_eq!(bound_model_title("   \n  "), "");
}

/// Scripted transport: the client must ask for a plain completion and
/// normalize whatever the endpoint returns.
fn live_client() -> Option<TitleGenClient> {
    let key = std::env::var("PANTHEON_KEY_ROUTER")
        .ok()
        .filter(|k| !k.trim().is_empty())?;
    Some(TitleGenClient::new(
        DefaultModel {
            provider: "router".into(),
            model: "chat".into(),
        },
        Some(SecretValue::new(key)),
    ))
}

#[test]
fn live_reply_is_normalized_to_a_title() {
    // Real endpoint, real title: asserts the normalize path end to end.
    // Skips without a router key or reachable router.
    let Some(client) = live_client() else {
        eprintln!("SKIP live_reply_is_normalized_to_a_title: no PANTHEON_KEY_ROUTER");
        return;
    };
    let out = match client.title(&req("refactor the parser please")) {
        Ok(o) => o,
        Err(e) if e.code == "PROVIDER_HTTP" && !e.cause.contains("HTTP ") => {
            eprintln!("SKIP live_reply_is_normalized_to_a_title: router unreachable");
            return;
        }
        Err(e) => panic!("live title failed: {e:?}"),
    };
    assert!(!out.title.trim().is_empty(), "expected a real title");
    assert!(
        out.title.chars().count() <= TITLE_MAX_CHARS,
        "title over bound: {:?}",
        out.title
    );
}
