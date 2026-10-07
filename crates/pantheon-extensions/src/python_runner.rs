//! Python hook runner: spawns `__init__.py`-style plugins in a subprocess.
//! V0 contract: one JSON line in on stdin, one JSON line out on stdout.
//! Fail-open: any crash/timeout/bad output => Ok(None), never a broken turn.
use crate::hooks::Hook;
use crate::manifest::PluginManifest;
use pantheon_api::capability::{Capability, Policy};
use pantheon_api::error::{Layer, PantheonError};
use pantheon_exec::sandbox::{build_sandboxed, enforce, Enforcement};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

fn xerr(code: &str, cause: String, retryable: bool) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Extension,
        retryable,
        cause,
        "check plugin dir, python3, and hook timeout",
        "",
    )
}

/// What the runner sends to the plugin process.
#[derive(Debug, Clone, Serialize)]
pub struct HookInput {
    pub hook: String,
    pub session_id: String,
    pub platform: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub extra: HashMap<String, String>,
}

/// What a plugin process may return. `context` = inject (existing contract);
/// `directive` = a gate/transform decision (see [`crate::hooks::HookClass`]).
/// null/{} = silent.
#[derive(Debug, Clone, Deserialize)]
pub struct HookOutput {
    #[serde(default)]
    pub context: Option<String>,
    /// A gate deny or a transform replacement. `deny: true` blocks the gated
    /// action; `replacement` (when set) replaces the payload. Both fail open
    /// on a *failing* plugin (see manager), while an explicit directive is
    /// authoritative when the plugin answers cleanly.
    #[serde(default)]
    pub directive: Option<HookDirective>,
    #[serde(default)]
    pub error: Option<String>,
}

/// A plugin's answer to a gate or transform hook.
#[derive(Debug, Clone, Deserialize)]
pub struct HookDirective {
    /// Gate: block the action. Fails the turn closed when true.
    #[serde(default)]
    pub deny: bool,
    /// Human-readable reason for a deny (gate) - surfaced to the model/user.
    #[serde(default)]
    pub reason: Option<String>,
    /// Transform: replacement payload. Empty/None means "no change".
    #[serde(default)]
    pub replacement: Option<String>,
}

/// A loaded Python plugin on disk.
#[derive(Debug, Clone)]
pub struct PythonPlugin {
    pub dir: PathBuf,
    pub manifest: PluginManifest,
    /// Extensions root that approved this plugin, set by the manager at
    /// load time. `fire_hook_full` re-verifies the plugin dir against this
    /// scope immediately before every spawn (canonicalize + containment +
    /// approval-hash re-check, fail closed). `None` = standalone/test
    /// use: no fire-time re-verification, as before.
    pub scope_dir: Option<PathBuf>,
}

impl PythonPlugin {
    pub fn load(dir: &Path) -> Result<Self, PantheonError> {
        let manifest = PluginManifest::load(&dir.join("plugin.yaml"))?;
        if !dir.join("__init__.py").exists() {
            return Err(xerr(
                "EXT_NO_ENTRY",
                format!("{} has no __init__.py", dir.display()),
                false,
            ));
        }
        Ok(Self {
            dir: dir.to_path_buf(),
            manifest,
            scope_dir: None,
        })
    }
    pub fn provides(&self, hook: Hook) -> bool {
        self.manifest.hook_list().0.contains(&hook)
    }
}

/// Runner config.
#[derive(Debug, Clone)]
pub struct RunnerConfig {
    pub python: String,
    pub timeout: Duration,
    /// Capability policy gating plugin spawns. `Some` = every spawn goes
    /// through [`enforce`] first: `Deny` refuses the spawn, `Approval`
    /// holds it at the boundary, `Allow` runs it inside the enforcement's
    /// sandbox profile. `None` = no policy configured: spawns run as
    /// before (direct child, minimal env), preserving the standalone /
    /// test behavior.
    pub policy: Option<Policy>,
}

impl Default for RunnerConfig {
    fn default() -> Self {
        Self {
            python: "python3".into(),
            timeout: Duration::from_secs(10),
            policy: None,
        }
    }
}

impl RunnerConfig {
    /// Config with a policy attached (builder-style; the policy gates
    /// every plugin spawn - see [`fire_hook_full`]).
    pub fn with_policy(mut self, policy: Policy) -> Self {
        self.policy = Some(policy);
        self
    }
}

