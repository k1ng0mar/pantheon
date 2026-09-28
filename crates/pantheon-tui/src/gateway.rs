//! `pantheon gateway`: run the channel daemon against the live runtime.
//!
//! One thread per surface: Discord gateway websocket, Telegram long poll,
//! each with its own outbound queue and feeding a shared `EventSink` that
//! starts/resumes runs and answers approvals through the Supervisor.
//! Replies flow back over the same surfaces via `TelegramChannel` and
//! `DiscordChannel` (real REST transports) — never a memory buffer nobody
//! reads.
//!
//! Tokens come from the environment only:
//!   PANTHEON_DISCORD_TOKEN, PANTHEON_TELEGRAM_BOT_TOKEN
//! Missing tokens disable that surface; at least one must be set.

use pantheon_api::capability::Policy;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A parked run that needs continuing after an approval grant.
#[derive(Debug, Clone)]
struct ResumeRequest {
    gateway: String,
    thread_id: String,
    run_id: String,
}

/// Continue a parked run after a grant. `Send + Sync` so the daemon thread
/// can invoke it; the default implementation runs on a background thread
/// because the resume blocks on the model. Tests swap in a recorder.
type ResumeHook = Arc<dyn Fn(ResumeRequest) + Send + Sync>;

/// The production resume hook: continue the parked run on a background
/// thread and deliver the outcome text back to the originating thread.
/// Errors are logged, never fatal — a failed resume must not take the
/// gateway daemon down with it (unlike the CLI path, which exits).
fn default_resume_hook(
    data_dir: PathBuf,
    queues: &HashMap<String, Arc<Mutex<Vec<pantheon_gateway::OutboundMessage>>>>,
) -> ResumeHook {
    let queues = queues.clone();
    Arc::new(move |req: ResumeRequest| {
        let queues = queues.clone();
        let data_dir = data_dir.clone();
        std::thread::spawn(move || match resume_parked_run(&data_dir, &req.run_id) {
            Ok(answer) => {
                if !answer.is_empty() {
                    push_to_queue(&queues, &req.gateway, &req.thread_id, answer);
                }
            }
            Err(e) => eprintln!("gateway: resume {} failed: {e}", req.run_id),
        });
    })
}

/// Continue a parked run after a grant, returning the outcome text for
/// delivery. Same shape as `crate::agui::resume_after_grant`, but it
/// returns errors instead of exiting the process: the CLI can afford to
/// die on failure, a long-running gateway cannot.
fn resume_parked_run(data_dir: &Path, run_id: &str) -> Result<String, String> {
    use crate::config;
    use crate::config::build_model_policy;
    let file_cfg = config::Config::load_or_report(data_dir);
    let model_policy = build_model_policy(file_cfg.as_ref(), None, None);
    let allow_memory = file_cfg
        .as_ref()
        .map(|c| c.policy == Some(crate::config_schema::PolicyPreset::CoderMemory))
        .unwrap_or_else(|| {
            std::env::var("PANTHEON_ALLOW_MEMORY")
                .map(|v| v == "1" || v == "true")
                .unwrap_or(false)
        });
    let policy = if allow_memory {
        pantheon_api::capability::Policy::coder_with_memory()
    } else {
        pantheon_api::capability::Policy::coder()
    };
    let secrets = config::chat_secrets(file_cfg.as_ref());
    let session = pantheon_runtime::session::Session::new(
        data_dir.to_path_buf(),
        policy,
        model_policy,
        secrets,
    )
    .map_err(|e| e.to_string())?;
    // An empty turn rebuilds the transcript from the ledger and settles
    // the granted call; never resend the user message (duplicate turn).
    session
        .chat_turn(run_id, "", "")
        .map(|outcome| crate::terminal::outcome_text(&outcome))
        .map_err(|e| e.to_string())
}

