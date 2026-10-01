//! Silent user-scope service install for the gateway.
//!
//! Hermes installs its gateway service unconditionally at setup, in user
//! scope, without prompting — "always safe to call". This module is
//! Pantheon's equivalent. [`install`] detects the platform's service
//! manager at runtime and installs accordingly:
//!
//! - Linux + systemd → user unit `~/.config/systemd/user/pantheon-gateway.service`
//!   + `systemctl --user enable --now` (falls back to cron when the user
//!     bus is unavailable, e.g. containers)
//! - Linux without systemd → `crontab` `@reboot` entry, merged idempotently
//! - macOS → LaunchAgent plist under `~/Library/LaunchAgents` + `launchctl bootstrap`
//! - Windows → Task Scheduler logon task via `schtasks /Create` (user
//!   scope, no admin; `/F` makes re-runs idempotent)
//! - anything else → fail open with a one-line note showing the manual
//!   `pantheon schedule tick` cron line
//!
//! Re-running install when already installed is a no-op that ensures the
//! service is enabled and running. Nothing here prompts; nothing here
//! panics on a missing manager.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// systemd unit name / launchd-agnostic service name.
pub const UNIT_NAME: &str = "pantheon-gateway";
/// macOS LaunchAgent label.
pub const LAUNCHD_LABEL: &str = "africa.exoseed.pantheon.gateway";
/// Windows Task Scheduler task name.
pub const TASK_NAME: &str = "PantheonGateway";
/// Marker appended to the cron line we own, so the merge can find and
/// replace it idempotently without touching the user's other entries.
pub const CRON_MARKER: &str = "# pantheon-gateway";

/// Which background mechanism this platform uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mechanism {
    Systemd,
    Cron,
    Launchd,
    TaskScheduler,
    None,
}

impl Mechanism {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mechanism::Systemd => "systemd",
            Mechanism::Cron => "cron",
            Mechanism::Launchd => "launchd",
            Mechanism::TaskScheduler => "task-scheduler",
            Mechanism::None => "none",
        }
    }
}

/// Injectable host environment so tests can redirect unit dirs and PATH
/// without touching the real system.
#[derive(Debug, Clone)]
pub struct InstallEnv {
    /// Directories searched for helper binaries, in order. Real: `PATH`.
    pub path_dirs: Vec<PathBuf>,
    /// systemd user unit dir. Real: `$XDG_CONFIG_HOME/systemd/user` or
    /// `~/.config/systemd/user`. Honors `SYSTEMD_USER_CONFIG_DIR`.
    pub systemd_user_dir: PathBuf,
    /// LaunchAgents dir. Real: `~/Library/LaunchAgents`. Honors
    /// `PANTHEON_LAUNCHD_DIR` (test hook).
    pub launchd_agents_dir: PathBuf,
}

impl InstallEnv {
    pub fn real() -> Self {
        let path_dirs = std::env::var_os("PATH")
            .map(|p| std::env::split_paths(&p).collect())
            .unwrap_or_default();
        Self {
            path_dirs,
            systemd_user_dir: systemd_user_dir(),
            launchd_agents_dir: launchd_agents_dir(),
        }
    }

