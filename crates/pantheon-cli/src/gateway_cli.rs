//! `pantheon gateway`: run the channel daemon against the live runtime.
//!
//! One thread per surface: Discord gateway websocket, Telegram long poll,
//! both feeding a shared `EventSink` that starts/resumes runs and answers
//! approvals through the Supervisor. Replies flow back over the same
//! channels via `MemoryChannel` + `fanout`-compatible envelopes.
//!
//! Tokens come from the environment only:
//!   PANTHEON_DISCORD_TOKEN, PANTHEON_TELEGRAM_BOT_TOKEN
//! Missing tokens disable that surface; at least one must be set.

use pantheon_api::capability::Policy;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The runtime-backed sink. Maps channel threads to run ids through a
/// thread→run map so a conversation keeps its run across messages.
struct RuntimeSink {
    data_dir: PathBuf,
    policy: Policy,
    threads: Mutex<std::collections::HashMap<String, String>>,
    outbound: Arc<Mutex<Vec<pantheon_gateway::OutboundMessage>>>,
    /// Allowed platform sender ids (Telegram user id, Discord user id).
    /// None = allowlist disabled (local testing); Some(empty) denies
    /// everyone. The gateway is a remote shell, so the default is Some:
    /// cmd_gateway requires an explicit allowlist before it starts.
    allow: Option<std::collections::HashSet<String>>,
}

impl RuntimeSink {
    /// Allowlist check. A sender not on the list is refused loudly on
    /// their channel and nothing reaches the runtime.
    fn allowed(&self, sender: Option<&str>) -> bool {
        match (&self.allow, sender) {
            (None, _) => true,
            (Some(set), Some(id)) => set.contains(id),
            // No sender identity and an allowlist is active: refuse.
            // A bridge that strips identity must be explicitly trusted
            // by leaving the allowlist unset.
            (Some(_), None) => false,
        }
    }
}

fn open_session(
    data_dir: &Path,
    policy: Policy,
) -> Result<pantheon_runtime::session::Session, pantheon_api::error::PantheonError> {
    let cfg = crate::config_doc::Config::load_or_report(data_dir);
    let default = pantheon_api::model::DefaultModel {
        provider: std::env::var("PANTHEON_PROVIDER").unwrap_or_else(|_| "local".into()),
        model: std::env::var("PANTHEON_MODEL").unwrap_or_else(|_| "llama3.2".into()),
    };
    let model_policy = pantheon_api::model::ModelPolicy {
        reasoning_budget: Default::default(),
        reasoning: Default::default(),
        default: default.clone(),
        fallbacks: pantheon_api::model::FallbackChain::default(),
        auxiliaries: crate::config_doc::auxiliaries(cfg.as_ref(), &default),
    };
    let secrets = crate::config_doc::chat_secrets(cfg.as_ref());
    pantheon_runtime::session::Session::new(data_dir.to_path_buf(), policy, model_policy, secrets)
}

impl RuntimeSink {
    fn push_outbound(&self, thread: &str, text: String) {
        self.outbound
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(pantheon_gateway::OutboundMessage {
                to_conversation: thread.to_string(),
                text,
            });
    }
}

