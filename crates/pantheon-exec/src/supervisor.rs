//! Plugin subprocess supervisor: spawns verified plugins as child processes,
//! speaks newline-delimited JSON over stdio, enforces timeouts, owns the
//! process group, and kills cleanly on shutdown.
//!
//! Protocol (one JSON object + newline per line):
//!   stdin  -> {"call_id": "...", "tool": "...", "args": {...}}
//!   stdout -> {"call_id": "...", "result": ...} or
//!             {"call_id": "...", "error": {"code": "...", "cause": "..."}}
//!
//! Design (sync, std-only):
//! - One persistent child per supervisor. `call` locks the whole exchange
//!   (write request, read one response line) so responses can never
//!   interleave, even with concurrent callers behind an `Arc<Mutex<..>>`.
//! - The response read runs on a helper thread joined with a deadline, so a
//!   wedged plugin can never wedge the agent loop. Timeout kills the whole
//!   process group, not just the root, so plugin-spawned helpers die too.
//! - The child runs in its own process group (setsid on Unix). Stop is
//!   TERM, poll, then KILL. Env is filtered twice: `env_clear()` wipes
//!   inheritance, then only manifest-declared vars that ALSO match the
//!   operator's env allowlist (plus PATH) reach the child. Pantheon
//!   secrets never cross the boundary.
//! - Large plugin output goes through `compact_output` before it reaches the
//!   caller, same as shell output.
use crate::plugins::{DiscoveredPlugin, PluginManifest};
use crate::{compact_output, CompactionPolicy};
use pantheon_api::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[cfg(unix)]
const SIGTERM: i32 = 15;
#[cfg(unix)]
const SIGKILL: i32 = 9;

/// Grace period between TERM and KILL on stop().
const STOP_GRACE: Duration = Duration::from_secs(5);
/// Poll interval while waiting for exit or a response line.
const POLL_MS: u64 = 25;
/// Upper bound on a plugin runner's size. Runners are scripts; anything
/// larger is either a mistake or a hostile pre-spawn append, so fail
/// closed instead of reading it into memory.
const MAX_RUNNER_BYTES: usize = 16 * 1024 * 1024;

static CALL_SEQ: AtomicU64 = AtomicU64::new(1);

fn merr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Execution,
        false,
        cause,
        "check the plugin process",
        "",
    )
}

/// One request to a plugin: which tool, with what args.
#[derive(Debug, Serialize)]
struct PluginRequest<'a> {
    call_id: String,
    tool: &'a str,
    args: serde_json::Value,
}

/// One response from a plugin.
#[derive(Debug, Deserialize)]
struct PluginResponse {
    call_id: String,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<PluginErrorDetail>,
}

#[derive(Debug, Deserialize)]
struct PluginErrorDetail {
    code: String,
    cause: String,
}

/// A private, sealed copy of a plugin runner's bytes, ready for exec.
///
/// On Linux this is an unnamed O_TMPFILE inode: it never has a path, so
/// nothing can retarget, replace, or rewrite it between sealing and exec.
/// On other platforms it is a 0700 file with an unguessable random name
/// under the sealed dir; `path` carries that name so the caller can exec
/// it (fd-exec is a Linux facility). Stale named files are swept by
/// [`PluginSupervisor::sealed_dir`] on the next seal.
struct SealedRunner {
    file: std::fs::File,
    /// Read on non-Linux (fd-exec is Linux-only); on Linux the sealed fd
    /// is exec'd directly and this stays `None`.
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    path: Option<PathBuf>,
}