/// The shim: imports the plugin `__init__`, calls `register(ctx)` with a
/// capturing ctx, dispatches the requested hook, prints result JSON.
///
/// Hash-bound at load: the host passes the expected sha256 of the
/// verified `__init__.py` as argv[4]. The shim reads the entrypoint ONCE,
/// refuses to run when the bytes differ from what the host verified
/// (a swap between the host's check and this read fails closed here),
/// and executes EXACTLY the bytes it hashed - there is no
/// check-then-use gap inside the child between hashing and importing.
const SHIM: &str = r#"
import hashlib, importlib.util, json, sys
plug_dir, hook_name, payload_json, expected_sha = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
entry = plug_dir + "/__init__.py"
result = {"context": None}
try:
    with open(entry, "rb") as f:
        src = f.read()
    if hashlib.sha256(src).hexdigest() != expected_sha:
        raise RuntimeError(
            "plugin __init__.py changed after host verification; refusing to run swapped code")
    spec = importlib.util.spec_from_file_location(
        "pantheon_plugin", entry)
    mod = importlib.util.module_from_spec(spec)
    sys.modules["pantheon_plugin"] = mod
    exec(compile(src, entry, "exec"), mod.__dict__)
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

/// Fire one hook for its context return value. `Ok(Some(ctx))` = inject,
/// `Ok(None)` = silent. Infrastructure failures (crash, timeout, bad output)
/// are fail-open here: a context hook must never break a turn.
pub fn fire_hook(
    plugin: &PythonPlugin,
    hook: Hook,
    input: &HookInput,
    cfg: &RunnerConfig,
) -> Result<Option<String>, PantheonError> {
    Ok(fire_hook_full(plugin, hook, input, cfg)
        .ok()
        .and_then(|o| o.context)
        .filter(|c| !c.trim().is_empty()))
}

/// Hash-bind at spawn for hook plugins. The manager's approval check runs
/// at load time; the plugin dir is re-verified immediately before every
/// hook fire and the CANONICAL dir is what the child receives.
///
/// Returns the canonical dir plus the expected sha256 of the verified
/// `__init__.py`, which the SHIM re-checks inside the child before
/// importing (see `SHIM`). The entrypoint bytes are sandwiched around the
/// approval hash - read, hash+approve, re-read and compare - so the
/// expected hash is bound to bytes that were part of an approved tree:
/// a swap at any point fails closed here, and a swap after this function
/// returns fails closed in the SHIM. Either way swapped bytes never
/// execute.
///
/// Callers must not execute on `Err`; `fire_hook` collapses this to
/// silence, `fire_hook_full` surfaces it so gates can distinguish
/// "plugin refused" from "plugin allowed".
fn verify_hook_spawn(plugin: &PythonPlugin) -> Result<(PathBuf, String), PantheonError> {
    let tampered = |cause: String| xerr("EXT_TAMPERED", cause, false);
    let canon_dir = plugin
        .dir
        .canonicalize()
        .map_err(|e| tampered(format!("canonicalize {}: {e}", plugin.dir.display())))?;
    if let Some(scope) = plugin.scope_dir.as_ref() {
        let canon_scope = scope
            .canonicalize()
            .map_err(|e| tampered(format!("canonicalize {}: {e}", scope.display())))?;
        if !canon_dir.starts_with(&canon_scope) {
            return Err(tampered(format!(
                "plugin dir {} escapes the extensions dir",
                canon_dir.display()
            )));
        }
        if !pantheon_api::approval::is_bundled(&canon_scope, &canon_dir) {
            let entry = canon_dir.join("__init__.py");
            // (1) Read the entrypoint bytes BEFORE the approval hash.
            let before = std::fs::read(&entry)
                .map_err(|e| tampered(format!("read {}: {e}", entry.display())))?;
            // (2) The tree must be in an approved state.
            let hash = pantheon_api::approval::dir_hash(&canon_dir)
                .map_err(|e| tampered(format!("hash {}: {e}", canon_dir.display())))?;
            if !pantheon_api::approval::is_approved(
                &canon_scope,
                &plugin.manifest.name,
                &plugin.manifest.version,
                &hash,
            ) {
                return Err(tampered(format!(
                    "plugin '{}' changed since approval; re-approve to run it",
                    plugin.manifest.name
                )));
            }
            // (3) Re-read: the entrypoint must be byte-identical to what
            // step (1) saw, otherwise the expected hash below would not
            // be bound to the approved tree.
            let after = std::fs::read(&entry)
                .map_err(|e| tampered(format!("re-read {}: {e}", entry.display())))?;
            if before != after {
                return Err(tampered(format!(
                    "plugin '{}' changed during verification",
                    plugin.manifest.name
                )));
            }
            let expected = pantheon_api::approval::bytes_hash(&before);
            return Ok((canon_dir, expected));
        }
    }
    // Standalone (no scope) or bundled: no approval hash to bind against.
    // Still hand the SHIM the current bytes' hash so a swap between here
    // and the child's import fails closed instead of executing.
    let entry = canon_dir.join("__init__.py");
    let bytes =
        std::fs::read(&entry).map_err(|e| tampered(format!("read {}: {e}", entry.display())))?;
    if !canon_dir.join("plugin.yaml").is_file() {
        return Err(tampered(format!(
            "plugin manifest missing in {}",
            canon_dir.display()
        )));
    }
    Ok((canon_dir, pantheon_api::approval::bytes_hash(&bytes)))
}