    fn which(&self, bin: &str) -> Option<PathBuf> {
        // On Windows the binary is schtasks.exe; PATH lookup needs the suffix.
        let candidates: Vec<String> = if cfg!(target_os = "windows") && !bin.contains('.') {
            vec![format!("{bin}.exe"), bin.to_string()]
        } else {
            vec![bin.to_string()]
        };
        for dir in &self.path_dirs {
            for name in &candidates {
                let cand = dir.join(name);
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
        None
    }
}

fn systemd_user_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("SYSTEMD_USER_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    base.join("systemd").join("user")
}

fn launchd_agents_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("PANTHEON_LAUNCHD_DIR") {
        return PathBuf::from(d);
    }
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
        .join("Library")
        .join("LaunchAgents")
}

/// Detect the background mechanism for this platform at runtime.
pub fn detect(env: &InstallEnv) -> Mechanism {
    if cfg!(target_os = "windows") {
        return if env.which("schtasks").is_some() {
            Mechanism::TaskScheduler
        } else {
            Mechanism::None
        };
    }
    if cfg!(target_os = "macos") {
        return if env.which("launchctl").is_some() {
            Mechanism::Launchd
        } else {
            Mechanism::None
        };
    }
    if cfg!(target_os = "linux") {
        if env.which("systemctl").is_some() {
            return Mechanism::Systemd;
        }
        if env.which("crontab").is_some() {
            return Mechanism::Cron;
        }
        return Mechanism::None;
    }
    Mechanism::None
}

/// Absolute path to the running binary, resolved from argv[0] with a
/// PATH lookup as fallback. systemd/schtasks need an absolute ExecStart;
/// a bare `pantheon` would resolve against the service's own PATH and can
/// differ from the shell the user typed the command in.
pub fn self_exe() -> Option<PathBuf> {
    if let Ok(p) = std::env::current_exe() {
        if p.is_absolute() && p.exists() {
            return Some(p);
        }
    }
    let name = std::env::args().next()?;
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let cand = dir.join(&name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

/// Render the systemd user unit. Pure: the file content is a pure function
/// of the binary and data dir, which is what makes install idempotent.
pub fn render_systemd_unit(exe: &Path, data_dir: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=Pantheon gateway (chat surfaces + scheduled tasks)\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} gateway run\n\
         Environment=PANTHEON_DATA_DIR={data_dir}\n\
         Restart=always\n\
         RestartSec=5\n\
         # The gateway executes whatever a message asks for on this host.\n\
         # The allowlist in .env is the real control; this is defence in depth.\n\
         NoNewPrivileges=true\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = exe.display(),
        data_dir = data_dir.display(),
    )
}

/// Render the macOS LaunchAgent plist. Pure.
pub fn render_launchd_plist(exe: &Path, data_dir: &Path) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \n\
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20 <key>Label</key><string>{LAUNCHD_LABEL}</string>\n\
         \x20 <key>ProgramArguments</key>\n\
         \x20 <array><string>{exe}</string><string>gateway</string><string>run</string></array>\n\
         \x20 <key>EnvironmentVariables</key>\n\
         \x20 <dict><key>PANTHEON_DATA_DIR</key><string>{data_dir}</string></dict>\n\
         \x20 <key>RunAtLoad</key><true/>\n\
         \x20 <key>KeepAlive</key><true/>\n\
         </dict>\n\
         </plist>\n",
        exe = exe.display(),
        data_dir = data_dir.display(),
    )
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

/// Render the Windows Task Scheduler task XML (Task Scheduler 2.0 schema).
/// Logon trigger, least-privilege principal, no admin required. Pure.
pub fn render_task_xml(exe: &Path, data_dir: &Path) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n\
         <Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n\
         \x20 <RegistrationInfo>\n\
         \x20\x20 <Description>Pantheon gateway (chat surfaces + scheduled tasks)</Description>\n\
         \x20 </RegistrationInfo>\n\
         \x20 <Triggers>\n\
         \x20\x20 <LogonTrigger>\n\
         \x20\x20\x20 <Enabled>true</Enabled>\n\
         \x20\x20 </LogonTrigger>\n\
         \x20 </Triggers>\n\
         \x20 <Principals>\n\
         \x20\x20 <Principal id=\"Author\">\n\
         \x20\x20\x20 <LogonType>InteractiveToken</LogonType>\n\
         \x20\x20\x20 <RunLevel>LeastPrivilege</RunLevel>\n\
         \x20\x20 </Principal>\n\
         \x20 </Principals>\n\
         \x20 <Settings>\n\
         \x20\x20 <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n\
         \x20\x20 <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n\
         \x20\x20 <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n\
         \x20\x20 <AllowHardTerminate>true</AllowHardTerminate>\n\
         \x20\x20 <StartWhenAvailable>true</StartWhenAvailable>\n\
         \x20\x20 <Enabled>true</Enabled>\n\
         \x20 </Settings>\n\
         \x20 <Actions Context=\"Author\">\n\
         \x20\x20 <Exec>\n\
         \x20\x20\x20 <Command>{exe}</Command>\n\
         \x20\x20\x20 <Arguments>gateway run</Arguments>\n\
         \x20\x20\x20 <WorkingDirectory>{data_dir}</WorkingDirectory>\n\
         \x20\x20 </Exec>\n\
         \x20 </Actions>\n\
         </Task>\n",
        exe = xml_escape(&exe.display().to_string()),
        data_dir = xml_escape(&data_dir.display().to_string()),
    )
}