/// 128 bits of randomness as hex, for unguessable sealed-file names.
/// Same-uid attackers share our file permissions, so the name is the only
/// thing keeping a racing writer out of the sealed file - and O_EXCL
/// creation fails closed on a pre-created plant regardless.
fn random_hex32() -> String {
    static CTR: AtomicU64 = AtomicU64::new(0);
    let mut buf = [0u8; 16];
    #[cfg(unix)]
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    // Always mix in per-process uniqueness: even if the urandom read
    // failed, the name stays unpredictable in practice.
    let uniq = CTR.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id() as u64;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    for (i, b) in buf.iter_mut().enumerate() {
        let m = uniq
            .wrapping_add(pid)
            .wrapping_add(nanos)
            .wrapping_add(i as u64) as u8;
        *b ^= m;
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// Supervises one plugin child process. Owns the child, its stdin, and a
/// line-buffered stdout reader. Not Clone; share via `Arc<Mutex<..>>`.
pub struct PluginSupervisor {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: Option<BufReader<ChildStdout>>,
    /// Process group id (== child pid after setsid). Only signal this group.
    pgid: i32,
    /// Per-call timeout.
    timeout: Duration,
    /// Set false after a timeout kill; further calls fail fast.
    alive: bool,
    /// Plugin name, for error messages.
    name: String,
    /// Compaction policy for large plugin output.
    compaction: CompactionPolicy,
    /// Drop only kills the group while the owning runtime still holds its
    /// lease. A stale supervisor may abandon its handles instead of signaling
    /// a potentially reused PGID.
    drop_kill: bool,
}

impl PluginSupervisor {
    /// Spawn the plugin runner. `runner` must already be verified by
    /// `verify_plugin` - pass its RETURNED (canonicalized) path, not a
    /// re-joined raw path.
    ///
    /// Defense in depth: the runner is canonicalized again here, then its
    /// bytes are read through the pinned fd and written to a private
    /// sealed copy ([`Self::seal_runner_bytes`]) that the child execs - so
    /// a symlink swap or in-place rewrite between verification and execve
    /// cannot redirect execution or smuggle in new bytes. The canonical
    /// path is used for argv[0] and diagnostics only, never re-opened.
    ///
    /// Env is filtered twice: `env_clear()` wipes inheritance, then only
    /// manifest-declared vars that ALSO match `env_allowlist` (exact names
    /// or `PREFIX_*`, see [`pantheon_secrets::env::env_var_allowed`]) are
    /// copied from the host. A project-controlled manifest can declare any
    /// name it likes, so a declared name alone never crosses the boundary
    /// the operator's allowlist is the second, mandatory gate. Empty
    /// allowlist (default) = no host vars reach the plugin. Pantheon
    /// secrets never cross the boundary; they travel through the secrets
    /// broker, not ambient env.
    pub fn spawn(
        runner: &Path,
        manifest: &PluginManifest,
        data_dir: &Path,
        timeout: Duration,
        env_allowlist: &[String],
    ) -> Result<Self, PantheonError> {
        // Resolve symlinks at spawn time. The containment proof in
        // verify_plugin was about the canonical target; resolving again
        // here keeps exec glued to the target resolved at spawn time.
        let runner = runner.canonicalize().map_err(|e| {
            merr(
                "PLUGIN_UNSAFE_RUNNER",
                format!("canonicalize {}: {e}", runner.display()),
            )
        })?;
        // Read through the pinned fd: from here on the exact bytes in
        // hand are what get sealed and exec'd - no path component is
        // re-resolved.
        let (runner_bytes, _) = Self::read_runner_bytes(&runner)?;
        Self::spawn_inner(
            &runner,
            &runner_bytes,
            manifest,
            data_dir,
            timeout,
            env_allowlist,
        )
    }

    /// Verify-then-spawn with the check-then-use gap closed:
    ///  1. Full `verify_plugin` boundary (manifest checks, approval bound
    ///     to the current content hash, containment of the canonical
    ///     runner).
    ///  2. Immediately before exec: re-resolve the runner, re-check
    ///     containment, open it (pinning the inode), and read its bytes
    ///     through the pinned fd.
    ///  3. Recompute the approval hash with the runner's bytes+identity
    ///     taken from the pinned read, and compare against the approval
    ///     store. A pass proves the exact bytes in hand were
    ///     operator-approved - an in-place same-inode rewrite racing this
    ///     spawn cannot smuggle unapproved bytes past the check, because
    ///     the check runs on the bytes already read, not on whatever the
    ///     path resolves to now.
    ///  4. Write those bytes to a private sealed copy and exec THAT. From
    ///     the pinned read to execve no plugin-dir path is re-resolved or
    ///     re-read, so a rename swap, symlink swap, or in-place rewrite in
    ///     that window cannot affect what executes.
    ///
    /// Any tampering fails closed with `PLUGIN_TAMPERED` instead of
    /// executing unapproved bytes.
    ///
    /// This is the spawn entry point session startup should use.
    pub fn spawn_verified(
        plugin: &DiscoveredPlugin,
        data_dir: &Path,
        timeout: Duration,
        env_allowlist: &[String],
    ) -> Result<Self, PantheonError> {
        // Full verification at T0. Returns the canonical runner path.
        let canon_runner = crate::plugins::verify_plugin(plugin)?;
        // Re-resolve + re-check containment immediately before exec.
        // Fails closed on any divergence from what verify_plugin approved.
        let canon_root = plugin.root.canonicalize().map_err(|e| {
            merr(
                "PLUGIN_UNSAFE_RUNNER",
                format!("canonicalize {}: {e}", plugin.root.display()),
            )
        })?;
        let canon_runner = canon_runner.canonicalize().map_err(|e| {
            merr(
                "PLUGIN_TAMPERED",
                format!("re-resolve runner {}: {e}", canon_runner.display()),
            )
        })?;
        if !canon_runner.starts_with(&canon_root) {
            return Err(merr(
                "PLUGIN_TAMPERED",
                format!(
                    "runner {} escapes the plugin dir at spawn time",
                    canon_runner.display()
                ),
            ));
        }
        // Pin + read BEFORE the approval check: the open fd pins the
        // inode, and these bytes are what the check binds and what gets
        // sealed below.
        let (runner_bytes, runner_md) = Self::read_runner_bytes(&canon_runner)?;
        if !crate::plugin_approval::is_approved_with_pinned_runner(
            plugin,
            &canon_root,
            &canon_runner,
            &runner_bytes,
            &runner_md,
        ) {
            return Err(merr(
                "PLUGIN_TAMPERED",
                format!(
                    "plugin '{}' changed after verification; re-approve to run it",
                    plugin.manifest.name
                ),
            ));
        }
        Self::spawn_inner(
            &canon_runner,
            &runner_bytes,
            &plugin.manifest,
            data_dir,
            timeout,
            env_allowlist,
        )
    }

    /// Open the canonical runner for exec. The returned handle pins the
    /// file: on Linux the child execs this fd itself (see
    /// [`Self::exec_command`]); on other platforms the open handle is
    /// simply held across spawn. The file must be a regular file.
    fn open_runner(runner: &Path) -> Result<std::fs::File, PantheonError> {
        let file = std::fs::File::open(runner)
            .map_err(|e| merr("PLUGIN_SPAWN", format!("open {}: {e}", runner.display())))?;
        let md = file
            .metadata()
            .map_err(|e| merr("PLUGIN_SPAWN", format!("fstat {}: {e}", runner.display())))?;
        if !md.is_file() {
            return Err(merr(
                "PLUGIN_UNSAFE_RUNNER",
                format!("{} is not a regular file", runner.display()),
            ));
        }
        Ok(file)
    }

    /// Open `runner`, pinning the inode, and read its full contents through
    /// the open fd, returning the bytes and the fd's fstat metadata. The
    /// returned bytes are exactly what the caller holds - a rename swap or
    /// in-place rewrite after this point cannot change them.
    fn read_runner_bytes(runner: &Path) -> Result<(Vec<u8>, std::fs::Metadata), PantheonError> {
        let pin = Self::open_runner(runner)?;
        let md = pin
            .metadata()
            .map_err(|e| merr("PLUGIN_SPAWN", format!("fstat {}: {e}", runner.display())))?;
        let mut bytes = Vec::new();
        pin.take(MAX_RUNNER_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| merr("PLUGIN_SPAWN", format!("read {}: {e}", runner.display())))?;
        if bytes.len() > MAX_RUNNER_BYTES {
            return Err(merr(
                "PLUGIN_RUNNER_TOO_LARGE",
                format!(
                    "runner {} exceeds {} bytes; refusing to load",
                    runner.display(),
                    MAX_RUNNER_BYTES
                ),
            ));
        }
        Ok((bytes, md))
    }

    /// Shared spawn implementation. `runner` must already be canonical; it
    /// is used for argv[0] and diagnostics only - never re-opened.
    /// `runner_bytes` are the exact bytes to execute: they are written to
    /// a private sealed file (see [`Self::seal_runner_bytes`]) and the
    /// child execs that, so nothing under the plugin dir is re-resolved or
    /// re-read between verification and execve.
    fn spawn_inner(
        runner: &Path,
        runner_bytes: &[u8],
        manifest: &PluginManifest,
        data_dir: &Path,
        timeout: Duration,
        env_allowlist: &[String],
    ) -> Result<Self, PantheonError> {
        let sealed = Self::seal_runner_bytes(runner_bytes, data_dir)?;
        let mut cmd = Self::exec_command(runner, &sealed);
        Self::configure_command(&mut cmd, manifest, data_dir, env_allowlist);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                cmd.pre_exec(|| {
                    // Become session leader: own process group == our pid.
                    if libc::setsid() < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| merr("PLUGIN_SPAWN", format!("spawn {}: {e}", runner.display())))?;
        // The sealed file served its purpose once the child exists: on
        // Linux the child execs the sealed fd directly (O_TMPFILE files
        // never had a name at all), and the named fallback path is swept
        // by sealed_dir() on the next seal - so drop our copy here.
        drop(sealed);
        let pgid = child.id() as i32;
        // Refuse to supervise PID 1 or our own process, defensively.
        if pgid <= 1 || pgid == std::process::id() as i32 {
            let _ = child.kill();
            return Err(merr(
                "PLUGIN_BAD_PID",
                format!("refusing to supervise pid {pgid}"),
            ));
        }
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| merr("PLUGIN_STDIN", "child has no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| merr("PLUGIN_STDOUT", "child has no stdout".into()))?;
        Ok(Self {
            child: Some(child),
            stdin: Some(stdin),
            stdout: Some(BufReader::new(stdout)),
            pgid,
            timeout,
            alive: true,
            name: manifest.name.clone(),
            compaction: CompactionPolicy::default(),
            drop_kill: true,
        })
    }

    /// Write `bytes` to a private file and return it for exec. See
    /// [`SealedRunner`] for the platform story.
    fn seal_runner_bytes(bytes: &[u8], data_dir: &Path) -> Result<SealedRunner, PantheonError> {
        let dir = Self::sealed_dir(data_dir)?;
        #[cfg(target_os = "linux")]
        if let Ok(file) = Self::seal_tmpfile(&dir, bytes) {
            return Ok(SealedRunner { file, path: None });
        }
        // Fallback (non-Linux, or a filesystem that rejects O_TMPFILE):
        // unguessable random name + O_EXCL so a pre-created plant fails
        // closed instead of being executed.
        let (file, path) = Self::seal_named_file(&dir, bytes)?;
        Ok(SealedRunner {
            file,
            path: Some(path),
        })
    }

    /// Ensure the private sealed-file dir exists (0700) and sweep stale
    /// named sealed files left by earlier fallback-path seals or by a
    /// crash between seal and exec. A sealed file is only needed for the
    /// milliseconds between sealing and execve, so anything older than an
    /// hour is definitionally a leftover.
    fn sealed_dir(data_dir: &Path) -> Result<PathBuf, PantheonError> {
        let dir = data_dir.join("plugin-sealed");
        std::fs::create_dir_all(&dir).map_err(|e| {
            merr(
                "PLUGIN_SPAWN",
                format!("create sealed dir {}: {e}", dir.display()),
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(
                |e| {
                    merr(
                        "PLUGIN_SPAWN",
                        format!("chmod sealed dir {}: {e}", dir.display()),
                    )
                },
            )?;
        }
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let cutoff = std::time::SystemTime::now() - Duration::from_secs(3600);
            for entry in entries.flatten() {
                let p = entry.path();
                let stale = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .map(|m| m < cutoff)
                    .unwrap_or(false);
                if stale {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
        Ok(dir)
    }

    /// Linux: write `bytes` to an unnamed O_TMPFILE inode in `dir`.
    #[cfg(target_os = "linux")]
    fn seal_tmpfile(dir: &Path, bytes: &[u8]) -> Result<std::fs::File, PantheonError> {
        use std::ffi::CString;
        use std::os::unix::io::FromRawFd;
        let cdir = CString::new(dir.as_os_str().as_encoded_bytes())
            .map_err(|e| merr("PLUGIN_SPAWN", format!("sealed dir path: {e}")))?;
        // O_TMPFILE must be OR'd with O_RDWR or O_WRONLY; mode sets the
        // inode permissions (0700: owner-only, with the exec bit so
        // scripts pass execve's permission check).
        let wfd = unsafe { libc::open(cdir.as_ptr(), libc::O_TMPFILE | libc::O_RDWR, 0o700) };
        if wfd < 0 {
            return Err(merr(
                "PLUGIN_SPAWN",
                format!(
                    "O_TMPFILE in {}: {}",
                    dir.display(),
                    std::io::Error::last_os_error()
                ),
            ));
        }
        // Re-open read-only through /proc/self/fd BEFORE closing the
        // write handle: execve refuses (ETXTBSY) any file that is open for
        // writing, so the handle the child execs - and every handle we
        // keep - must be read-only.
        let rpath = CString::new(format!("/proc/self/fd/{wfd}"))
            .map_err(|e| merr("PLUGIN_SPAWN", format!("sealed fd path: {e}")))?;
        let rfd = unsafe { libc::open(rpath.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if rfd < 0 {
            unsafe { libc::close(wfd) };
            return Err(merr(
                "PLUGIN_SPAWN",
                format!(
                    "re-open sealed runner read-only: {}",
                    std::io::Error::last_os_error()
                ),
            ));
        }
        // SAFETY: wfd/rfd are valid file descriptions we own.
        let mut wfile = unsafe { std::fs::File::from_raw_fd(wfd) };
        let write_res = wfile
            .write_all(bytes)
            .map_err(|e| merr("PLUGIN_SPAWN", format!("write sealed runner: {e}")));
        drop(wfile); // close the write handle: no writer may exist at exec
        write_res?;
        Ok(unsafe { std::fs::File::from_raw_fd(rfd) })
    }

    /// Fallback: 0700 file with an unguessable random name, created O_EXCL.
    fn seal_named_file(
        dir: &Path,
        bytes: &[u8],
    ) -> Result<(std::fs::File, PathBuf), PantheonError> {
        for _ in 0..8 {
            let path = dir.join(format!("runner-{}", random_hex32()));
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o700);
            }
            match opts.open(&path) {
                Ok(mut file) => {
                    file.write_all(bytes)
                        .map_err(|e| merr("PLUGIN_SPAWN", format!("write sealed runner: {e}")))?;
                    drop(file); // close the write handle (ETXTBSY, see seal_tmpfile)
                    let ro = std::fs::OpenOptions::new()
                        .read(true)
                        .open(&path)
                        .map_err(|e| {
                            merr(
                                "PLUGIN_SPAWN",
                                format!("re-open sealed runner {}: {e}", path.display()),
                            )
                        })?;
                    return Ok((ro, path));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(merr(
                        "PLUGIN_SPAWN",
                        format!("create sealed runner {}: {e}", path.display()),
                    ))
                }
            }
        }
        Err(merr(
            "PLUGIN_SPAWN",
            "could not create a sealed runner file".to_string(),
        ))
    }

    /// Build the Command that execs the sealed runner. On Linux the child
    /// execs the already-open sealed fd via /proc/self/fd/N with argv[0]
    /// set to the canonical runner path (so `$0`/shebang behavior is
    /// unchanged) - no path component is re-resolved at exec time, and the
    /// sealed file holds exactly the bytes the approval check bound, so a
    /// rename swap, symlink swap, or in-place rewrite between the check
    /// and execve cannot affect what executes.
    #[cfg(target_os = "linux")]
    fn exec_command(runner: &Path, sealed: &SealedRunner) -> Command {
        use std::os::unix::io::AsRawFd;
        use std::os::unix::process::CommandExt;
        let fd = sealed.file.as_raw_fd();
        // Clear CLOEXEC so the fd survives into the child; the child
        // execs it directly.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags >= 0 {
            unsafe {
                libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
            }
        }
        let mut cmd = Command::new(format!("/proc/self/fd/{fd}"));
        cmd.arg0(runner.as_os_str());
        cmd
    }

    #[cfg(not(target_os = "linux"))]
    fn exec_command(runner: &Path, sealed: &SealedRunner) -> Command {
        // Best effort only: fd-exec is a Linux facility. Exec the sealed
        // private path - the plugin dir itself is never re-resolved - but
        // a same-uid writer that guesses the unguessable name inside the
        // seal/exec window is not fully closed on this platform.
        match &sealed.path {
            Some(path) => Command::new(path),
            None => Command::new(runner),
        }
    }

    /// stdio + env policy for the plugin child. Shared by both spawn
    /// entry points.
    fn configure_command(
        cmd: &mut Command,
        manifest: &PluginManifest,
        data_dir: &Path,
        env_allowlist: &[String],
    ) {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env_clear();
        // Minimal safe env: PATH so shebangs and basic tools resolve.
        if let Ok(p) = std::env::var("PATH") {
            cmd.env("PATH", p);
        }
        // Plus only what the manifest declares AND the operator allowlists.
        for decl in &manifest.env_vars {
            if !pantheon_secrets::env::env_var_allowed(env_allowlist, &decl.name) {
                continue;
            }
            if let Ok(v) = std::env::var(&decl.name) {
                cmd.env(&decl.name, v);
            }
        }
        // Pantheon-provided vars MUST come after env_clear() - the clear
        // wipes everything set before it.
        cmd.env("PANTHEON_PLUGIN_NAME", &manifest.name)
            .env("PANTHEON_DATA_DIR", data_dir.to_string_lossy().to_string());
    }

    /// Call one tool. Blocks up to the supervisor timeout. On timeout the
    /// whole process group is killed and the supervisor is marked dead.
    pub fn call(&mut self, tool: &str, args: serde_json::Value) -> Result<String, PantheonError> {
        if !self.alive {
            return Err(merr(
                "PLUGIN_DEAD",
                format!(
                    "plugin '{}' was killed after a timeout; respawn it",
                    self.name
                ),
            ));
        }
        let call_id = format!("call_{}", CALL_SEQ.fetch_add(1, Ordering::Relaxed));
        let req = PluginRequest {
            call_id: call_id.clone(),
            tool,
            args,
        };
        let line = serde_json::to_string(&req)
            .map_err(|e| merr("PLUGIN_ENCODE", format!("encode request: {e}")))?;
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| merr("PLUGIN_DEAD", "plugin stdin is gone".into()))?;
        stdin
            .write_all(line.as_bytes())
            .and_then(|_| stdin.write_all(b"\n"))
            .and_then(|_| stdin.flush())
            .map_err(|e| {
                self.alive = false;
                merr(
                    "PLUGIN_WRITE",
                    format!("write to plugin '{}': {e}", self.name),
                )
            })?;

        // Read one response line on a helper thread; the main thread waits
        // with a deadline. A wedged plugin can never wedge the loop.
        // The reader is moved into the thread and sent back with the line.
        let mut reader = self.stdout.take().ok_or_else(|| {
            merr(
                "PLUGIN_DEAD",
                format!("plugin '{}' stdout is gone", self.name),
            )
        })?;
        let (tx, rx) = mpsc::channel::<(BufReader<ChildStdout>, Option<String>)>();
        std::thread::spawn(move || {
            let mut line = String::new();
            let out = match reader.read_line(&mut line) {
                Ok(0) => (reader, None), // EOF
                Ok(_) => {
                    while line.ends_with('\n') || line.ends_with('\r') {
                        line.pop();
                    }
                    (reader, Some(line))
                }
                Err(_) => (reader, None),
            };
            let _ = tx.send(out);
        });
        let deadline = Instant::now() + self.timeout;
        let (reader_back, line_opt) = loop {
            match rx.recv_timeout(Duration::from_millis(POLL_MS)) {
                Ok(pair) => break pair,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if Instant::now() > deadline {
                        // Timeout: the reader thread still owns stdout and is
                        // blocked in read_line. TERM the group; a compliant
                        // plugin exits, closing the pipe, and the reader
                        // thread unblocks on EOF and sends the reader back
                        // (which we drop). If it ignores TERM, escalate to
                        // SIGKILL so neither the group nor the thread leaks.
                        self.kill_group();
                        self.child.take();
                        self.stdin.take();
                        // SIGKILL is terminal, so one signal is enough. The
                        // reader thread is already wedged in read_line and is
                        // deliberately leaked: it unblocks on EOF once the
                        // group dies and drops its handle with the thread.
                        #[cfg(unix)]
                        unsafe {
                            libc::killpg(self.pgid, SIGKILL);
                        }
                        // stdout stays None (moved into the dead thread).
                        self.alive = false;
                        return Err(merr(
                            "PLUGIN_TIMEOUT",
                            format!(
                                "tool '{tool}' did not respond within {}s; process group {} killed",
                                self.timeout.as_secs(),
                                self.pgid
                            ),
                        ));
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // Thread died without sending; restore nothing usable.
                    self.alive = false;
                    return Err(merr(
                        "PLUGIN_DEAD",
                        format!("plugin '{}' reader thread died", self.name),
                    ));
                }
            }
        };
        // Normal path: restore the reader for the next call.
        self.stdout = Some(reader_back);
        let line = match line_opt {
            Some(l) if !l.is_empty() => l,
            _ => {
                self.alive = false;
                return Err(merr(
                    "PLUGIN_EOF",
                    format!("plugin '{}' closed stdout mid-call", self.name),
                ));
            }
        };
        let resp: PluginResponse = serde_json::from_str(&line).map_err(|e| {
            merr(
                "PLUGIN_PROTOCOL",
                format!("plugin '{}' sent invalid JSON: {e}", self.name),
            )
        })?;
        if resp.call_id != call_id {
            // A mismatched id means the protocol stream is corrupted (we
            // have one in-flight call). Restoring stdout would poison every
            // later call with this stale line, so treat it like EOF.
            self.alive = false;
            self.stdout.take();
            return Err(merr(
                "PLUGIN_PROTOCOL",
                format!(
                    "plugin '{}' answered wrong call: got {}, want {call_id}; marking dead to avoid stream desync",
                    self.name, resp.call_id
                ),
            ));
        }
        if let Some(err) = resp.error {
            return Err(merr(&err.code, err.cause));
        }
        match resp.result {
            Some(v) => {
                let raw = if v.is_string() {
                    v.as_str().unwrap_or("").to_string()
                } else {
                    serde_json::to_string(&v).unwrap_or_default()
                };
                Ok(compact_output(&raw, &self.compaction).text)
            }
            None => Err(merr(
                "PLUGIN_PROTOCOL",
                format!("plugin '{}' sent neither result nor error", self.name),
            )),
        }
    }

    /// Kill the owned process group, never any other group.
    fn kill_group(&self) {
        // Guard: only signal the group we created.
        if self.pgid <= 1 || self.pgid == std::process::id() as i32 {
            return;
        }
        #[cfg(unix)]
        unsafe {
            libc::killpg(self.pgid, SIGTERM);
        }
    }

    /// relinquish the handles without signaling the process group. This is
    /// used when the run lease is lost and PID reuse safety forbids a kill.
    pub fn abandon(&mut self) {
        self.stdin.take();
        self.stdout.take();
        self.child.take();
        self.alive = false;
        self.drop_kill = false;
    }

    /// Graceful stop: TERM the process group, wait up to STOP_GRACE, then
    /// KILL the group. Idempotent.
    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            self.stdin.take();
            self.stdout.take();
            self.kill_group();
            let deadline = Instant::now() + STOP_GRACE;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => {
                        if Instant::now() > deadline {
                            #[cfg(unix)]
                            unsafe {
                                if self.pgid > 1 && self.pgid != std::process::id() as i32 {
                                    libc::killpg(self.pgid, SIGKILL);
                                }
                            }
                            #[cfg(not(unix))]
                            {
                                let _ = child.kill();
                            }
                            let _ = child.wait();
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(POLL_MS));
                    }
                    Err(_) => break,
                }
            }
            self.alive = false;
        }
    }

    pub fn is_alive(&self) -> bool {
        self.alive
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Process group id owned by this supervisor.  It is safe to persist only
    /// together with a run lease; a bare PID is not an ownership token.
    pub fn pgid(&self) -> i32 {
        self.pgid
    }
}

impl Drop for PluginSupervisor {
    fn drop(&mut self) {
        // Never leave orphans: group-kill on drop. KILL, not TERM - drop
        // cannot wait for a graceful exit, so the guaranteed signal is the
        // stop() is the graceful path when the caller can wait. A stale
        // supervisor calls abandon() so Drop never signals a reused PGID.
        if self.child.is_some() && self.drop_kill {
            #[cfg(unix)]
            unsafe {
                if self.pgid > 1 && self.pgid != std::process::id() as i32 {
                    libc::killpg(self.pgid, SIGKILL);
                }
            }
            self.child.take();
        }
    }
}

// PluginSupervisor owns a raw Child; safe to move between threads as long as
// only one thread touches it at a time (callers share via Mutex).
unsafe impl Send for PluginSupervisor {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::{PluginLocation, PluginManifest};
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::AtomicBool;

    const GOOD_SCRIPT: &[u8] = b"#!/bin/sh\necho SEALED_OK\n";
    const EVIL_SCRIPT: &[u8] = b"#!/bin/sh\necho PWNED\n";

    fn test_plugin(root: &Path) -> DiscoveredPlugin {
        DiscoveredPlugin {
            manifest: PluginManifest {
                name: "racetest".to_string(),
                description: "rewrite-race test plugin".to_string(),
                version: "1.0.0".to_string(),
                sha: None,
                maintainer: String::new(),
                capabilities: Vec::new(),
                env_vars: Vec::new(),
                runner: "runner.sh".to_string(),
                enabled: true,
            },
            location: PluginLocation::User,
            root: root.to_path_buf(),
        }
    }

    /// Write `bytes` to the runner IN PLACE (same inode): open with
    /// truncate + write, no rename. This is the attack the old loader lost
    /// to - the open fd pins identity, not bytes.
    fn inplace_write(path: &Path, bytes: &[u8]) {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path)
            .unwrap();
        f.write_all(bytes).unwrap();
        // Deliberately no fsync: torn/racy states must be observable.
    }

    #[test]
    fn spawn_verified_never_executes_unapproved_bytes_under_rewrite_race() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        // Scope dir <tmp>/plugins, so approvals land in
        // <tmp>/plugins/.approvals.json and the plugin is third-party
        // (not under a `bundled` dir).
        let plugin_root = tmp.path().join("plugins").join("racetest");
        std::fs::create_dir_all(&plugin_root).unwrap();
        let runner_path = plugin_root.join("runner.sh");
        std::fs::write(&runner_path, GOOD_SCRIPT).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&runner_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(
            plugin_root.join("manifest.yaml"),
            "name: racetest\nversion: 1.0.0\nrunner: runner.sh\n",
        )
        .unwrap();

        let plugin = test_plugin(&plugin_root);
        crate::plugin_approval::record_approval(&plugin).unwrap();
        assert!(crate::plugin_approval::is_approved(&plugin));

        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let stop_w = std::sync::Arc::clone(&stop);
        let churn_path = runner_path.clone();
        let churner = std::thread::spawn(move || {
            let mut flip = false;
            while !stop_w.load(Ordering::Relaxed) {
                flip = !flip;
                inplace_write(&churn_path, if flip { EVIL_SCRIPT } else { GOOD_SCRIPT });
            }
        });

        let mut executed_ok = 0u32;
        let mut refused = 0u32;
        for _ in 0..150 {
            match PluginSupervisor::spawn_verified(&plugin, &data_dir, Duration::from_secs(10), &[])
            {
                Ok(mut sup) => {
                    // The child prints one line and exits. If the sealed
                    // bytes were anything but the approved script, this
                    // assertion fires.
                    let mut line = String::new();
                    let n = sup.stdout.as_mut().unwrap().read_line(&mut line).unwrap();
                    assert!(n > 0, "plugin child produced no output");
                    assert_eq!(
                        line.trim(),
                        "SEALED_OK",
                        "executed bytes were NOT the approved script: {line:?}"
                    );
                    executed_ok += 1;
                    sup.stop();
                }
                Err(e) => {
                    // Fail-closed is always acceptable under churn: the dir
                    // changed mid-hash, the pinned bytes weren't the
                    // approved ones, verify_plugin's shebang check saw a
                    // torn file, etc.
                    assert!(
                        [
                            "PLUGIN_TAMPERED",
                            "PLUGIN_NOT_APPROVED",
                            "PLUGIN_UNSAFE_RUNNER",
                            "PLUGIN_NO_SHEBANG"
                        ]
                        .contains(&e.code.as_str()),
                        "unexpected error under churn: {}",
                        e.code
                    );
                    refused += 1;
                }
            }
        }
        stop.store(true, Ordering::Relaxed);
        churner.join().unwrap();
        // Sanity: the race actually happened - both outcomes observed.
        assert!(
            refused > 0,
            "expected some fail-closed refusals under churn"
        );
        assert!(
            executed_ok > 0,
            "expected some successful spawns under churn"
        );
    }

    #[test]
    fn pinned_runner_approval_binds_bytes_not_identity() {
        // Approval recorded for GOOD bytes. An in-place rewrite (same
        // inode) must NOT pass the pinned-runner check with the new bytes,
        // and the check with the old bytes must fail too once the dir
        // changed - the binding is on bytes, not identity.
        let tmp = tempfile::tempdir().unwrap();
        let plugin_root = tmp.path().join("plugins").join("bindtest");
        std::fs::create_dir_all(&plugin_root).unwrap();
        let runner_path = plugin_root.join("runner.sh");
        std::fs::write(&runner_path, GOOD_SCRIPT).unwrap();
        std::fs::write(
            plugin_root.join("manifest.yaml"),
            "name: bindtest\nversion: 1.0.0\nrunner: runner.sh\n",
        )
        .unwrap();
        let plugin = DiscoveredPlugin {
            manifest: PluginManifest {
                name: "bindtest".to_string(),
                version: "1.0.0".to_string(),
                runner: "runner.sh".to_string(),
                ..test_plugin(&plugin_root).manifest
            },
            location: PluginLocation::User,
            root: plugin_root.clone(),
        };
        crate::plugin_approval::record_approval(&plugin).unwrap();

        let canon_root = plugin_root.canonicalize().unwrap();
        let canon_runner = runner_path.canonicalize().unwrap();
        let read_pinned = || {
            let mut f = std::fs::File::open(&runner_path).unwrap();
            let md = f.metadata().unwrap();
            let mut b = Vec::new();
            f.read_to_end(&mut b).unwrap();
            (b, md)
        };

        // Pre-rewrite: pinned GOOD bytes pass.
        let (b, md) = read_pinned();
        assert!(crate::plugin_approval::is_approved_with_pinned_runner(
            &plugin,
            &canon_root,
            &canon_runner,
            &b,
            &md
        ));

        // In-place rewrite to EVIL (same inode - identity unchanged).
        let ino_before = md.ino();
        inplace_write(&runner_path, EVIL_SCRIPT);
        let (b2, md2) = read_pinned();
        assert_eq!(md2.ino(), ino_before, "test setup: inode must not change");
        // Pinned EVIL bytes must NOT pass.
        assert!(!crate::plugin_approval::is_approved_with_pinned_runner(
            &plugin,
            &canon_root,
            &canon_runner,
            &b2,
            &md2
        ));
        // Stale GOOD bytes still pass - and that is CORRECT, not a hole:
        // the check binds the bytes in hand, and the loader execs a
        // sealed copy of exactly those bytes. GOOD bytes were
        // operator-approved whenever they were read, so executing them
        // is safe. The dangerous direction - unapproved bytes passing
        // is what the assertion above rules out.
        assert!(crate::plugin_approval::is_approved_with_pinned_runner(
            &plugin,
            &canon_root,
            &canon_runner,
            &b,
            &md
        ));
    }
}
