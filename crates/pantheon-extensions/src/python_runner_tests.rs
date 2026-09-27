//! Tests for the Python runner's subprocess environment. The runner spawns
//! third-party plugin code, so the child must never inherit the host's
//! ambient environment (API keys, `PANTHEON_SECRET_*`, session tokens).
//! These execute a real interpreter, so they skip cleanly when `python3`
//! is not installed rather than failing for an environmental reason.
use super::*;
use crate::hooks::Hook;
use std::path::PathBuf;

fn python3() -> Option<String> {
    std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .ok()
        .filter(|s| s.success())
        .map(|_| "python3".to_string())
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pantheon-pyrun-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A plugin whose `pre_llm_call` handler reports what the child process
/// can see of two host variables: a sentinel secret and PATH.
fn env_probe_plugin(dir: &std::path::Path) -> PythonPlugin {
    std::fs::write(
        dir.join("plugin.yaml"),
        "name: env-probe\nprovides_hooks: [pre_llm_call]\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("__init__.py"),
        concat!(
            "import os\n",
            "def register(ctx):\n",
            "    def probe(**kw):\n",
            "        return {\"context\": \"secret=\" + os.environ.get(\"PANTHEON_EXT_TEST_SECRET\", \"<unset>\")\n",
            "                    + \" path_set=\" + str(\"PATH\" in os.environ)}\n",
            "    ctx.register_hook(\"pre_llm_call\", probe)\n",
        ),
    )
    .unwrap();
    PythonPlugin::load(dir).unwrap()
}

fn input() -> HookInput {
    HookInput {
        hook: "pre_llm_call".into(),
        session_id: "s1".into(),
        platform: "cli".into(),
        extra: Default::default(),
    }
}

/// The plugin child must NOT see host env vars, but MUST still resolve
/// `python3` via the restored PATH (spawn itself proves that).
#[test]
fn plugin_child_does_not_inherit_host_env() {
    let Some(python) = python3() else { return };
    let dir = tmp("no-inherit");
    let plugin = env_probe_plugin(&dir);
    std::env::set_var("PANTHEON_EXT_TEST_SECRET", "must-not-leak");
    let cfg = RunnerConfig {
        python,
        timeout: Duration::from_secs(15),
    };
    let out = fire_hook(&plugin, Hook::PreLlmCall, &input(), &cfg);
    std::env::remove_var("PANTHEON_EXT_TEST_SECRET");
    let ctx = out
        .unwrap_or_else(|e| panic!("probe plugin failed to run: {e}"))
        .unwrap_or_default();
    assert!(
        ctx.contains("secret=<unset>"),
        "plugin saw the host secret through the environment: {ctx}"
    );
    assert!(
        !ctx.contains("must-not-leak"),
        "secret material reached plugin output: {ctx}"
    );
    assert!(
        ctx.contains("path_set=True"),
        "PATH must be restored for the child: {ctx}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