/// The cron line we own: `@reboot` starts the gateway (scheduler included)
/// when the machine boots. The marker makes the merge idempotent.
pub fn cron_line(exe: &Path) -> String {
    format!("@reboot {} gateway run {CRON_MARKER}", exe.display())
}

/// Merge our cron line into an existing crontab. Idempotent: any previous
/// line carrying our marker is removed first, so installing twice yields
/// byte-identical output. The user's other entries are untouched.
pub fn merge_crontab(existing: &str, line: &str) -> String {
    let mut kept: Vec<&str> = existing
        .lines()
        .filter(|l| !l.contains(CRON_MARKER))
        .collect();
    kept.push(line);
    let mut out = kept.join("\n");
    out.push('\n');
    out
}

/// Manual fallback line shown when no service manager exists: a plain cron
/// entry that fires due jobs every five minutes.
pub fn manual_cron_line(exe: &Path) -> String {
    format!("*/5 * * * * {} schedule tick {CRON_MARKER}", exe.display())
}

fn run_cmd(env: &InstallEnv, bin: &str, args: &[&str]) -> Result<(), String> {
    let abs = env
        .which(bin)
        .ok_or_else(|| format!("{bin} not found on PATH"))?;
    let status = Command::new(&abs)
        .args(args)
        .status()
        .map_err(|e| format!("run {bin}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{bin} {} failed: {status}", args.join(" ")))
    }
}

fn run_capture(env: &InstallEnv, bin: &str, args: &[&str]) -> Result<String, String> {
    let abs = env
        .which(bin)
        .ok_or_else(|| format!("{bin} not found on PATH"))?;
    let out = Command::new(&abs)
        .args(args)
        .output()
        .map_err(|e| format!("run {bin}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!("{bin} {} failed: {}", args.join(" "), out.status))
    }
}

fn install_systemd(env: &InstallEnv, data_dir: &Path, exe: &Path) -> Result<bool, String> {
    let dir = &env.systemd_user_dir;
    let path = dir.join(format!("{UNIT_NAME}.service"));
    let body = render_systemd_unit(exe, data_dir);
    let changed = std::fs::read_to_string(&path)
        .map(|old| old != body)
        .unwrap_or(true);
    if changed {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        std::fs::write(&path, body).map_err(|e| format!("write {}: {e}", path.display()))?;
        // daemon-reload picks up a unit that did not exist when the
        // manager last read its directory.
        run_cmd(env, "systemctl", &["--user", "daemon-reload"])?;
    }
    // enable --now is idempotent: re-running only ensures enabled+running.
    run_cmd(env, "systemctl", &["--user", "enable", "--now", UNIT_NAME])?;
    Ok(changed)
}

fn current_uid(env: &InstallEnv) -> Result<String, String> {
    let out = run_capture(env, "id", &["-u"])?;
    let uid = out.trim().to_string();
    if uid.is_empty() || !uid.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("unexpected `id -u` output: {uid:?}"));
    }
    Ok(uid)
}