/// Fire one hook and return the full answer, INCLUDING an error for
/// infrastructure failure.
///
/// This distinction is the whole point: a context hook can collapse failure
/// into silence, but a *gate* must be able to tell "the plugin allowed this"
/// apart from "the plugin never answered". So this returns `Err` on spawn
/// failure, crash, timeout, and unparseable output; a clean answer - even an
/// empty one - is `Ok`.
pub fn fire_hook_full(
    plugin: &PythonPlugin,
    hook: Hook,
    input: &HookInput,
    cfg: &RunnerConfig,
) -> Result<HookOutput, PantheonError> {
    let payload =
        serde_json::to_string(input).map_err(|e| xerr("EXT_INPUT_ENCODE", e.to_string(), false))?;
    // Sandbox enforcement: the capability policy owns *whether* the
    // plugin process may run at all. `None` = no policy configured
    // (standalone/test use): the spawn proceeds un-gated, as before.
    let profile = match cfg.policy.as_ref() {
        Some(policy) => match enforce(policy, &Capability::PluginEnable) {
            Enforcement::Run(profile) => Some(profile),
            Enforcement::Deny { reason, .. } => {
                return Err(xerr(
                    "EXT_DENIED",
                    format!("plugin.enable: {reason}"),
                    false,
                ))
            }
            Enforcement::RequireApproval { scope, .. } => {
                return Err(xerr(
                    "EXT_APPROVAL_REQUIRED",
                    format!("plugin.enable requires approval (scope: {scope})"),
                    false,
                ))
            }
        },
        None => None,
    };
    let arg_dir = {
        // Re-verify immediately before spawn: canonicalize, prove
        // containment, re-hash against the approval store with the
        // entrypoint bytes sandwiched around the hash, fail closed on
        // any divergence since load. The canonical dir - not the raw
        // configured path - is what the child loads `__init__.py` from,
        // and the SHIM executes exactly the bytes this hash covers.
        let (canon_dir, expected_sha) = verify_hook_spawn(plugin)?;
        (canon_dir.to_string_lossy().to_string(), expected_sha)
    };
    let (arg_dir, expected_sha) = arg_dir;
    let args = [
        "-c",
        SHIM,
        arg_dir.as_str(),
        hook.name(),
        payload.as_str(),
        expected_sha.as_str(),
    ];
    // The plugin runs in its own directory; canonicalize always yields an
    // absolute path, so it is a valid cwd for build_sandboxed.
    let cwd = arg_dir.clone();
    let (mut cmd, group_leader) = match &profile {
        // Policy allowed it: the enforcement's sandbox profile decides
        // how isolated the child runs (namespace wrapper + rlimits where
        // the host supports them; a direct spawn where it does not - the
        // policy gate above is what restores enforcement, the wrapper is
        // best-effort isolation, see build_sandboxed).
        //
        // build_sandboxed confines the child via pre_exec setsid(), so
        // the child is a process-group leader and run_drained may
        // killpg the group on a stuck drain. A direct spawn is not a
        // group leader - signalling its pid as a group could hit
        // unrelated processes.
        Some(profile) => (build_sandboxed(profile, &cfg.python, &args, &cwd), true),
        None => {
            let mut cmd = Command::new(&cfg.python);
            cmd.args(args);
            cmd.current_dir(&cwd);
            (cmd, false)
        }
    };
    // Third-party plugin code: no ambient host env crosses the boundary.
    crate::minimal_child_env(&mut cmd);
    let (status, out_bytes, _err_bytes) =
        run_drained(cmd, cfg.timeout, "EXT_TIMEOUT", "plugin hook", group_leader)?;
    if !status.success() {
        return Err(xerr("EXT_CRASH", format!("plugin exited {status}"), false));
    }
    let out = String::from_utf8_lossy(&out_bytes);
    serde_json::from_str(out.trim()).map_err(|e| xerr("EXT_BAD_OUTPUT", e.to_string(), false))
}

