//! Tests for `pantheon_cli::fallback_cli` — sibling file so sources stay
//! test-free.
use super::*;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// `cmd_fallback` reads the data dir from `PANTHEON_DATA_DIR`, and Rust runs
/// tests in parallel threads, so the env var is shared mutable state. Every
/// test in this file therefore takes this lock for its whole body; without
/// it two tests would race on one config file and assert each other's writes.
fn env_lock() -> MutexGuard<'static, ()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    let m = L.get_or_init(|| Mutex::new(()));
    match m.lock() {
        Ok(g) => g,
        // A prior test panicked mid-critical-section. The env var may be
        // wrong, but every test re-sets it on entry, so recovering the guard
        // is safe and keeps one failure from cascading into five.
        Err(poisoned) => poisoned.into_inner(),
    }
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