fn install_launchd(env: &InstallEnv, data_dir: &Path, exe: &Path) -> Result<bool, String> {
    let dir = &env.launchd_agents_dir;
    let path = dir.join(format!("{LAUNCHD_LABEL}.plist"));
    let body = render_launchd_plist(exe, data_dir);
    let changed = std::fs::read_to_string(&path)
        .map(|old| old != body)
        .unwrap_or(true);
    if changed {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        std::fs::write(&path, body).map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    let uid = current_uid(env)?;
    let domain = format!("gui/{uid}");
    // bootout first (ignored when not loaded) so re-install is idempotent:
    // bootstrap refuses a label that is already bootstrapped.
    let _ = run_cmd(
        env,
        "launchctl",
        &["bootout", &format!("{domain}/{LAUNCHD_LABEL}")],
    );
    run_cmd(
        env,
        "launchctl",
        &["bootstrap", &domain, &path.to_string_lossy()],
    )?;
    Ok(changed)
}

fn install_cron(env: &InstallEnv, exe: &Path) -> Result<bool, String> {
    let line = cron_line(exe);
    // `crontab -l` exits non-zero when the user has no crontab yet; that
    // just means "empty table", not an error.
    let current = run_capture(env, "crontab", &["-l"]).unwrap_or_default();
    let merged = merge_crontab(&current, &line);
    if merged.trim_end() == current.trim_end() {
        return Ok(false);
    }
    let abs = env.which("crontab").ok_or("crontab not found on PATH")?;
    let mut child = Command::new(&abs)
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("run crontab: {e}"))?;
    child
        .stdin
        .as_mut()
        .ok_or("crontab stdin unavailable")?
        .write_all(merged.as_bytes())
        .map_err(|e| format!("write crontab: {e}"))?;
    let status = child.wait().map_err(|e| format!("wait crontab: {e}"))?;
    if status.success() {
        Ok(true)
    } else {
        Err(format!("crontab - failed: {status}"))
    }
}

fn install_task_scheduler(env: &InstallEnv, data_dir: &Path, exe: &Path) -> Result<bool, String> {
    let xml = render_task_xml(exe, data_dir);
    let tmp = std::env::temp_dir().join("pantheon-gateway-task.xml");
    std::fs::write(&tmp, xml).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    // /F overwrites an existing task, so re-running converges.
    let result = run_cmd(
        env,
        "schtasks",
        &[
            "/Create",
            "/TN",
            TASK_NAME,
            "/XML",
            &tmp.to_string_lossy(),
            "/F",
        ],
    );
    let _ = std::fs::remove_file(&tmp);
    result.map(|()| true)
}

/// What [`install`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    /// Installed (or already installed) and ensured running.
    Installed {
        mechanism: Mechanism,
        /// False when nothing changed: the previous install was already current.
        changed: bool,
    },
    /// No service manager on this platform. The note is the one-line
    /// fail-open message for the user.
    Unavailable { note: String },
    /// A manager was found but the install failed.
    Failed { mechanism: Mechanism, error: String },
}

/// Install and start the gateway as a user background service.
///
/// Idempotent, never prompts, fails open: always safe to call. On Linux,
/// a `systemctl` without a working user bus (containers) falls back to
/// the cron `@reboot` entry automatically.
pub fn install(data_dir: &Path, exe: &Path) -> InstallOutcome {
    install_with(&InstallEnv::real(), data_dir, exe)
}

/// [`install`] with an injectable environment (tests).
pub fn install_with(env: &InstallEnv, data_dir: &Path, exe: &Path) -> InstallOutcome {
    match detect(env) {
        Mechanism::Systemd => match install_systemd(env, data_dir, exe) {
            Ok(changed) => InstallOutcome::Installed {
                mechanism: Mechanism::Systemd,
                changed,
            },
            Err(_) if env.which("crontab").is_some() => match install_cron(env, exe) {
                Ok(changed) => InstallOutcome::Installed {
                    mechanism: Mechanism::Cron,
                    changed,
                },
                Err(e2) => InstallOutcome::Failed {
                    mechanism: Mechanism::Cron,
                    error: e2,
                },
            },
            Err(e) => InstallOutcome::Failed {
                mechanism: Mechanism::Systemd,
                error: e,
            },
        },
        Mechanism::Cron => match install_cron(env, exe) {
            Ok(changed) => InstallOutcome::Installed {
                mechanism: Mechanism::Cron,
                changed,
            },
            Err(e) => InstallOutcome::Failed {
                mechanism: Mechanism::Cron,
                error: e,
            },
        },
        Mechanism::Launchd => match install_launchd(env, data_dir, exe) {
            Ok(changed) => InstallOutcome::Installed {
                mechanism: Mechanism::Launchd,
                changed,
            },
            Err(e) => InstallOutcome::Failed {
                mechanism: Mechanism::Launchd,
                error: e,
            },
        },
        Mechanism::TaskScheduler => match install_task_scheduler(env, data_dir, exe) {
            Ok(changed) => InstallOutcome::Installed {
                mechanism: Mechanism::TaskScheduler,
                changed,
            },
            Err(e) => InstallOutcome::Failed {
                mechanism: Mechanism::TaskScheduler,
                error: e,
            },
        },
        Mechanism::None => InstallOutcome::Unavailable {
            note: format!(
                "no service manager found; for scheduled tasks add to cron: {}",
                manual_cron_line(exe)
            ),
        },
    }
}

