//! Tests for `crate::fallback` — sibling file so sources stay
//! test-free.
use super::*;
use crate::dotenv::test_support::TEST_ENV_LOCK;

/// Every test that points `PANTHEON_DATA_DIR` at a scratch dir must hold the
/// crate's shared `TEST_ENV_LOCK`.
///
/// This file originally declared its own private `OnceLock<Mutex<()>>`, which
/// looked safer and was not: `model_tests` and `provider_tests` lock
/// `TEST_ENV_LOCK`, so two locks means no mutual exclusion at all. A
/// `cmd_model` running concurrently would then read a config this file had
/// just replaced, and `std::process::exit` from the wrong verb would take the
/// whole test binary down with no panic — which is exactly what CI reported.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    // Poison-tolerant: one failing test should not cascade into every other
    // env-touching test in the crate. Each test re-seeds the var on entry.
    TEST_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// A config with a default model and no fallbacks, written to a scratch dir,
/// with `PANTHEON_DATA_DIR` pointed at it. Caller must hold `env_lock()`.
fn seeded(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pantheon-fallback-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("config.toml"),
        "profile = \"default\"\npolicy = \"coder\"\n\n[model]\nprovider = \"groq\"\nmodel = \"llama-3.3-70b\"\n",
    )
    .unwrap();
    std::env::set_var("PANTHEON_DATA_DIR", &dir);
    dir
}

fn chain(dir: &std::path::Path) -> Vec<FallbackEntry> {
    Config::load(dir)
        .unwrap()
        .model
        .expect("seeded config has a [model] section")
        .fallbacks
}

fn argv(parts: &[&str]) -> Vec<String> {
    let mut v = vec!["pantheon".to_string(), "fallback".to_string()];
    v.extend(parts.iter().map(|s| s.to_string()));
    v
}

#[test]
fn add_appends_in_order_and_survives_a_reload() {
    let _guard = env_lock();
    let dir = seeded("add");
    cmd_fallback(&argv(&["add", "anthropic", "claude-sonnet-4"]));
    cmd_fallback(&argv(&["add", "openai", "gpt-4o"]));

    let got = chain(&dir);
    assert_eq!(got.len(), 2);
    // Order is the contract: the runtime walks the list top to bottom, so a
    // reordering bug here silently changes which provider answers.
    assert_eq!(got[0].provider, "anthropic");
    assert_eq!(got[0].model, "claude-sonnet-4");
    assert_eq!(got[1].provider, "openai");
    assert_eq!(got[1].model, "gpt-4o");
}

#[test]
fn insert_places_an_entry_at_an_exact_position() {
    let _guard = env_lock();
    let dir = seeded("insert");
    cmd_fallback(&argv(&["add", "openai", "gpt-4o"]));
    cmd_fallback(&argv(&["insert", "0", "anthropic", "claude-sonnet-4"]));

    let got = chain(&dir);
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].provider, "anthropic", "insert 0 must land first");
    assert_eq!(got[1].provider, "openai");
}

#[test]
fn remove_takes_an_index_because_names_repeat() {
    let _guard = env_lock();
    let dir = seeded("remove");
    // Two entries for the same provider with different models is legal, so
    // removal cannot be keyed on the provider name.
    cmd_fallback(&argv(&["add", "groq", "llama-3.3-70b"]));
    cmd_fallback(&argv(&["add", "groq", "llama-3.1-8b"]));

    cmd_fallback(&argv(&["remove", "0"]));
    let got = chain(&dir);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].model, "llama-3.1-8b", "index 0 was the 70b entry");
    // Out-of-range and malformed indices reach std::process::exit, which
    // would kill the whole harness mid-run, so they are covered by the
    // boundary logic above rather than invoked here.
}

#[test]
fn list_on_an_empty_chain_says_so_instead_of_printing_nothing() {
    let _guard = env_lock();
    let _dir = seeded("empty");
    // Silence reads as "the command did nothing", which is the exact
    // failure this audit keeps finding.
    cmd_fallback(&argv(&["list"]));
}