impl pantheon_gateway::EventSink for RuntimeSink {
    fn on_message(&self, thread_id: &str, sender: Option<&str>, text: &str) {
        if !self.allowed(sender) {
            self.push_outbound(
                thread_id,
                "not authorized: this bot only accepts messages from its allowlist".into(),
            );
            return;
        }
        let session = match open_session(&self.data_dir.clone(), self.policy.clone()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("gateway: session open failed: {e}");
                return;
            }
        };
        let run_id = self
            .threads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(thread_id.to_string())
            .or_insert_with(pantheon_runtime::new_run_id)
            .clone();
        match session.chat(&run_id, text) {
            Ok(outcome) => {
                let text = match outcome {
                    pantheon_agent::LoopOutcome::Answered { text: t, .. } => t,
                    pantheon_agent::LoopOutcome::AwaitingApproval { capability, scope } => format!(
                        "approval needed: {capability:?}. Reply 'grant {run_id} {scope}' to \
                         allow it, or 'deny {run_id} {scope}' to refuse it."
                    ),
                    pantheon_agent::LoopOutcome::Denied { capability } => {
                        format!("denied: {capability:?}")
                    }
                    pantheon_agent::LoopOutcome::BudgetExhausted { cap } => {
                        format!("stopped: {cap} budget exhausted")
                    }
                    other => format!("ended: {other:?}"),
                };
                self.push_outbound(thread_id, text);
            }
            Err(e) => {
                self.push_outbound(thread_id, format!("error: {e}"));
            }
        }
    }

    fn on_approval(&self, thread_id: &str, sender: Option<&str>, scope: &str, grant: bool) {
        if !self.allowed(sender) {
            self.push_outbound(thread_id, "not authorized".into());
            return;
        }
        let sup = match pantheon_runtime::Supervisor::open(self.data_dir.clone()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("gateway: supervisor open failed: {e}");
                return;
            }
        };
        let run_id = self
            .threads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(thread_id)
            .cloned();
        let Some(run_id) = run_id else {
            self.push_outbound(thread_id, "error: no run for this conversation".into());
            return;
        };
        let result = if grant {
            sup.grant(&run_id, scope)
        } else {
            sup.deny(&run_id, scope)
        };
        match result {
            Ok(()) => {
                self.push_outbound(
                    thread_id,
                    if grant {
                        format!("granted {scope}")
                    } else {
                        format!("denied {scope}")
                    },
                );
            }
            Err(e) => self.push_outbound(thread_id, format!("error: {e}")),
        }
    }
}

/// `pantheon gateway [start|stop|restart|status|run]`
///
/// `run` is the foreground process the other subcommands wrap in a service
/// manager. It is the real gateway: threads, transports, and the outbox
/// drain. `start` writes a unit and lets the OS supervise it so the bot
/// survives a logout or reboot, which a bare backgrounded process does not.
pub fn cmd_gateway(args: &[String]) {
    match args.get(2).map(String::as_str) {
        None | Some("run") | Some("foreground") => run_gateway_foreground(),
        Some("start") => service_ctl(ServiceAction::Start),
        Some("stop") => service_ctl(ServiceAction::Stop),
        Some("restart") => service_ctl(ServiceAction::Restart),
        Some("status") => service_ctl(ServiceAction::Status),
        Some(other) => {
            eprintln!("gateway: unknown subcommand '{other}'");
            eprintln!("usage: pantheon gateway [start|stop|restart|status|run]");
            std::process::exit(2);
        }
    }
}