/// Point-in-time service state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStatus {
    /// What this platform would use for a fresh install.
    pub detected: Mechanism,
    /// The mechanism actually installed, if any.
    pub installed: Option<Mechanism>,
    /// Whether it is currently running (cron counts as running: cron owns it).
    pub running: bool,
}

/// Inspect the service without changing anything: status never installs.
pub fn status() -> ServiceStatus {
    status_with(&InstallEnv::real())
}

/// [`status`] with an injectable environment (tests).
pub fn status_with(env: &InstallEnv) -> ServiceStatus {
    let detected = detect(env);
    // systemd user unit
    if env
        .systemd_user_dir
        .join(format!("{UNIT_NAME}.service"))
        .exists()
    {
        let running = run_capture(env, "systemctl", &["--user", "is-active", UNIT_NAME])
            .map(|s| s.trim() == "active")
            .unwrap_or(false);
        return ServiceStatus {
            detected,
            installed: Some(Mechanism::Systemd),
            running,
        };
    }
    // launchd plist
    if env
        .launchd_agents_dir
        .join(format!("{LAUNCHD_LABEL}.plist"))
        .exists()
    {
        let running = current_uid(env)
            .ok()
            .and_then(|uid| {
                run_capture(
                    env,
                    "launchctl",
                    &["print", &format!("gui/{uid}/{LAUNCHD_LABEL}")],
                )
                .ok()
            })
            .is_some_and(|out| out.contains("state = running"));
        return ServiceStatus {
            detected,
            installed: Some(Mechanism::Launchd),
            running,
        };
    }
    // cron @reboot entry
    if let Ok(cur) = run_capture(env, "crontab", &["-l"]) {
        if cur.lines().any(|l| l.contains(CRON_MARKER)) {
            return ServiceStatus {
                detected,
                installed: Some(Mechanism::Cron),
                running: true,
            };
        }
    }
    // windows task scheduler
    if let Ok(out) = run_capture(
        env,
        "schtasks",
        &["/Query", "/TN", TASK_NAME, "/FO", "LIST"],
    ) {
        // schtasks /Query /FO LIST prints "Status: Running" for a live task.
        let running = out.lines().any(|l| {
            let l = l.trim_start();
            l.starts_with("Status:") && l.contains("Running")
        });
        return ServiceStatus {
            detected,
            installed: Some(Mechanism::TaskScheduler),
            running,
        };
    }
    ServiceStatus {
        detected,
        installed: None,
        running: false,
    }
}

// ── gateway run startup ─────────────────────────────────────────────────
// `gateway run` is both the chat gateway and the always-on scheduler. The
// scheduler loop starts unconditionally — a scheduler-only install (no bot
// tokens) is the main always-on use case — while each chat surface starts
// only when its token and the allowlist are present. The old behavior was
// exit(2) when anything was missing, which turned a scheduler-only service
// into a crash-loop under the service manager.

/// Which chat surfaces `gateway run` should start, decided purely from env
/// values. Pure: no process I/O, no exits — testable without spawning
/// anything. The scheduler loop is deliberately NOT part of this plan: it
/// always starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelPlan {
    pub telegram: bool,
    pub discord: bool,
}

impl ChannelPlan {
    /// Decide from raw env values. A surface starts only when its token is
    /// present AND the allowlist is non-empty: a token without an allowlist
    /// would hand the host to anyone who finds the bot.
    pub fn from_env(
        discord_token: Option<&str>,
        telegram_token: Option<&str>,
        allow: &std::collections::HashSet<String>,
    ) -> Self {
        let token_ok = |t: Option<&str>| t.is_some_and(|s| !s.trim().is_empty());
        let gated = !allow.is_empty();
        Self {
            telegram: token_ok(telegram_token) && gated,
            discord: token_ok(discord_token) && gated,
        }
    }