/// Push one reply into a surface's outbound queue. A message for an
/// unknown surface is dead-lettered with a log rather than misdelivered.
fn push_to_queue(
    queues: &HashMap<String, Arc<Mutex<Vec<pantheon_gateway::OutboundMessage>>>>,
    gateway: &str,
    thread: &str,
    text: String,
) {
    match queues.get(gateway) {
        Some(q) => {
            q.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(pantheon_gateway::OutboundMessage {
                    to_conversation: thread.to_string(),
                    text,
                    gateway: gateway.to_string(),
                    attempts: 0,
                })
        }
        None => eprintln!(
            "gateway: no outbound queue for surface '{gateway}'; dropping reply to {thread}"
        ),
    }
}

/// The runtime-backed sink. Maps channel threads to run ids through a
/// thread→run map so a conversation keeps its run across messages.
struct RuntimeSink {
    data_dir: PathBuf,
    policy: Policy,
    threads: Mutex<HashMap<String, String>>,
    /// One outbound queue per surface ("telegram"/"discord"). Each
    /// surface's daemon drains only its own queue, so a Telegram reply
    /// can never be picked up by the Discord daemon and vice versa — the
    /// old single shared queue let each daemon grab the other's replies.
    queues: HashMap<String, Arc<Mutex<Vec<pantheon_gateway::OutboundMessage>>>>,
    /// Allowed platform sender ids (Telegram user id, Discord user id).
    /// None = allowlist disabled (local testing); Some(empty) denies
    /// everyone. The gateway is a remote shell, so the default is Some:
    /// cmd_gateway requires an explicit allowlist before it starts.
    allow: Option<HashSet<String>>,
    /// Runs after a grant is recorded so the parked run continues.
    resume: ResumeHook,
}

impl RuntimeSink {
    fn new(
        data_dir: PathBuf,
        policy: Policy,
        queues: HashMap<String, Arc<Mutex<Vec<pantheon_gateway::OutboundMessage>>>>,
        allow: Option<HashSet<String>>,
    ) -> Self {
        let resume = default_resume_hook(data_dir.clone(), &queues);
        Self {
            data_dir,
            policy,
            threads: Mutex::new(HashMap::new()),
            queues,
            allow,
            resume,
        }
    }
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
    let cfg = crate::config::Config::load_or_report(data_dir);
    let default = pantheon_api::model::DefaultModel {
        provider: std::env::var("PANTHEON_PROVIDER").unwrap_or_else(|_| "local".into()),
        model: std::env::var("PANTHEON_MODEL").unwrap_or_else(|_| "llama3.2".into()),
    };
    let model_policy = pantheon_api::model::ModelPolicy {
        reasoning_budget: Default::default(),
        reasoning: Default::default(),
        default: default.clone(),
        fallbacks: pantheon_api::model::FallbackChain::default(),
        auxiliaries: crate::config::auxiliaries(cfg.as_ref(), &default),
    };
    let secrets = crate::config::chat_secrets(cfg.as_ref());
    pantheon_runtime::session::Session::new(data_dir.to_path_buf(), policy, model_policy, secrets)
}

impl RuntimeSink {
    fn push_outbound(&self, gateway: &str, thread: &str, text: String) {
        push_to_queue(&self.queues, gateway, thread, text);
    }
}

/// Tags every event with the surface it arrived on so the shared sink can
/// route replies into the right per-surface queue. One of these wraps the
/// `RuntimeSink` per daemon thread.
struct SurfaceSink<'a> {
    inner: &'a RuntimeSink,
    gateway: &'static str,
}

impl pantheon_gateway::EventSink for SurfaceSink<'_> {
    fn on_message(&self, thread_id: &str, sender: Option<&str>, text: &str) {
        self.inner.on_message(self.gateway, thread_id, sender, text);
    }
    fn on_approval(
        &self,
        thread_id: &str,
        sender: Option<&str>,
        run_id: Option<&str>,
        scope: &str,
        grant: bool,
    ) {
        self.inner
            .on_approval(self.gateway, thread_id, sender, run_id, scope, grant);
    }
}

