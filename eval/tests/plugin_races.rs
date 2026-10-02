//! Regression + race-harness tests for the plugin loader check-then-use
//! races:
//!
//! (a) HASH-BIND AT SPAWN: verification hashes the plugin dir, but the old
//!     spawn path exec'd later with no re-hash. A swapper racing session
//!     start could get unapproved bytes executed.
//! (b) CONTAINMENT: verification canonicalized the runner for the
//!     starts_with containment check, discarded the result, and spawned the
//!     raw path - a symlink swap between canonicalize and exec escaped the
//!     plugin dir.
//!
//! Deterministic tests below fail before the fix and pass after it; the
//! `race_*` harnesses re-run the actual race with a swapper thread
//! rename-swapping two plugin variants and count "dangerous wins"
//! (approval/hash bound to A, executed B).

use pantheon_api::capability::Capability;
use pantheon_exec::plugins::{
    DiscoveredPlugin, EnvVarDecl, PluginLocation, PluginManifest, ToolCapability,
};
use pantheon_exec::supervisor::PluginSupervisor;
use pantheon_extensions::hooks::Hook;
use pantheon_extensions::python_runner::{fire_hook_full, HookInput, PythonPlugin, RunnerConfig};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Tool-plugin fixtures
// ---------------------------------------------------------------------------

fn tool_manifest(name: &str) -> PluginManifest {
    PluginManifest {
        name: name.into(),
        description: "race fixture".into(),
        version: "0.1.0".into(),
        sha: None,
        maintainer: String::new(),
        capabilities: vec![ToolCapability {
            name: "who".into(),
            capability: Capability::ShellExecute,
            description: "identify".into(),
            parameters: serde_json::json!({}),
        }],
        env_vars: Vec::<EnvVarDecl>::new(),
        runner: "run.sh".into(),
        enabled: true,
    }
}