    /// True when no chat surface starts: scheduler-only mode.
    pub fn is_empty(&self) -> bool {
        !self.telegram && !self.discord
    }
}

/// Read the channel tokens from the secrets store, with the process
/// environment winning when both are set: `(discord_token, telegram_token,
/// allowlist)`. Empty/blank tokens are treated as missing.
///
/// This is what `gateway run` calls. Tokens the Full Setup wizard collects
/// land in `<data_dir>/gateway.env` via the secrets store
/// (`pantheon_secrets::gateway_token`); an exported
/// `PANTHEON_TELEGRAM_BOT_TOKEN` / `PANTHEON_DISCORD_TOKEN` always
/// overrides the stored one. [`read_channel_env`] stays the pure
/// env-only reader for callers that must not touch the filesystem.
/// Callers feed the tokens into [`ChannelPlan`] to decide what starts.
pub fn read_channel_tokens(
    data_dir: &std::path::Path,
) -> (
    Option<String>,
    Option<String>,
    std::collections::HashSet<String>,
) {
    let token = |name: &str| {
        pantheon_secrets::gateway_token(data_dir, name)
            .map(|v| v.expose().to_string())
            .filter(|t| !t.trim().is_empty())
    };
    (
        token(pantheon_secrets::DISCORD_TOKEN_NAME),
        token(pantheon_secrets::TELEGRAM_TOKEN_NAME),
        read_allowlist(),
    )
}