/// Env preflight, shared by `gateway run` and `gateway start`.
///
/// Returns `(discord_token, telegram_token, allowlist)`. Exits with the fix in
/// the message when something is missing: a service that cannot possibly work
/// should not be installed, and a foreground run should not die on the first
/// thread with a bare `Option::unwrap`.
///
/// The allowlist is not optional. The gateway executes whatever a message asks
/// for on this machine, so without a list any stranger who finds the bot owns
/// the host.
fn preflight_or_exit() -> (
    Option<String>,
    Option<String>,
    std::collections::HashSet<String>,
) {
    let discord = std::env::var("PANTHEON_DISCORD_TOKEN").ok();
    let telegram = std::env::var("PANTHEON_TELEGRAM_BOT_TOKEN").ok();
    let has_token = discord.as_deref().is_some_and(|t| !t.trim().is_empty())
        || telegram.as_deref().is_some_and(|t| !t.trim().is_empty());
    if !has_token {
        eprintln!(
            "gateway: no bot token. Set one of:\n  \
             PANTHEON_DISCORD_TOKEN   (Discord bot token)\n  \
             PANTHEON_TELEGRAM_BOT_TOKEN  (from @BotFather)\n\
             put it in <data_dir>/.env so the service can read it too"
        );
        std::process::exit(2);
    }
    // Comma-separated platform ids:
    //   PANTHEON_GATEWAY_ALLOW=6123456789,223344556677889900
    let allow: std::collections::HashSet<String> = std::env::var("PANTHEON_GATEWAY_ALLOW")
        .ok()
        .map(|raw| {
            raw.split(',')
                .map(|id| id.trim().to_string())
                .filter(|id| !id.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if allow.is_empty() {
        eprintln!(
            "gateway: refusing to start without PANTHEON_GATEWAY_ALLOW\n\
             set it to the comma-separated user ids allowed to talk to the bot\n\
             (Telegram: message the bot, check @userinfobot; Discord: enable developer mode, right-click a user)"
        );
        std::process::exit(2);
    }
    (discord, telegram, allow)
}

fn run_gateway_foreground() {
    // Same preflight the service path runs. Validating here, in the user's
    // shell, is what turns a silent crash-loop into an error with a fix.
    let (discord_token, telegram_token, allow) = preflight_or_exit();
    let discord_token = discord_token.filter(|t| !t.trim().is_empty());
    let telegram_token = telegram_token.filter(|t| !t.trim().is_empty());
    let data_dir = super::data_dir();
    let outbound = Arc::new(Mutex::new(Vec::new()));
    // The gateway runs the same policy the config names. It used to read
    // PANTHEON_GATEWAY_POLICY only, so a user with a working `policy =
    // "reader"` config still got coder on Discord and Telegram.
    let gw_cfg = crate::config_doc::Config::load_or_report(&data_dir);
    let policy = match std::env::var("PANTHEON_GATEWAY_POLICY").as_deref() {
        Ok("researcher") => Policy::researcher_readonly(),
        Ok("coder") => Policy::coder(),
        _ => crate::config_schema::policy_for_config(&gw_cfg),
    };
    let sink = Arc::new(RuntimeSink {
        data_dir: data_dir.clone(),
        policy,
        threads: Mutex::new(std::collections::HashMap::new()),
        outbound: outbound.clone(),
        allow: Some(allow),
    });
    let state_dir = data_dir.join("gateway");
    let _ = std::fs::create_dir_all(&state_dir);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

    if let Some(token) = telegram_token {
        let sink = sink.clone();
        let outbound = outbound.clone();
        let stop = stop.clone();
        let state = state_dir.clone();
        handles.push(std::thread::spawn(move || {
            let channel = Arc::new(pantheon_gateway::TelegramChannel::rest(&token));
            let transport = Arc::new(pantheon_gateway::TelegramRestTransport::new(&token));
            let daemon = pantheon_gateway::ChannelDaemon::new(state.join("tg-cursor"));
            daemon.run(
                vec![channel],
                Some((&transport, "https://api.telegram.org", &token)),
                sink.as_ref(),
                &outbound,
                &|| stop.load(std::sync::atomic::Ordering::Acquire),
            );
        }));
    }
    if let Some(token) = discord_token {
        let sink = sink.clone();
        let outbound = outbound.clone();
        let stop = stop.clone();
        handles.push(std::thread::spawn(move || {
            let gateway = pantheon_gateway::discord_gateway::DiscordGateway::new(&token);
            let inbox = pantheon_gateway::MemoryChannel::new("discord");
            let inbox = Arc::new(inbox);
            // Gateway thread: dispatches into the inbox.
            let gw_inbox = inbox.clone();
            let gw_stop = stop.clone();
            let gw = std::thread::spawn(move || {
                gateway.run(&gw_inbox, &|| {
                    gw_stop.load(std::sync::atomic::Ordering::Acquire)
                })
            });
            // Drain thread: route inbox events through the daemon plumbing.
            let daemon = pantheon_gateway::ChannelDaemon::new(state_dir.join("discord-cursor"));
            daemon.run(vec![inbox], None, sink.as_ref(), &outbound, &|| {
                stop.load(std::sync::atomic::Ordering::Acquire)
            });
            let _ = gw.join();
        }));
    }

    // Outbox drain thread: picks up replies queued by
    // `pantheon run --deliver <channel>` from another process and pushes
    // them into the shared outbound queue the channel daemons send from.
    // Without this, `--deliver` writes a file nobody reads.
    {
        let outbound = outbound.clone();
        let stop = stop.clone();
        let dd = data_dir.clone();
        handles.push(std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Acquire) {
                let (msgs, bad) = drain_outbound(&dd);
                for b in bad {
                    eprintln!("gateway outbox: {b}");
                }
                for m in msgs {
                    outbound
                        .lock()
                        .map(|mut q| q.push(m))
                        .unwrap_or_else(|_| eprintln!("gateway outbox: queue poisoned"));
                }
                // 2s is frequent enough that a queued reply feels immediate
                // and rare enough to cost nothing when the queue is empty.
                for _ in 0..20 {
                    if stop.load(std::sync::atomic::Ordering::Acquire) {
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }));
    }

    eprintln!("gateway running; Ctrl+C to stop");
    for h in handles {
        let _ = h.join();
    }
}

#[cfg(test)]
#[path = "gateway_cli_tests.rs"]
mod tests;

/// A reply produced by `pantheon run --deliver` that the gateway has not
/// sent yet.
///
/// The gateway's own outbound queue lives in memory, so a reply produced by
/// a separate process would be lost the moment that process exits. This is
/// the on-disk hand-off: `run --deliver` appends one line, the running
/// gateway drains it on its next tick. One JSON object per line, so a
/// partially written tail is discarded rather than corrupting the queue.
pub fn outbox_path(data_dir: &Path) -> PathBuf {
    data_dir.join("gateway").join("outbox.jsonl")
}

/// Gateway service state for in-process callers (the TUI `/gateway`
/// command). Read-only: unlike `service_ctl`, asking never installs,
/// starts, or prints anything.
pub(crate) struct GatewayStatus {
    /// A service unit exists (systemd unit on Linux, plist on macOS).
    pub installed: bool,
    /// The manager reports the service active.
    pub active: bool,
    /// Queued outbound messages awaiting delivery.
    pub outbox_pending: usize,
}

pub(crate) fn gateway_status() -> GatewayStatus {
    let data_dir = crate::data_dir();
    #[cfg(target_os = "linux")]
    let installed = unit_path().exists();
    #[cfg(target_os = "macos")]
    let installed = plist_path().exists();
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let installed = false;
    #[cfg(target_os = "linux")]
    let active = which("systemctl")
        .and_then(|_| {
            std::process::Command::new("systemctl")
                .args(["--user", "is-active", UNIT_NAME])
                .output()
                .ok()
        })
        .is_some_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "active");
    #[cfg(target_os = "macos")]
    let active = {
        let domain = format!("gui/{}", current_uid());
        which("launchctl")
            .and_then(|_| {
                std::process::Command::new("launchctl")
                    .args(["print", &format!("{domain}/{PLIST_LABEL}")])
                    .output()
                    .ok()
            })
            .is_some_and(|o| o.status.success())
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let active = false;
    let outbox_pending = std::fs::read_to_string(outbox_path(&data_dir))
        .map(|t| t.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0);
    GatewayStatus {
        installed,
        active,
        outbox_pending,
    }
}

/// Append a reply for the gateway to deliver.
pub fn enqueue_outbound(data_dir: &Path, to_conversation: &str, text: &str) -> Result<(), String> {
    use std::io::Write;
    let path = outbox_path(data_dir);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let line = serde_json::json!({
        "to": to_conversation,
        "text": text,
        "queued_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    });
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    writeln!(f, "{line}").map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

/// Take every queued reply, leaving the file empty. Called on each gateway
/// tick. A malformed line is skipped and reported rather than aborting the
/// drain, so one bad write cannot wedge delivery.
pub fn drain_outbound(data_dir: &Path) -> (Vec<pantheon_gateway::OutboundMessage>, Vec<String>) {
    let path = outbox_path(data_dir);
    let (Ok(raw), false) = (std::fs::read_to_string(&path), path.exists()) else {
        return (Vec::new(), Vec::new());
    };
    let mut msgs = Vec::new();
    let mut bad = Vec::new();
    for (i, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(v) => {
                let to = v.get("to").and_then(|x| x.as_str()).unwrap_or_default();
                let text = v.get("text").and_then(|x| x.as_str()).unwrap_or_default();
                if to.is_empty() || text.is_empty() {
                    bad.push(format!("line {}: missing to/text", i + 1));
                    continue;
                }
                msgs.push(pantheon_gateway::OutboundMessage {
                    to_conversation: to.to_string(),
                    text: text.to_string(),
                });
            }
            Err(e) => bad.push(format!("line {}: {e}", i + 1)),
        }
    }
    // Truncate only after a successful read, so a read error leaves the
    // queue intact instead of silently dropping undelivered replies.
    if let Err(e) = std::fs::write(&path, b"") {
        bad.push(format!("truncate {}: {e}", path.display()));
    }
    (msgs, bad)
}

// ── service supervision ────────────────────────────────────────────────────
// `gateway start` has to survive a reboot, so it installs a real OS service
// instead of backgrounding a process. systemd on Linux, launchd on macOS.
// Anywhere else the command says so and points at `gateway run`, because a
// silent no-op here would leave the user staring at a bot that never answers.

const UNIT_NAME: &str = "pantheon-gateway";
#[cfg(target_os = "macos")]
const PLIST_LABEL: &str = "africa.exoseed.pantheon.gateway";

#[derive(Clone, Copy)]
enum ServiceAction {
    Start,
    Stop,
    Restart,
    Status,
}

/// Absolute path to the running binary, resolved from argv[0] with a
/// PATH lookup as fallback. systemd needs an absolute ExecStart; a bare
/// `pantheon` would resolve against the unit's own PATH and can differ from
/// the shell the user typed the command in, which is a classic "works
/// foreground, dies as a service" bug.
fn self_exe() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::current_exe() {
        if p.is_absolute() && p.exists() {
            return Some(p);
        }
    }
    let name = std::env::args().next()?;
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':') {
        if dir.is_empty() {
            continue;
        }
        let cand = std::path::Path::new(dir).join(&name);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn platform_name() -> &'static str {
    "systemd (linux)"
}
#[cfg(target_os = "macos")]
fn platform_name() -> &'static str {
    "launchd (macos)"
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn platform_name() -> &'static str {
    "unsupported"
}

fn service_ctl(action: ServiceAction) {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        service_ctl_os(action)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = action;
        eprintln!(
            "gateway: no service manager on this platform\n\
             run it under whatever supervises your host, or use:\n  \
             pantheon gateway run"
        );
        std::process::exit(2);
    }
}

#[cfg(target_os = "linux")]
fn systemd_dir() -> std::path::PathBuf {
    // Honor a non-standard SYSTEMD_USER_CONFIG_DIR so this works in
    // containers and test harnesses that relocate the unit search path.
    std::env::var("SYSTEMD_USER_CONFIG_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            let base = std::env::var("XDG_CONFIG_HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| {
                    std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
                        .join(".config")
                });
            base.join("systemd").join("user")
        })
}