impl RuntimeSink {
    fn on_message(&self, gateway: &str, thread_id: &str, sender: Option<&str>, text: &str) {
        if !self.allowed(sender) {
            self.push_outbound(
                gateway,
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
                self.push_outbound(gateway, thread_id, text);
            }
            Err(e) => {
                self.push_outbound(gateway, thread_id, format!("error: {e}"));
            }
        }
    }

    fn on_approval(
        &self,
        gateway: &str,
        thread_id: &str,
        sender: Option<&str>,
        callback_run_id: Option<&str>,
        scope: &str,
        grant: bool,
    ) {
        if !self.allowed(sender) {
            self.push_outbound(gateway, thread_id, "not authorized".into());
            return;
        }
        let sup = match pantheon_runtime::Supervisor::open(self.data_dir.clone()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("gateway: supervisor open failed: {e}");
                return;
            }
        };
        // Phone notifications for locally-started runs carry the run id in
        // the button callback (the daemon's thread map never saw those
        // runs). Legacy buttons fall back to the thread map. Either way the
        // supervisor validates the scope against actual pending approvals,
        // so a forged run id grants nothing.
        let run_id = callback_run_id.map(|s| s.to_string()).or_else(|| {
            self.threads
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(thread_id)
                .cloned()
        });
        let Some(run_id) = run_id else {
            self.push_outbound(
                gateway,
                thread_id,
                "error: no run for this conversation".into(),
            );
            return;
        };
        let result = if grant {
            sup.grant(&run_id, scope)
        } else {
            sup.deny(&run_id, scope)
        };
        match result {
            Ok(()) => self.after_approval(gateway, thread_id, &run_id, scope, grant),
            Err(e) => self.push_outbound(gateway, thread_id, format!("error: {e}")),
        }
    }

    /// The decision is recorded: ack the user, and on grant hand the
    /// parked run to the resume hook so it actually continues. (Granting
    /// without resuming left runs parked forever — the ack lied.)
    fn after_approval(
        &self,
        gateway: &str,
        thread_id: &str,
        run_id: &str,
        scope: &str,
        grant: bool,
    ) {
        self.push_outbound(
            gateway,
            thread_id,
            if grant {
                format!("granted {scope}")
            } else {
                format!("denied {scope}")
            },
        );
        if grant {
            self.resume_run(gateway, thread_id, run_id);
        }
    }

    /// Hand a grant to the resume hook. The hook — not the daemon thread —
    /// decides how the resume runs; the default spawns a background thread
    /// so the poll loop never blocks on the model.
    fn resume_run(&self, gateway: &str, thread_id: &str, run_id: &str) {
        (self.resume)(ResumeRequest {
            gateway: gateway.to_string(),
            thread_id: thread_id.to_string(),
            run_id: run_id.to_string(),
        });
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

fn run_gateway_foreground() {
    // The scheduler loop starts unconditionally: a scheduler-only install
    // (no bot tokens) is the main always-on use case, and a service that
    // exits here would crash-loop under the service manager. Chat surfaces
    // start only when their token and the allowlist are present; otherwise
    // the process warns and keeps running.
    let (discord_token, telegram_token, allow) = pantheon_gateway::read_channel_env();
    let plan = pantheon_gateway::ChannelPlan::from_env(
        discord_token.as_deref(),
        telegram_token.as_deref(),
        &allow,
    );
    if plan.is_empty() {
        eprintln!("gateway: {}", pantheon_gateway::channels_disabled_note());
    }
    let discord_token = discord_token.filter(|_| plan.discord);
    let telegram_token = telegram_token.filter(|_| plan.telegram);
    let data_dir = crate::terminal::data_dir();
    // One outbound queue per surface. The old single shared queue let the
    // Telegram daemon grab Discord replies (send fails → infinite requeue)
    // and the Discord daemon swallow Telegram ones; keyed queues make
    // cross-surface pickup impossible.
    let mut queues: HashMap<String, Arc<Mutex<Vec<pantheon_gateway::OutboundMessage>>>> =
        HashMap::new();
    if telegram_token.is_some() {
        queues.insert("telegram".to_string(), Arc::new(Mutex::new(Vec::new())));
    }
    if discord_token.is_some() {
        queues.insert("discord".to_string(), Arc::new(Mutex::new(Vec::new())));
    }
    // The gateway runs the same policy the config names. It used to read
    // PANTHEON_GATEWAY_POLICY only, so a user with a working `policy =
    // "reader"` config still got coder on Discord and Telegram.
    let gw_cfg = crate::config::Config::load_or_report(&data_dir);
    let policy = match std::env::var("PANTHEON_GATEWAY_POLICY").as_deref() {
        Ok("researcher") => Policy::researcher_readonly(),
        Ok("coder") => Policy::coder(),
        _ => crate::config_schema::policy_for_config(&gw_cfg),
    };
    let sink = Arc::new(RuntimeSink::new(
        data_dir.clone(),
        policy,
        queues.clone(),
        Some(allow),
    ));
    let state_dir = data_dir.join("gateway");
    let _ = std::fs::create_dir_all(&state_dir);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

    if let (Some(token), Some(queue)) = (telegram_token, queues.get("telegram").cloned()) {
        let sink = sink.clone();
        let stop = stop.clone();
        let state = state_dir.clone();
        handles.push(std::thread::spawn(move || {
            let channel = Arc::new(pantheon_gateway::TelegramChannel::rest(&token));
            let transport = Arc::new(pantheon_gateway::TelegramRestTransport::new(&token));
            let surface = SurfaceSink {
                inner: sink.as_ref(),
                gateway: "telegram",
            };
            let daemon = pantheon_gateway::ChannelDaemon::new(state.join("tg-cursor"));
            daemon.run(
                vec![channel],
                Some((&transport, "https://api.telegram.org", &token)),
                &surface,
                queue.as_ref(),
                &|| stop.load(std::sync::atomic::Ordering::Acquire),
            );
        }));
    }
    if let (Some(token), Some(queue)) = (discord_token, queues.get("discord").cloned()) {
        let sink = sink.clone();
        let stop = stop.clone();
        handles.push(std::thread::spawn(move || {
            // The Discord channel owns both directions: the gateway
            // websocket feeds its inbox, the daemon drains it, and replies
            // go out over Discord REST. (It used to be a MemoryChannel
            // whose outbox nobody read — replies accumulated in RAM and
            // the user got silence.)
            let discord = Arc::new(pantheon_gateway::DiscordChannel::rest(&token));
            let gateway = pantheon_gateway::discord_gateway::DiscordGateway::new(&token);
            let gw_inbox = discord.clone();
            let gw_stop = stop.clone();
            let gw = std::thread::spawn(move || {
                gateway.run(&gw_inbox, &|| {
                    gw_stop.load(std::sync::atomic::Ordering::Acquire)
                })
            });
            // Drain thread: route inbox events through the daemon plumbing.
            let surface = SurfaceSink {
                inner: sink.as_ref(),
                gateway: "discord",
            };
            let daemon = pantheon_gateway::ChannelDaemon::new(state_dir.join("discord-cursor"));
            daemon.run(vec![discord], None, &surface, queue.as_ref(), &|| {
                stop.load(std::sync::atomic::Ordering::Acquire)
            });
            let _ = gw.join();
        }));
    }

    // Outbox drain thread: picks up replies queued by
    // `pantheon run --deliver <channel>` from another process and files
    // them into that surface's outbound queue. Without this, `--deliver`
    // writes a file nobody reads.
    {
        let queues = queues.clone();
        let stop = stop.clone();
        let dd = data_dir.clone();
        handles.push(std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::Acquire) {
                let (msgs, bad) = drain_outbound(&dd);
                for b in bad {
                    eprintln!("gateway outbox: {b}");
                }
                for m in msgs {
                    match queues.get(&m.gateway) {
                        Some(q) => q
                            .lock()
                            .map(|mut qq| qq.push(m))
                            .unwrap_or_else(|_| eprintln!("gateway outbox: queue poisoned")),
                        None => eprintln!(
                            "gateway outbox: unknown gateway '{}' for thread {}; dropping",
                            m.gateway, m.to_conversation,
                        ),
                    }
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
    // Scheduler loop: the gateway service is also the always-on scheduler.
    // It ticks due jobs on the same claim ledger and job store as
    // `pantheon schedule tick`, so the two can never double-fire an
    // occurrence (the claim is an atomic first-wins INSERT). Detached: the
    // loop never returns, and the process ends with the rest on Ctrl+C.
    {
        let dd = data_dir.clone();
        std::thread::spawn(move || crate::schedule::run_scheduler_loop(&dd));
    }
    for h in handles {
        let _ = h.join();
    }
}
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
    let data_dir = crate::terminal::data_dir();
    // Shared cross-platform inspection: the same code `gateway status`
    // uses, so the TUI's `/gateway` line can never disagree with the CLI.
    let st = pantheon_gateway::service_status();
    let outbox_pending = std::fs::read_to_string(outbox_path(&data_dir))
        .map(|t| t.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0);
    GatewayStatus {
        installed: st.installed.is_some(),
        active: st.running,
        outbox_pending,
    }
}

pub fn enqueue_outbound(
    data_dir: &Path,
    to_conversation: &str,
    text: &str,
    gateway: &str,
) -> Result<(), String> {
    use std::io::Write;
    let path = outbox_path(data_dir);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let line = serde_json::json!({
        "to": to_conversation,
        "text": text,
        "gateway": gateway,
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
    // No outbox file yet is the common case, not an error.
    let Ok(raw) = std::fs::read_to_string(&path) else {
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
                let gateway = v
                    .get("gateway")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default();
                if to.is_empty() || text.is_empty() {
                    bad.push(format!("line {}: missing to/text", i + 1));
                    continue;
                }
                msgs.push(pantheon_gateway::OutboundMessage {
                    to_conversation: to.to_string(),
                    text: text.to_string(),
                    gateway: gateway.to_string(),
                    attempts: 0,
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
// Thin over the cross-platform machinery in `pantheon_gateway::service` —
// the same code `pantheon init` uses, so the two can never disagree about
// how the service is installed. systemd on Linux (cron `@reboot`
// fallback), launchd on macOS, Task Scheduler on Windows.

#[derive(Clone, Copy)]
enum ServiceAction {
    Start,
    Stop,
    Restart,
    Status,
}

/// Install (or converge) the background service and confirm it settled.
/// Shared by `start` and by `restart` when nothing is installed yet.
fn service_start(data_dir: &std::path::Path) {
    let exe = match pantheon_gateway::self_exe() {
        Some(e) => e,
        None => {
            eprintln!("gateway: cannot resolve the pantheon binary path for ExecStart");
            std::process::exit(1);
        }
    };
    match pantheon_gateway::install_service(data_dir, &exe) {
        pantheon_gateway::InstallOutcome::Installed { mechanism, changed } => {
            if changed {
                println!("gateway: service installed via {}.", mechanism.as_str());
            } else {
                println!(
                    "gateway: service already installed via {}; ensured running.",
                    mechanism.as_str()
                );
            }
            confirm_active(mechanism);
        }
        pantheon_gateway::InstallOutcome::Unavailable { note } => {
            // Fail open, like `pantheon init`: no manager is a manual setup.
            println!("gateway: {note}");
        }
        pantheon_gateway::InstallOutcome::Failed { mechanism, error } => {
            eprintln!(
                "gateway: install via {} failed: {error}",
                mechanism.as_str()
            );
            std::process::exit(1);
        }
    }
}

/// The manager accepting the start request is not the process staying up:
/// poll briefly so a crash-loop is reported here instead of as a bot that
/// never answers. Mechanisms that only fire later (cron `@reboot`, logon
/// tasks) are reported honestly instead of polled.
fn confirm_active(mechanism: pantheon_gateway::ServiceMechanism) {
    use pantheon_gateway::ServiceMechanism as M;
    match mechanism {
        M::Cron => {
            println!("gateway: installed; the @reboot entry starts it at next boot.");
            return;
        }
        M::TaskScheduler => {
            println!("gateway: installed; it starts at next logon.");
            println!("         run `pantheon gateway run` in the foreground for this session.");
            return;
        }
        M::Systemd | M::Launchd => {}
        M::None => return,
    }
    for _ in 0..12 {
        if pantheon_gateway::service_status().running {
            println!("gateway: active");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    eprintln!(
        "gateway: installed but not reporting active — it may be crash-looping.\n\
         most common cause: the token or allowlist is not visible to the\n\
         service. Check the service logs."
    );
    std::process::exit(1);
}

fn service_ctl(action: ServiceAction) {
    let data_dir = crate::terminal::data_dir();
    match action {
        ServiceAction::Start => service_start(&data_dir),
        ServiceAction::Stop => match pantheon_gateway::stop_service() {
            pantheon_gateway::StopOutcome::Stopped { mechanism } => {
                println!("gateway: stopped ({}).", mechanism.as_str());
            }
            pantheon_gateway::StopOutcome::NotInstalled => {
                eprintln!("gateway: no service installed");
                std::process::exit(1);
            }
            pantheon_gateway::StopOutcome::Noop { note, .. } => println!("gateway: {note}"),
            pantheon_gateway::StopOutcome::Failed { mechanism, error } => {
                eprintln!("gateway: stop via {} failed: {error}", mechanism.as_str());
                std::process::exit(1);
            }
        },
        // Restart used to install a missing unit too; keep that.
        ServiceAction::Restart => match pantheon_gateway::restart_service() {
            pantheon_gateway::RestartOutcome::Restarted { mechanism } => {
                println!("gateway: restarted ({}).", mechanism.as_str());
            }
            pantheon_gateway::RestartOutcome::NotInstalled => service_start(&data_dir),
            pantheon_gateway::RestartOutcome::Noop { note, .. } => println!("gateway: {note}"),
            pantheon_gateway::RestartOutcome::Failed { mechanism, error } => {
                eprintln!(
                    "gateway: restart via {} failed: {error}",
                    mechanism.as_str()
                );
                std::process::exit(1);
            }
        },
        // Status never installs: asking whether it is running must not have
        // the side effect of installing it. Nor does it need tokens: a
        // scheduler-only install is a healthy install.
        ServiceAction::Status => {
            print_schedule_queue();
            let st = pantheon_gateway::service_status();
            match st.installed {
                Some(m) => println!(
                    "gateway: {} ({}).",
                    if st.running {
                        "active"
                    } else {
                        "installed, not active"
                    },
                    m.as_str()
                ),
                None => println!("gateway: not installed (pantheon gateway start)"),
            }
        }
    }
}

/// Scheduler queue summary for `gateway status`: due jobs and next fire,
/// loaded read-only from the same job store the tick loop uses. Never
/// installs or mutates anything.
fn print_schedule_queue() {
    let data_dir = crate::terminal::data_dir();
    match crate::schedule::load_schedulable(&data_dir) {
        Ok(jobs) => {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            println!(
                "schedule queue: {}",
                pantheon_gateway::queue_summary(&jobs, now_ms)
            );
        }
        Err(e) => println!("schedule queue: unavailable ({e})"),
    }
}