/// Comma-separated platform ids:
///   PANTHEON_GATEWAY_ALLOW=6123456789,223344556677889900
/// The allowlist stays env-only: it is an operator control, not a secret
/// the wizard collects.
fn read_allowlist() -> std::collections::HashSet<String> {
    std::env::var("PANTHEON_GATEWAY_ALLOW")
        .ok()
        .map(|raw| {
            raw.split(',')
                .map(|id| id.trim().to_string())
                .filter(|id| !id.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Read the channel env without exiting: `(discord_token, telegram_token,
/// allowlist)`. Callers feed this into [`ChannelPlan`] to decide what
/// starts. Empty/blank tokens are treated as missing.
pub fn read_channel_env() -> (
    Option<String>,
    Option<String>,
    std::collections::HashSet<String>,
) {
    let present = |name: &str| std::env::var(name).ok().filter(|t| !t.trim().is_empty());
    (
        present("PANTHEON_DISCORD_TOKEN"),
        present("PANTHEON_TELEGRAM_BOT_TOKEN"),
        read_allowlist(),
    )
}

/// One-line warning printed when `gateway run` starts with no chat surface.
/// Printed, not fatal: the scheduler loop is the point of a scheduler-only
/// install.
pub fn channels_disabled_note() -> &'static str {
    "channel surfaces disabled: set PANTHEON_TELEGRAM_BOT_TOKEN or \
     PANTHEON_DISCORD_TOKEN and PANTHEON_GATEWAY_ALLOW to enable chat \
     control; the scheduler loop is running"
}

// ── stop / restart ──────────────────────────────────────────────────────
// The TUI's `gateway stop|restart` and `pantheon init` share this, so the
// two can never disagree about how the service is managed. `status` never
// installs; `stop` never uninstalls.

/// What [`stop_with`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopOutcome {
    /// The service was stopped (the install remains).
    Stopped { mechanism: Mechanism },
    /// Nothing installed.
    NotInstalled,
    /// The mechanism has nothing to stop (a cron `@reboot` entry only
    /// fires at boot): an honest no-op, not a silent success.
    Noop { mechanism: Mechanism, note: String },
    /// A manager was found but the stop failed.
    Failed { mechanism: Mechanism, error: String },
}

/// Stop the background service without removing its install. Idempotent,
/// never prompts, fails open.
pub fn stop() -> StopOutcome {
    stop_with(&InstallEnv::real())
}

/// [`stop`] with an injectable environment (tests).
pub fn stop_with(env: &InstallEnv) -> StopOutcome {
    let installed = match status_with(env).installed {
        Some(m) => m,
        None => return StopOutcome::NotInstalled,
    };
    let res: Result<(), String> = match installed {
        Mechanism::Systemd => run_cmd(env, "systemctl", &["--user", "stop", UNIT_NAME]),
        Mechanism::Launchd => match current_uid(env) {
            Ok(uid) => run_cmd(
                env,
                "launchctl",
                &["bootout", &format!("gui/{uid}/{LAUNCHD_LABEL}")],
            ),
            Err(e) => Err(e),
        },
        Mechanism::Cron => {
            return StopOutcome::Noop {
                mechanism: installed,
                note: "the cron @reboot entry starts the gateway at boot; there is \
                       no running service to stop — kill any foreground \
                       `pantheon gateway run` yourself"
                    .to_string(),
            }
        }
        Mechanism::TaskScheduler => run_cmd(env, "schtasks", &["/End", "/TN", TASK_NAME]),
        Mechanism::None => return StopOutcome::NotInstalled,
    };
    match res {
        Ok(()) => StopOutcome::Stopped {
            mechanism: installed,
        },
        Err(e) => {
            // Idempotent stop: failing to stop something that is not
            // running is success (launchd bootout of an unloaded label,
            // systemctl stop racing a dead unit).
            if !status_with(env).running {
                StopOutcome::Stopped {
                    mechanism: installed,
                }
            } else {
                StopOutcome::Failed {
                    mechanism: installed,
                    error: e,
                }
            }
        }
    }
}

/// What [`restart_with`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartOutcome {
    /// The service was restarted.
    Restarted { mechanism: Mechanism },
    /// The mechanism has nothing to restart (a cron `@reboot` entry only
    /// fires at boot): an honest no-op, not a silent success.
    Noop { mechanism: Mechanism, note: String },
    /// Nothing installed.
    NotInstalled,
    /// A manager was found but the restart failed.
    Failed { mechanism: Mechanism, error: String },
}

/// Restart the background service. Idempotent, never prompts.
pub fn restart() -> RestartOutcome {
    restart_with(&InstallEnv::real())
}

/// [`restart`] with an injectable environment (tests).
pub fn restart_with(env: &InstallEnv) -> RestartOutcome {
    let installed = match status_with(env).installed {
        Some(m) => m,
        None => return RestartOutcome::NotInstalled,
    };
    let res: Result<(), String> = match installed {
        Mechanism::Systemd => run_cmd(env, "systemctl", &["--user", "restart", UNIT_NAME]),
        Mechanism::Launchd => match current_uid(env) {
            Ok(uid) => {
                let domain = format!("gui/{uid}");
                let path = env
                    .launchd_agents_dir
                    .join(format!("{LAUNCHD_LABEL}.plist"));
                // bootout first, ignoring "not loaded": restart must work
                // whether or not the old generation is still up.
                let _ = run_cmd(
                    env,
                    "launchctl",
                    &["bootout", &format!("{domain}/{LAUNCHD_LABEL}")],
                );
                run_cmd(
                    env,
                    "launchctl",
                    &["bootstrap", &domain, &path.to_string_lossy()],
                )
            }
            Err(e) => Err(e),
        },
        Mechanism::Cron => {
            return RestartOutcome::Noop {
                mechanism: installed,
                note: "the cron @reboot entry only fires at boot, so restart is a no-op; \
                       run `pantheon gateway run` in the foreground to pick up changes now"
                    .to_string(),
            }
        }
        Mechanism::TaskScheduler => run_cmd(env, "schtasks", &["/End", "/TN", TASK_NAME])
            .and_then(|()| run_cmd(env, "schtasks", &["/Run", "/TN", TASK_NAME])),
        Mechanism::None => return RestartOutcome::NotInstalled,
    };
    match res {
        Ok(()) => RestartOutcome::Restarted {
            mechanism: installed,
        },
        Err(e) => RestartOutcome::Failed {
            mechanism: installed,
            error: e,
        },
    }
}