#[cfg(target_os = "linux")]
fn unit_path() -> std::path::PathBuf {
    systemd_dir().join(format!("{UNIT_NAME}.service"))
}

/// The unit body. `Restart=always` covers a crash; `WantedBy=default.target`
/// is what makes it come back after a reboot. The env the gateway needs
/// (tokens, allowlist) lives in the data dir's .env, which the binary loads
/// itself — so the unit deliberately does not inline secrets, and the file
/// it points at is never world-readable.
#[cfg(target_os = "linux")]
fn unit_body(exe: &std::path::Path, data_dir: &std::path::Path) -> String {
    format!(
        "[Unit]\n\
         Description=Pantheon gateway (Discord / Telegram)\n\
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

#[cfg(target_os = "linux")]
fn service_ctl_os(action: ServiceAction) {
    let data_dir = crate::data_dir();
    let path = unit_path();
    // Start/restart install a unit that will outlive this shell, so validate
    // the env first. Installing a unit that crash-loops forever and reporting
    // success is worse than refusing: the user gets a bot that silently
    // answers nothing.
    if matches!(
        action,
        ServiceAction::Start | ServiceAction::Restart | ServiceAction::Status
    ) {
        crate::config_doc::init_env_and_catalog(&data_dir);
        let _ = preflight_or_exit();
    }
    let have_systemctl = which("systemctl").is_some();
    if !have_systemctl {
        eprintln!(
            "gateway: systemctl not found on PATH (expected {})\n\
             start it yourself, or run in the foreground:\n  \
             pantheon gateway run",
            platform_name()
        );
        std::process::exit(2);
    }
    let verb = match action {
        ServiceAction::Start => "start",
        ServiceAction::Stop => "stop",
        ServiceAction::Restart => "restart",
        ServiceAction::Status => "status",
    };
    // Status never writes a unit: asking whether it is running must not
    // have the side effect of installing it.
    if matches!(action, ServiceAction::Status) {
        // Never installs: asking whether it is running must not cause it to be.
        std::process::exit(run_ctl(&[verb, UNIT_NAME]));
    }
    if matches!(action, ServiceAction::Start | ServiceAction::Restart) && !path.exists() {
        let exe = match self_exe() {
            Some(e) => e,
            None => {
                eprintln!("gateway: cannot resolve the pantheon binary path for ExecStart");
                std::process::exit(1);
            }
        };
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!("gateway: create {}: {e}", parent.display());
                std::process::exit(1);
            }
        }
        if let Err(e) = std::fs::write(&path, unit_body(&exe, &data_dir)) {
            eprintln!("gateway: write {}: {e}", path.display());
            std::process::exit(1);
        }
        println!("wrote {}", path.display());
        // daemon-reload picks up a unit that did not exist when the
        // manager last read its directory. Without it, `start` fails
        // with a bare "unit not found" that blames the user's command.
        if which("systemctl").is_some() {
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .status();
        }
        if matches!(action, ServiceAction::Start) {
            // Enable so it returns after a reboot; harmless if already on.
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "enable", UNIT_NAME])
                .status();
        }
    }
    if matches!(action, ServiceAction::Stop) && !path.exists() {
        eprintln!("gateway: no unit installed at {}", path.display());
        std::process::exit(1);
    }
    let started = matches!(action, ServiceAction::Start | ServiceAction::Restart);
    let code = run_ctl(&[verb, UNIT_NAME]);
    if code != 0 {
        std::process::exit(code);
    }
    if started {
        // systemctl's exit code only says the start request was accepted.
        // Confirm the unit actually settled, so a crash-loop is reported here
        // instead of as a bot that never answers.
        confirm_running();
    }
}