/// A complete tool-plugin dir whose runner answers `who:<TAG>` - the tag
/// is baked into the script at write time, so it reports which variant's
/// bytes actually executed (reading an external file at call time would
/// misattribute across a swap).
fn write_tool_variant(dir: &Path, who: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let manifest = tool_manifest("victim");
    std::fs::write(
        dir.join("manifest.yaml"),
        serde_yaml::to_string(&manifest).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join("run.sh"),
        format!(
            "#!/bin/sh\nread -r line\ncid=$(printf '%s' \"$line\" | sed 's/.*\"call_id\":\"\\([^\"]*\\)\".*/\\1/')\nprintf '{{\"call_id\":\"%s\",\"result\":\"who:{who}\"}}\\n' \"$cid\"\n"
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(dir.join("run.sh")).unwrap().permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(dir.join("run.sh"), p).unwrap();
    }
}

fn discovered_tool(root: &Path) -> DiscoveredPlugin {
    DiscoveredPlugin {
        manifest: tool_manifest("victim"),
        location: PluginLocation::User,
        root: root.to_path_buf(),
    }
}

fn call_who(sup: &mut PluginSupervisor) -> Option<String> {
    // `call` returns the result value with the JSON envelope stripped.
    sup.call("who", serde_json::json!({})).ok()
}

// ---------------------------------------------------------------------------
// (b) deterministic: verify_plugin returns the canonical runner path
// ---------------------------------------------------------------------------

#[test]
fn verify_plugin_returns_canonical_runner_path() {
    let tmp = TempDir::new().unwrap();
    // Real dir behind a symlink: the raw root goes through `link`, the
    // canonical target does not.
    let real = tmp.path().join("real").join("bundled").join("t");
    std::fs::create_dir_all(&real).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(tmp.path().join("real"), tmp.path().join("link")).unwrap();
    let root = tmp.path().join("link").join("bundled").join("t");
    let plugin = DiscoveredPlugin {
        manifest: PluginManifest {
            runner: "run.sh".into(),
            ..tool_manifest("t")
        },
        location: PluginLocation::User,
        root: root.clone(),
    };
    std::fs::write(root.join("manifest.yaml"), "name: t\n").unwrap();
    std::fs::write(root.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();

    let out = pantheon_exec::plugins::verify_plugin(&plugin).unwrap();
    // BEFORE FIX: returned root.join("run.sh") (through the symlink).
    // AFTER: the canonical path the containment check actually proved.
    assert_eq!(out, root.canonicalize().unwrap().join("run.sh"));
    assert!(
        !out.to_string_lossy().contains("link"),
        "returned path must not go through the symlink: {out:?}"
    );
}

// ---------------------------------------------------------------------------
// (b) behavioral: a symlink swap between verify and spawn cannot redirect
// exec. The runner "run.sh" is a symlink -> good.sh; verify returns the
// canonical target. Swap the symlink to evil.sh AFTER verify, then spawn
// the verified path: the child must still execute good.sh.
// BEFORE FIX: verify returned the raw path and spawn exec'd through the
// swapped symlink -> evil.sh ran -> this test failed.
#[test]
fn spawn_execs_verified_target_despite_symlink_swap() {
    let tmp = TempDir::new().unwrap();
    let plug = tmp.path().join("bundled").join("t");
    std::fs::create_dir_all(&plug).unwrap();
    for (name, marker) in [("good.sh", "GOOD"), ("evil.sh", "EVIL")] {
        std::fs::write(
            plug.join(name),
            format!(
                "#!/bin/sh\nread -r line\ncid=$(printf '%s' \"$line\" | sed 's/.*\"call_id\":\"\\([^\"]*\\)\".*/\\1/')\nprintf '{{\"call_id\":\"%s\",\"result\":\"{marker}\"}}\\n' \"$cid\"\n"
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(plug.join(name)).unwrap().permissions();
            p.set_mode(0o755);
            std::fs::set_permissions(plug.join(name), p).unwrap();
        }
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(plug.join("good.sh"), plug.join("run.sh")).unwrap();
    let plugin = DiscoveredPlugin {
        manifest: PluginManifest {
            runner: "run.sh".into(),
            ..tool_manifest("t")
        },
        location: PluginLocation::User,
        root: plug.clone(),
    };
    std::fs::write(plug.join("manifest.yaml"), "name: t\n").unwrap();

    // Verify: binds the canonical target (good.sh).
    let verified = pantheon_exec::plugins::verify_plugin(&plugin).unwrap();
    assert_eq!(verified, plug.canonicalize().unwrap().join("good.sh"));

    // The (b) race: swap the symlink after verification.
    std::fs::remove_file(plug.join("run.sh")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(plug.join("evil.sh"), plug.join("run.sh")).unwrap();

    // Spawn the VERIFIED path: must execute good.sh, not the swapped link.
    let mut sup = PluginSupervisor::spawn(
        &verified,
        &plugin.manifest,
        tmp.path(),
        Duration::from_secs(5),
        &[],
    )
    .unwrap();
    let got = call_who(&mut sup).expect("call works");
    sup.stop();
    assert_eq!(
        got, "GOOD",
        "spawn must exec the verified target, not the swapped symlink"
    );
}

// ---------------------------------------------------------------------------
// (a) deterministic: spawn_verified fails closed when bytes changed
// ---------------------------------------------------------------------------

#[test]
fn spawn_verified_fails_closed_on_swapped_bytes() {
    let tmp = TempDir::new().unwrap();
    let plugins = tmp.path().join("plugins");
    let victim = plugins.join("victim");
    write_tool_variant(&victim, "A");
    let plugin = discovered_tool(&victim);
    // Operator approves variant A.
    pantheon_exec::plugin_approval::record_approval(&plugin).unwrap();
    assert!(pantheon_exec::plugins::verify_plugin(&plugin).is_ok());

    // Attacker swaps the bytes after approval: B writes a sentinel if it
    // ever executes.
    let sentinel = tmp.path().join("pwned");
    write_tool_variant(&victim, "B");
    std::fs::write(
        victim.join("run.sh"),
        format!(
            "#!/bin/sh\ntouch {}\nread -r line\nprintf '{{\"call_id\":\"x\",\"result\":\"who:B\"}}\\n'\n",
            sentinel.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = std::fs::metadata(victim.join("run.sh"))
            .unwrap()
            .permissions();
        p.set_mode(0o755);
        std::fs::set_permissions(victim.join("run.sh"), p).unwrap();
    }

    let err =
        match PluginSupervisor::spawn_verified(&plugin, tmp.path(), Duration::from_secs(5), &[]) {
            Ok(_) => panic!("swapped bytes must fail closed"),
            Err(e) => e,
        };
    assert!(
        err.code == "PLUGIN_TAMPERED" || err.code == "PLUGIN_NOT_APPROVED",
        "unexpected code: {}",
        err.code
    );
    assert!(!sentinel.exists(), "variant B must never have executed");
}

// ---------------------------------------------------------------------------
// Hook-plugin fixtures
// ---------------------------------------------------------------------------

fn write_hook_variant(dir: &Path, tag: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("plugin.yaml"),
        "name: victim\nversion: 0.1.0\nprovides_hooks: [pre_llm_call]\n",
    )
    .unwrap();
    // NOTE: the two variants intentionally differ in SIZE (padding). The
    // pre-fix SHIM imports via importlib's exec_module, which consults
    // __pycache__ validated by (mtime, size): same-size variants written
    // within one mtime tick would let a swap serve stale cached bytecode
    // and hide wins. Different sizes make the cache useless across swaps.
    let pad =
        "# padding to change file size across variants\n".repeat(if tag == "A" { 1 } else { 40 });
    std::fs::write(
        dir.join("__init__.py"),
        format!(
            "{pad}def register(ctx):\n    def pre_llm_call(**kw):\n        return {{'context': 'CTX_{tag}:' + __file__}}\n    ctx.register_hook('pre_llm_call', pre_llm_call)\n"
        ),
    )
    .unwrap();
}

/// Deterministic xorshift jitter (no extra deps). The race loops use this
/// to defeat phase-locking between the swapper thread and the spawner:
/// without jitter the two loops can synchronize so every attempt sees
/// the same swap phase, making win counts bimodal (0 or many).
fn jitter_us(state: &mut u64, max_us: u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state % max_us
}

fn hook_input() -> HookInput {
    HookInput {
        hook: Hook::PreLlmCall.name().to_string(),
        session_id: "race".into(),
        platform: "test".into(),
        extra: HashMap::new(),
    }
}

fn hook_cfg() -> RunnerConfig {
    RunnerConfig {
        python: "python3".into(),
        timeout: Duration::from_secs(15),
        policy: None,
    }
}

fn approved_hook_plugin(ext_dir: &Path, victim: &Path) -> PythonPlugin {
    pantheon_api::approval::record_approval_for(ext_dir, "victim", "0.1.0", victim).unwrap();
    let mut plugin = PythonPlugin::load(victim).expect("fixture loads");
    plugin.scope_dir = Some(ext_dir.to_path_buf());
    plugin
}

// ---------------------------------------------------------------------------
// (a) deterministic: hook fire fails closed when bytes changed since approval
// ---------------------------------------------------------------------------

#[test]
fn hook_fire_fails_closed_on_swapped_bytes() {
    let tmp = TempDir::new().unwrap();
    let ext_dir = tmp.path().join("ext");
    let victim = ext_dir.join("victim");
    write_hook_variant(&victim, "A");
    let plugin = approved_hook_plugin(&ext_dir, &victim);
    // Sanity: approved bytes fire fine.
    let out = fire_hook_full(&plugin, Hook::PreLlmCall, &hook_input(), &hook_cfg())
        .expect("approved fire works");
    assert!(
        out.context.as_deref().unwrap().starts_with("CTX_A:"),
        "{out:?}"
    );

    // Attacker swaps in variant B between load-check and hook fire.
    write_hook_variant(&victim, "B");
    let err = fire_hook_full(&plugin, Hook::PreLlmCall, &hook_input(), &hook_cfg())
        .expect_err("swapped hook bytes must fail closed");
    assert_eq!(err.code, "EXT_TAMPERED", "{err:?}");
}

// ---------------------------------------------------------------------------
// (b) deterministic: hook fire loads __init__.py from the canonical dir
// ---------------------------------------------------------------------------

#[test]
fn hook_fire_uses_canonical_plugin_dir() {
    let tmp = TempDir::new().unwrap();
    let real_ext = tmp.path().join("real").join("ext");
    let victim = real_ext.join("victim");
    write_hook_variant(&victim, "A");
    #[cfg(unix)]
    std::os::unix::fs::symlink(tmp.path().join("real"), tmp.path().join("link")).unwrap();
    // Address the plugin through the symlink; approve against the canonical
    // scope (that is what the fire-time re-check canonicalizes to).
    let canon_ext = real_ext.canonicalize().unwrap();
    pantheon_api::approval::record_approval_for(&canon_ext, "victim", "0.1.0", &victim).unwrap();
    let mut plugin = PythonPlugin::load(&tmp.path().join("link").join("ext").join("victim"))
        .expect("fixture loads");
    plugin.scope_dir = Some(tmp.path().join("link").join("ext"));

    let out =
        fire_hook_full(&plugin, Hook::PreLlmCall, &hook_input(), &hook_cfg()).expect("fire works");
    let ctx = out.context.expect("context");
    let canon_init = victim.canonicalize().unwrap().join("__init__.py");
    // BEFORE FIX: SHIM got the raw symlinked dir, so __file__ contained
    // "link". AFTER: the canonical dir.
    assert_eq!(ctx, format!("CTX_A:{}", canon_init.display()));
    assert!(
        !ctx.contains("link"),
        "plugin must load from the canonical dir, got: {ctx}"
    );
}

// ---------------------------------------------------------------------------
// Race harness: swapper thread rename-swapping A <-> B under a fixed path
// ---------------------------------------------------------------------------

/// Swap `victim` between `store_a` and `store_b` contents by rename until
/// `stop` is set. `victim_is_a` tracks which variant is live.
fn spawn_dir_swapper(
    victim: PathBuf,
    store_a: PathBuf,
    store_b: PathBuf,
    victim_is_a: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let spare = victim.parent().unwrap().join("spare");
        std::fs::create_dir_all(&spare).unwrap();
        let mut next_is_a = false;
        while !stop.load(Ordering::Relaxed) {
            let src = if next_is_a { &store_a } else { &store_b };
            // victim -> spare -> victim dance; each rename is atomic.
            if std::fs::rename(&victim, &spare).is_err() {
                break;
            }
            if std::fs::rename(src, &victim).is_err() {
                let _ = std::fs::rename(&spare, &victim);
                break;
            }
            if std::fs::rename(&spare, src).is_err() {
                break;
            }
            victim_is_a.store(next_is_a, Ordering::Relaxed);
            next_is_a = !next_is_a;
            // Let the state settle so the main thread's hash usually sees a
            // stable tree; without this the swapper is so hot that no
            // verification ever completes and the race never gets checked.
            std::thread::sleep(Duration::from_millis(3));
        }
    })
}

const RACE_ITERS: usize = 200;

/// Tool-plugin race. BEFORE: old call-site pattern (verify -> gap ->
/// spawn raw path, no re-hash). AFTER: spawn_verified (re-hash at spawn).
/// A "dangerous win" = approval/hash bound to A at check time, but B
/// executed.
#[test]
fn race_tool_plugin_swap_old_vs_new() {
    let tmp = TempDir::new().unwrap();
    let plugins = tmp.path().join("plugins");
    let victim = plugins.join("victim");
    let store_a = tmp.path().join("store_a");
    let store_b = tmp.path().join("store_b");
    write_tool_variant(&victim, "A");
    write_tool_variant(&store_a, "A");
    write_tool_variant(&store_b, "B");
    let plugin = discovered_tool(&victim);
    pantheon_exec::plugin_approval::record_approval(&plugin).unwrap();

    let victim_is_a = Arc::new(AtomicBool::new(true));
    let stop = Arc::new(AtomicBool::new(false));
    let swapper = spawn_dir_swapper(
        victim.clone(),
        store_a,
        store_b,
        Arc::clone(&victim_is_a),
        Arc::clone(&stop),
    );

    // ---- BEFORE: verify, sleep (the session-start gap), spawn, use ----
    let raw_runner = victim.join("run.sh");
    let mut before_wins = 0usize;
    let mut before_checked = 0usize;
    let mut js = 0x9E3779B97F4A7C15u64;
    for _ in 0..RACE_ITERS {
        std::thread::sleep(Duration::from_micros(jitter_us(&mut js, 4000)));
        // Old call-site pattern: verify_plugin's Ok was the whole check.
        if pantheon_exec::plugins::verify_plugin(&plugin).is_err() {
            continue; // bytes were B at check time: correctly refused
        }
        before_checked += 1;
        std::thread::sleep(Duration::from_millis(5)); // the verify->spawn gap
        let mut sup = match PluginSupervisor::spawn(
            &raw_runner,
            &plugin.manifest,
            tmp.path(),
            Duration::from_secs(5),
            &[],
        ) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if call_who(&mut sup).as_deref() == Some("who:B") {
            before_wins += 1; // checked H(A), executed B
        }
        sup.stop();
    }

    // ---- AFTER: spawn_verified re-hashes immediately before exec ----
    let mut after_wins = 0usize;
    let mut after_refused = 0usize;
    for _ in 0..RACE_ITERS {
        std::thread::sleep(Duration::from_micros(jitter_us(&mut js, 4000)));
        match PluginSupervisor::spawn_verified(&plugin, tmp.path(), Duration::from_secs(5), &[]) {
            Ok(mut sup) => {
                if call_who(&mut sup).as_deref() == Some("who:B") {
                    after_wins += 1;
                }
                sup.stop();
            }
            Err(_) => after_refused += 1, // fail-closed: swapped mid-flight
        }
    }

    stop.store(true, Ordering::Relaxed);
    swapper.join().unwrap();

    eprintln!("tool race: before wins={before_wins}/{before_checked} after wins={after_wins} refused={after_refused}");
    assert!(
        before_checked > 0,
        "race never got a clean check; harness broken"
    );
    assert!(
        before_wins > 0,
        "expected the old verify->gap->spawn pattern to lose the race at least once in {RACE_ITERS} iters"
    );
    assert_eq!(
        after_wins, 0,
        "spawn_verified must never execute swapped bytes"
    );
}

/// Hook-plugin race. BEFORE: fire with no scope (old behavior: check only
/// at load). AFTER: scope bound, re-hash at every fire. Dangerous win =
/// only A approved, but CTX_B executed.
#[test]
fn race_hook_plugin_swap_old_vs_new() {
    let tmp = TempDir::new().unwrap();
    let ext_dir = tmp.path().join("ext");
    let victim = ext_dir.join("victim");
    let store_a = tmp.path().join("hstore_a");
    let store_b = tmp.path().join("hstore_b");
    write_hook_variant(&victim, "A");
    write_hook_variant(&store_a, "A");
    write_hook_variant(&store_b, "B");
    pantheon_api::approval::record_approval_for(&ext_dir, "victim", "0.1.0", &victim).unwrap();

    let victim_is_a = Arc::new(AtomicBool::new(true));
    let stop = Arc::new(AtomicBool::new(false));
    let swapper = spawn_dir_swapper(
        victim.clone(),
        store_a,
        store_b,
        Arc::clone(&victim_is_a),
        Arc::clone(&stop),
    );
    let cfg = hook_cfg();

    // ---- BEFORE: the pre-fix fire protocol, replicated exactly: no
    // load/fire re-verification, and the old SHIM which imported
    // whatever `__init__.py` was live at import time. Only A is
    // approved; a swap mid-flight executes B.
    let payload = serde_json::to_string(&hook_input()).unwrap();
    let mut js = 0x123456789ABCDEFu64;
    let mut before_wins = 0usize;
    for _ in 0..RACE_ITERS / 4 {
        std::thread::sleep(Duration::from_micros(jitter_us(&mut js, 8000)));
        let out = std::process::Command::new("python3")
            .args([
                "-c",
                OLD_SHIM,
                &victim.to_string_lossy(),
                "pre_llm_call",
                &payload,
            ])
            .current_dir(&victim)
            .output();
        if let Ok(out) = out {
            if let Ok(v) =
                serde_json::from_str::<serde_json::Value>(&String::from_utf8_lossy(&out.stdout))
            {
                let ctx = v.get("context").and_then(|c| c.as_str()).unwrap_or("");
                if ctx.starts_with("CTX_B:") {
                    before_wins += 1; // only A approved, B executed
                }
            }
        }
    }

    // ---- AFTER: scope bound -> re-hash before every fire ---------------
    // NOTE: do NOT re-record approval here: the store must stay bound to
    // variant A (recorded at setup), or the re-check would bless B.
    let mut new_plugin = PythonPlugin::load(&victim).expect("loads");
    new_plugin.scope_dir = Some(ext_dir.to_path_buf());
    let mut after_wins = 0usize;
    let mut after_ok_a = 0usize;
    for _ in 0..RACE_ITERS / 4 {
        std::thread::sleep(Duration::from_micros(jitter_us(&mut js, 8000)));
        match fire_hook_full(&new_plugin, Hook::PreLlmCall, &hook_input(), &cfg) {
            Ok(out) => {
                let ctx = out.context.unwrap_or_default();
                if ctx.starts_with("CTX_B:") {
                    after_wins += 1;
                } else if ctx.starts_with("CTX_A:") {
                    after_ok_a += 1;
                }
            }
            Err(_) => {} // fail-closed on swap: expected
        }
    }

    stop.store(true, Ordering::Relaxed);
    swapper.join().unwrap();

    eprintln!(
        "hook race: before wins={before_wins} after wins={after_wins} clean_a_fires={after_ok_a}"
    );
    assert!(
        before_wins > 0,
        "expected the old no-recheck fire path to execute swapped bytes at least once"
    );
    assert_eq!(
        after_wins, 0,
        "fire-time re-verification must never execute swapped bytes"
    );
}

/// The pre-fix hook SHIM, embedded verbatim from git HEAD: it imported
/// whatever `__init__.py` was live at import time with no hash binding.
/// Used only to replicate the old vulnerable fire protocol in the race
/// harness's BEFORE phase.
const OLD_SHIM: &str = r#"
import importlib.util, json, sys
plug_dir, hook_name, payload_json = sys.argv[1], sys.argv[2], sys.argv[3]
result = {"context": None}
try:
    spec = importlib.util.spec_from_file_location(
        "pantheon_plugin", plug_dir + "/__init__.py")
    mod = importlib.util.module_from_spec(spec)
    sys.modules["pantheon_plugin"] = mod
    spec.loader.exec_module(mod)
    payload = json.loads(payload_json)
    calls = {}
    class Ctx:
        def register_hook(self, name, fn):
            calls.setdefault(name, []).append(fn)
    if not hasattr(mod, "register"):
        raise RuntimeError("plugin has no register(ctx)")
    mod.register(Ctx())
    out = None
    directive = None
    # Hermes plugins take FLAT kwargs (a `transform_tool_result` handler
    # expects `result=`, a `pre_tool_call` handler expects `tool=`/`args=`).
    # HookInput nests per-call context under `extra`, so lift it to the top
    # level here. Reserved names (hook/session_id/platform) win on collision,
    # so `extra` can never shadow the envelope's own identity.
    call = dict(payload.get("extra") or {})
    for k in ("hook", "session_id", "platform"):
        if k in payload:
            call[k] = payload[k]
    for fn in calls.get(hook_name, []):
        r = fn(**call)
        if not isinstance(r, dict):
            continue
        if r.get("context"):
            c = str(r["context"])
            out = (out + "\n" + c) if out else c
        # Gate/transform directives: first one wins, mirroring Hermes'
        # first-non-None contract. Keys are read flat so a plugin can answer
        # `{"deny": True, "reason": "..."}` or `{"replacement": "..."}`.
        if directive is None:
            d = {}
            if r.get("deny"):
                d["deny"] = True
            if r.get("reason"):
                d["reason"] = str(r["reason"])
            if r.get("replacement") is not None:
                d["replacement"] = str(r["replacement"])
            if d:
                directive = d
    result = {"context": out, "directive": directive}
except Exception as e:
    result = {"context": None, "directive": None,
              "error": f"{type(e).__name__}: {e}"}
print(json.dumps(result))
"#;