/// Cap per captured pipe, in bytes: a plugin's answer is one JSON line,
/// so anything past this is protocol breakage, not data. Bounded so a
/// chatty plugin cannot grow the parent without limit.
const PIPE_CAP_BYTES: usize = 1024 * 1024;

/// Grace after the child is gone (exited or killed) for the pipe
/// drainers to observe EOF before we stop waiting for them. A
/// grandchild that inherited the pipe can hold it open indefinitely
/// CPython file descriptors are inheritable by default, so a plugin
/// that double-forks leaves the write end open after the direct child
/// is dead. Waiting for the drainers without a bound hangs the caller;
/// the old code did exactly that (its "the pipes hit EOF once the child
/// is dead, so joining cannot hang" comment was wrong).
const POST_EXIT_DRAIN_GRACE: Duration = Duration::from_secs(5);

/// Tail wait after a group-kill: the pipe holder usually dies fast, and
/// its last bytes may already sit in the pipe. Bounded; whatever is
/// buffered when it expires is what we keep.
const POST_KILL_DRAIN_GRACE: Duration = Duration::from_secs(1);

/// Spawn `cmd` (already configured with piped stdout/stderr) and wait up
/// to `timeout`, draining BOTH pipes on reader threads from the moment
/// of spawn.
///
/// A child that fills the 64KiB pipe buffer while the parent only
/// `try_wait()`s wedges forever: the child blocks on write, the parent
/// blocks on wait, and the watchdog misreports the wedge as a timeout.
/// Draining concurrently removes the wedge; a genuinely slow plugin
/// still trips the timeout and is killed.
///
/// `group_leader` must be true exactly when the child was spawned as a
/// process-group leader - every `build_sandboxed` command is, via
/// `setsid()` in `pre_exec`; a direct spawn is not. Only then may we
/// `killpg` the group when a pipe-holding grandchild outlives the child:
/// signalling a pid that is not a group leader could hit unrelated
/// processes.
///
/// Returns `(status, stdout, stderr)`, each stream truncated to
/// [`PIPE_CAP_BYTES`]. `timeout_code`/`what` name the timeout error.
fn run_drained(
    mut cmd: Command,
    timeout: Duration,
    timeout_code: &'static str,
    what: &'static str,
    group_leader: bool,
) -> Result<(ExitStatus, Vec<u8>, Vec<u8>), PantheonError> {
    use std::io::Read;
    use std::sync::{Arc, Mutex};
    /// Drain one pipe on a thread, appending into a shared buffer as
    /// bytes arrive - so the parent can take what has been captured
    /// even if EOF never comes. Returns the thread and the buffer.
    fn drain<R: Read + Send + 'static>(
        pipe: R,
    ) -> (std::thread::JoinHandle<()>, Arc<Mutex<Vec<u8>>>) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let buf2 = Arc::clone(&buf);
        let h = std::thread::spawn(move || {
            let mut pipe = pipe;
            let mut chunk = [0u8; 8192];
            loop {
                let n = match pipe.read(&mut chunk) {
                    Ok(0) => break, // EOF
                    Ok(n) => n,
                    Err(_) => break,
                };
                if let Ok(mut kept) = buf2.lock() {
                    let room = PIPE_CAP_BYTES.saturating_sub(kept.len());
                    kept.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
        });
        (h, buf)
    }
    /// Finish one drainer after the child is gone: wait up to
    /// [`POST_EXIT_DRAIN_GRACE`] for EOF, then stop waiting and return
    /// the bytes captured so far. On expiry, when `group_leader` is set,
    /// SIGKILL the child's process group first (with the same
    /// never-signal-init-or-ourselves guard the sandbox runner uses), so
    /// a pipe-holding grandchild dies and the drainer can still observe
    /// EOF - then a short tail wait for its last bytes. Never hangs; in
    /// the worst case a blocked reader thread is left behind instead of
    /// the caller.
    fn finish_drain(
        handle: std::thread::JoinHandle<()>,
        buf: Arc<Mutex<Vec<u8>>>,
        child_pid: u32,
        group_leader: bool,
    ) -> Vec<u8> {
        let buffered = || buf.lock().map(|g| g.clone()).unwrap_or_default();
        let deadline = std::time::Instant::now() + POST_EXIT_DRAIN_GRACE;
        loop {
            if handle.is_finished() {
                let _ = handle.join();
                return buffered();
            }
            if std::time::Instant::now() >= deadline {
                #[cfg(unix)]
                if group_leader && child_pid > 1 && child_pid != std::process::id() {
                    // SAFETY: libc::killpg with a validated pgid; no
                    // memory unsafety involved.
                    unsafe { libc::killpg(child_pid as libc::pid_t, libc::SIGKILL) };
                }
                // Short tail wait: the holder usually dies at once and
                // its last bytes are already in the pipe.
                let tail = std::time::Instant::now() + POST_KILL_DRAIN_GRACE;
                while !handle.is_finished() && std::time::Instant::now() < tail {
                    std::thread::sleep(Duration::from_millis(10));
                }
                let _ = handle.is_finished().then(|| handle.join());
                return buffered();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| xerr("EXT_SPAWN", e.to_string(), false))?;
    let child_pid = child.id();
    let stdout_drainer = child.stdout.take().map(drain);
    let stderr_drainer = child.stderr.take().map(drain);

    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    // Bounded drain after the kill too: a grandchild
                    // holding a pipe open must not hang the timeout
                    // path either. (Bytes are discarded; the run
                    // already failed.)
                    for (h, b) in stdout_drainer.into_iter().chain(stderr_drainer) {
                        finish_drain(h, b, child_pid, group_leader);
                    }
                    return Err(xerr(
                        timeout_code,
                        format!("{what} exceeded {}s", timeout.as_secs()),
                        true,
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(xerr("EXT_WAIT", e.to_string(), true)),
        }
    };
    let out = stdout_drainer
        .map(|(h, b)| finish_drain(h, b, child_pid, group_leader))
        .unwrap_or_default();
    let err = stderr_drainer
        .map(|(h, b)| finish_drain(h, b, child_pid, group_leader))
        .unwrap_or_default();
    Ok((status, out, err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

    /// A real plugin dir on disk: `plugin.yaml` + `__init__.py` whose
    /// `pre_llm_call` handler floods stderr with 200KB (well past the
    /// 64KiB pipe buffer) and then answers cleanly.
    fn chatty_plugin_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-ext-drain-test-{}-{}",
            std::process::id(),
            FIXTURE_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.yaml"),
            "name: drain-fixture\nprovides_hooks: [pre_llm_call]\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("__init__.py"),
            "import sys\n\
             def register(ctx):\n\
             \x20   def pre_llm_call(**kw):\n\
             \x20       sys.stderr.write('e' * 200000)\n\
             \x20       sys.stderr.flush()\n\
             \x20       return {'context': 'plugin-says-hi'}\n\
             \x20   ctx.register_hook('pre_llm_call', pre_llm_call)\n",
        )
        .unwrap();
        dir
    }

    fn hook_input() -> HookInput {
        HookInput {
            hook: Hook::PreLlmCall.name().to_string(),
            session_id: "test-session".to_string(),
            platform: "test".to_string(),
            extra: HashMap::new(),
        }
    }

    /// Item 1 regression, helper level: 200KB on stdout *and* stderr
    /// must be captured, not wedged. The old try_wait-only loop blocked
    /// here (child stuck on write, parent on wait) and the watchdog
    /// misreported the wedge as a timeout.
    #[test]
    fn drained_spawn_captures_pipe_flood_without_wedging() {
        let mut cmd = Command::new("python3");
        cmd.arg("-c").arg(
            "import sys; sys.stdout.write('o' * 200000); sys.stdout.flush(); \
             sys.stderr.write('e' * 200000); sys.stderr.flush()",
        );
        // Generous: the run takes well under a second; the timeout is
        // only a backstop so a regression fails instead of hanging.
        let (status, out, err) =
            run_drained(cmd, Duration::from_secs(30), "EXT_TIMEOUT", "test", false)
                .expect("drained spawn should complete");
        assert!(status.success());
        assert_eq!(out.len(), 200_000, "stdout fully captured");
        assert_eq!(err.len(), 200_000, "stderr fully captured");
    }

    /// Item 3: a grandchild that inherits the pipe must not hang the
    /// drain. The child calls `os.setsid()` (so it is its own process
    /// group, like every `build_sandboxed` child), spawns `sleep 60`
    /// with the pipe as its stdout - the grandchild holds the write end
    /// open - then prints its answer and exits at once. The pipe never
    /// hits EOF while the grandchild lives, and CPython fds are
    /// inheritable by default, so a daemonizing plugin does exactly
    /// this. Before the fix `run_drained` joined the drainer
    /// unconditionally and hung here forever (fail-before was shown
    /// with `timeout` -> exit 124); now the grace expires, the group is
    /// killed, and the captured answer is returned.
    #[test]
    fn grandchild_holding_stdout_does_not_hang_drain() {
        let mut cmd = Command::new("python3");
        cmd.arg("-c").arg(
            "import os, subprocess, sys; \
             os.setsid(); \
             subprocess.Popen(['sleep', '60'], stdout=sys.stdout, stderr=sys.stderr); \
             sys.stdout.write('PLUGIN_ANSWER\\n'); sys.stdout.flush()",
        );
        let start = std::time::Instant::now();
        let (status, out, _err) =
            run_drained(cmd, Duration::from_secs(30), "EXT_TIMEOUT", "test", true)
                .expect("drain must complete despite the pipe-holding grandchild");
        let elapsed = start.elapsed();
        assert!(status.success(), "child exited 0");
        assert!(
            out.windows(b"PLUGIN_ANSWER".len())
                .any(|w| w == b"PLUGIN_ANSWER"),
            "the child's answer must be captured, got: {:?}",
            String::from_utf8_lossy(&out),
        );
        assert!(
            elapsed < Duration::from_secs(30),
            "drain must return via the grace, not outlive the grandchild's 60s sleep (took {elapsed:?})"
        );
    }

    /// Item 1 regression, end to end: a plugin flooding stderr past the
    /// pipe buffer still completes with its answer instead of timing out.
    #[test]
    fn hook_full_completes_despite_chatty_stderr() {
        let dir = chatty_plugin_dir();
        let plugin = PythonPlugin::load(&dir).expect("fixture plugin loads");
        let cfg = RunnerConfig {
            timeout: Duration::from_secs(15),
            ..RunnerConfig::default()
        };
        let out = fire_hook_full(&plugin, Hook::PreLlmCall, &hook_input(), &cfg)
            .expect("chatty plugin should complete, not time out");
        assert_eq!(out.context.as_deref(), Some("plugin-says-hi"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Item 2: a policy that denies `plugin.enable` refuses the spawn
    /// no child is ever started (a spawn would surface as a different
    /// error, never EXT_DENIED).
    #[test]
    fn hook_full_denied_when_policy_denies_plugin_enable() {
        let dir = chatty_plugin_dir();
        let plugin = PythonPlugin::load(&dir).expect("fixture plugin loads");
        // Policy::default() is default-deny: no rule grants plugin.enable.
        let cfg = RunnerConfig::default().with_policy(Policy::default());
        let err = fire_hook_full(&plugin, Hook::PreLlmCall, &hook_input(), &cfg)
            .expect_err("denied policy must refuse the spawn");
        assert_eq!(err.code, "EXT_DENIED", "unexpected error: {err:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Item 2: a policy that allows `plugin.enable` lets the spawn
    /// proceed through the sandbox profile - the gate was consulted and
    /// passed.
    ///
    /// `PluginEnable` maps to `VeryHigh`, so this needs a working bwrap
    /// user namespace. Hosts that refuse the uid map fail closed by
    /// design, which is not what this test is about: it is about the
    /// policy gate letting the spawn through. Skip when the host cannot
    /// provide the boundary, and say so.
    #[test]
    fn hook_full_proceeds_when_policy_allows_plugin_enable() {
        if !pantheon_exec::sandbox::boundary_available(
            pantheon_exec::sandbox::ExecutionBoundary::StrictNamespaces,
        ) {
            eprintln!("SKIP: host cannot build the StrictNamespaces boundary (bwrap userns)");
            return;
        }
        let dir = chatty_plugin_dir();
        let plugin = PythonPlugin::load(&dir).expect("fixture plugin loads");
        let policy = Policy::default().allow(Capability::PluginEnable);
        let cfg = RunnerConfig {
            timeout: Duration::from_secs(30),
            ..RunnerConfig::default().with_policy(policy)
        };
        let out = fire_hook_full(&plugin, Hook::PreLlmCall, &hook_input(), &cfg)
            .expect("allowed policy should let the spawn proceed");
        assert_eq!(out.context.as_deref(), Some("plugin-says-hi"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