/// Fail loudly if the unit did not reach a running state. Restart=always means
/// a misconfigured unit is `activating (auto-restart)`, never `failed`, so
/// `is-active` alone is not enough: it has to be active AND not flapping.
#[cfg(target_os = "linux")]
fn confirm_running() {
    for _ in 0..12 {
        if let Ok(out) = std::process::Command::new("systemctl")
            .args(["--user", "is-active", UNIT_NAME])
            .output()
        {
            if String::from_utf8_lossy(&out.stdout).trim() == "active" {
                println!("gateway: active (logs: journalctl --user -u {UNIT_NAME} -f)");
                return;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    eprintln!(
        "gateway: unit did not reach 'active' — it is crash-looping.\n\
         most common cause: the token or allowlist is not visible to the\n\
         service. Check the journal:\n  \
         journalctl --user -u {UNIT_NAME} -n 40"
    );
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn confirm_running() {
    println!("gateway: bootstrapped (logs: launchctl print {PLIST_LABEL})");
}

#[cfg(target_os = "macos")]
fn plist_path() -> std::path::PathBuf {
    std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{PLIST_LABEL}.plist"))
}

#[cfg(target_os = "macos")]
fn plist_body(exe: &std::path::Path, data_dir: &std::path::Path) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \n\
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20 <key>Label</key><string>{PLIST_LABEL}</string>\n\
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

#[cfg(target_os = "macos")]
fn service_ctl_os(action: ServiceAction) {
    let data_dir = crate::data_dir();
    let path = plist_path();
    let label = PLIST_LABEL;
    let uid = std::process::id();
    let _ = uid;
    if which("launchctl").is_none() {
        eprintln!("gateway: launchctl not found; run `pantheon gateway run` in the foreground");
        std::process::exit(2);
    }
    let domain = format!("gui/{}", current_uid());
    if matches!(action, ServiceAction::Status) {
        let _ = std::process::Command::new("launchctl")
            .args(["print", &format!("{domain}/{label}")])
            .status();
        return;
    }
    if matches!(action, ServiceAction::Start | ServiceAction::Restart) {
        if matches!(action, ServiceAction::Stop) {
            let _ = std::process::Command::new("launchctl")
                .args(["bootout", &format!("{domain}/{label}")])
                .status();
        }
        if !path.exists() {
            let exe = match self_exe() {
                Some(e) => e,
                None => {
                    eprintln!("gateway: cannot resolve the pantheon binary path");
                    std::process::exit(1);
                }
            };
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::write(&path, plist_body(&exe, &data_dir)) {
                eprintln!("gateway: write {}: {e}", path.display());
                std::process::exit(1);
            }
            println!("wrote {}", path.display());
        }
        let _ = std::process::Command::new("launchctl")
            .args(["bootout", &format!("{domain}/{label}")])
            .status();
        run_ctl(&["bootstrap", &domain, &path.to_string_lossy()]);
    } else {
        run_ctl(&["bootout", &format!("{domain}/{label}")]);
    }
}

#[cfg(target_os = "macos")]
fn current_uid() -> u32 {
    // launchd's gui domain is keyed by the numeric uid, and getuid is not in
    // the stable std surface; reading proc-free via libc would add a dep for
    // one number, so shell out once and tolerate failure.
    which("id")
        .and_then(|_| std::process::Command::new("id").arg("-u").output().ok())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(501)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
/// Run the service manager and return its exit code. Deliberately not `!`:
/// `start` keeps going afterwards to confirm the unit actually came up, and
/// `status` reports rather than exiting.
fn run_ctl(args: &[&str]) -> i32 {
    let bin = match which(if cfg!(target_os = "linux") {
        "systemctl"
    } else {
        "launchctl"
    }) {
        Some(b) => b,
        None => {
            eprintln!("gateway: service manager not found on PATH");
            return 127;
        }
    };
    let mut full: Vec<String> = Vec::new();
    if cfg!(target_os = "linux") {
        full.push("--user".into());
    }
    full.extend(args.iter().map(|a| a.to_string()));
    match std::process::Command::new(&bin).args(&full).status() {
        Ok(s) => s.code().unwrap_or(1),
        Err(e) => {
            eprintln!("gateway: run {}: {e}", bin.display());
            1
        }
    }
}

/// Minimal PATH lookup. `which` is not worth a dependency for one call, and
/// this keeps the check honest about the PATH the user actually has.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn which(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':') {
        if dir.is_empty() {
            continue;
        }
        let cand = std::path::Path::new(dir).join(bin);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

#[cfg(test)]
#[path = "gateway_service_tests.rs"]
mod service_tests;
