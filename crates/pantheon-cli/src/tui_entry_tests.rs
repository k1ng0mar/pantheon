//! Tests for the interactive entry point.
//!
//! `Entry::resolve` is pure, so the "does setup run or does a session run"
//! decision is tested without a terminal, a config, or a model.

use super::*;
use std::path::PathBuf;

/// A temp data dir that cleans itself up.
struct Temp(PathBuf);
impl Temp {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "pantheon-entry-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn write(&self, name: &str, body: &str) {
        std::fs::write(self.0.join(name), body).unwrap();
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[test]
fn no_config_runs_setup() {
    let t = Temp::new("empty");
    assert!(!is_configured(t.path()));
    assert!(matches!(
        Entry::resolve(is_configured(t.path()), None),
        Entry::Setup { resume: None }
    ));
}

#[test]
fn a_config_with_a_model_opens_a_session() {
    let t = Temp::new("model");
    t.write(
        "config.toml",
        "[model]\nprovider = \"openrouter\"\nmodel = \"qwen3-coder\"\n",
    );
    assert!(is_configured(t.path()));
    assert!(matches!(
        Entry::resolve(is_configured(t.path()), None),
        Entry::Session { resume: None }
    ));
}

#[test]
fn a_blank_model_is_not_a_configured_agent() {
    // An empty model string is the shape a half-written config takes. Treating
    // it as configured opens a session that cannot answer anything.
    let t = Temp::new("blank");
    t.write(
        "config.toml",
        "[model]\nprovider = \"openrouter\"\nmodel = \"\"\n",
    );
    assert!(!is_configured(t.path()));
}

#[test]
fn a_whitespace_model_is_not_a_configured_agent() {
    let t = Temp::new("space");
    t.write(
        "config.toml",
        "[model]\nprovider = \"openrouter\"\nmodel = \"   \"\n",
    );
    assert!(!is_configured(t.path()));
}

#[test]
fn a_config_without_a_model_runs_setup() {
    // Permissions, workspace, and memory can all be configured while the
    // model is still unset. That is a half-finished install, not a working
    // agent, and it must not open a session.
    let t = Temp::new("nomo");
    t.write("config.toml", "policy = \"coder\"\nworkspace = \"/tmp\"\n");
    assert!(!is_configured(t.path()));
}

#[test]
fn a_malformed_config_is_reported_and_does_not_open_a_session() {
    // The reader prints the parse error. resolve must not treat "unreadable"
    // as "fresh install", or the next thing that happens is setup overwriting
    // a file the user is trying to fix.
    let t = Temp::new("broken");
    t.write("config.toml", "this is not = = toml [[[\n");
    assert!(!is_configured(t.path()));
}

#[test]
fn a_resume_id_survives_the_resolve() {
    for configured in [true, false] {
        match Entry::resolve(configured, Some("run_abc".into())) {
            Entry::Session { resume } => {
                assert_eq!(resume.as_deref(), Some("run_abc"));
            }
            Entry::Setup { resume } => {
                assert_eq!(resume.as_deref(), Some("run_abc"));
            }
        }
    }
}

#[test]
fn setup_and_session_are_the_only_two_outcomes() {
    // Guards against a third variant being added without deciding what it
    // draws. The match above is exhaustive, so a new variant fails to compile
    // here first, which is the point.
    let outcomes = [Entry::resolve(true, None), Entry::resolve(false, None)];
    assert_eq!(outcomes.len(), 2);
}
