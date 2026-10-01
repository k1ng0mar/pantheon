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
    config::apply_tool_enablement(&session, file_cfg.as_ref());
    config::apply_budget_tiers(&session, file_cfg.as_ref());
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
///
/// `gateway` on every method is the *channel name* (`telegram`/`discord`
/// in the legacy single-bot shape, any slug for explicit multi-bot
/// channels) — never the platform. Thread ids are namespaced per channel
/// before they touch the thread map: two bots on the same platform hand
/// out overlapping thread ids, and the raw id would let bot B pick up bot
/// A's conversation (and answer it from the wrong agent).
struct RuntimeSink {
    data_dir: PathBuf,
    /// Config-default policy. A channel with an agent may narrow it to
    /// the agent's resolved preset (see [`RuntimeSink::policy_for`]).
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
    /// Per-channel agent runtimes, by channel name. A message is answered
    /// by its channel's agent — own profile, own persona, own memory
    /// namespace. A channel with no entry here runs anonymous, exactly as
    /// before profiles existed.
    agents: HashMap<String, pantheon_runtime::AgentRuntime>,
}

/// Namespace a raw platform thread id under its channel so two bots on
/// the same platform never share a thread-map entry.
fn thread_key(channel: &str, thread_id: &str) -> String {
    format!("{channel}\u{1f}{thread_id}")
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
            agents: HashMap::new(),
        }
    }

    /// Attach the per-channel agent runtimes (built once at startup).
    /// Builder-style so the anonymous single-bot path keeps calling
    /// `new` unchanged.
    fn with_agents(mut self, agents: HashMap<String, pantheon_runtime::AgentRuntime>) -> Self {
        self.agents = agents;
        self
    }

    /// The policy for one channel's turn. `PANTHEON_GATEWAY_POLICY` wins
    /// when set (the historical override); otherwise the channel agent's
    /// resolved preset; otherwise the config default. Anonymous channels
    /// get exactly today's behavior.
    fn policy_for(&self, channel: &str) -> Policy {
        match std::env::var("PANTHEON_GATEWAY_POLICY").as_deref() {
            Ok("researcher") => return Policy::researcher_readonly(),
            Ok("coder") => return Policy::coder(),
            _ => {}
        }
        if let Some(preset) = self
            .agents
            .get(channel)
            .and_then(|a| crate::config_schema::PolicyPreset::parse(a.policy_preset()))
        {
            return preset.to_policy();
        }
        self.policy.clone()
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
        .inspect(|s| {
            crate::config::apply_tool_enablement(s, cfg.as_ref());
            crate::config::apply_budget_tiers(s, cfg.as_ref());
        })
}

impl RuntimeSink {
    fn push_outbound(&self, gateway: &str, thread: &str, text: String) {
        push_to_queue(&self.queues, gateway, thread, text);
    }
}

/// Tags every event with the channel it arrived on so the shared sink can
/// route replies into the right per-channel queue. One of these wraps the
/// `RuntimeSink` per daemon thread. The tag is the channel name (any slug
/// for explicit multi-bot channels), not a 'static platform string.
struct SurfaceSink<'a> {
    inner: &'a RuntimeSink,
    gateway: &'a str,
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
        let agent = self.agents.get(gateway).cloned();
        let session = match open_session(&self.data_dir, self.policy_for(gateway)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("gateway: session open failed: {e}");
                return;
            }
        };
        if let Some(agent) = agent {
            // Attach the channel's agent before chatting: `chat_turn`
            // binds the run to this agent, so a run id that another
            // channel's agent owns is refused instead of answered from
            // the wrong identity.
            if let Err(e) = session.with_agent(agent) {
                self.push_outbound(gateway, thread_id, format!("error: {e}"));
                return;
            }
        }
        let key = thread_key(gateway, thread_id);
        let run_id = self
            .threads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key)
            .or_insert_with(pantheon_runtime::new_run_id)
            .clone();
        match session.chat(&run_id, text) {
            Ok(outcome) => {
                let text = match outcome {
                    pantheon_agent::LoopOutcome::Answered { text: t, .. } => t,
                    pantheon_agent::LoopOutcome::AwaitingApproval { capability, scope } => {
                        // `is_git_push` is advisory detection (see its
                        // docs), not a security boundary: a push hidden in
                        // an opaque shell construction may never be flagged.
                        // Say so on the one approval the detector produces;
                        // the approval flow and message shape are untouched.
                        let advisory =
                            if matches!(&capability, pantheon_api::capability::Capability::GitPush)
                            {
                                " Note: git-push detection is advisory; a push \
                             hidden in an opaque shell construction may not \
                             be flagged."
                            } else {
                                ""
                            };
                        format!(
                            "approval needed: {capability:?}.{advisory} Reply 'grant {run_id} {scope}' to \
                             allow it, or 'deny {run_id} {scope}' to refuse it."
                        )
                    }
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
        _callback_run_id: Option<&str>,
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
        // The daemon's thread map only knows runs it initiated itself:
        // button taps on approval prompts the gateway sent resolve here.
        // (Phone notifications about locally-started runs were removed;
        // nothing sends run-id-carrying callbacks anymore.) Either way the
        // supervisor validates the scope against actual pending approvals,
        // so a forged run id grants nothing.
        let run_id = self
            .threads
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&thread_key(gateway, thread_id))
            .cloned();
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

/// One gateway chat surface: a named channel bound to a platform, a bot
/// token, and the agent profile that serves it.
struct ChannelSpec {
    /// Channel name — keys outbound queues, cursor files, and thread ids.
    name: String,
    /// `"telegram"` or `"discord"`.
    platform: String,
    /// Bot token (never logged).
    token: String,
    /// Agent profile serving this channel; `None` = default resolution.
    profile: Option<String>,
    /// Voice replies on this channel.
    voice_replies: bool,
}

/// A `Channel` that answers to a configured name. Two bots on the same
/// platform are otherwise indistinguishable to the daemon plumbing
/// (`TelegramChannel::name()` is the hardcoded `"telegram"`); the wrapper
/// gives each its channel name for routing, claiming, and logs while the
/// inner channel keeps doing the real transport work.
struct NamedChannel {
    name: String,
    inner: Arc<dyn pantheon_gateway::Channel>,
}

impl NamedChannel {
    fn new(name: String, inner: Arc<dyn pantheon_gateway::Channel>) -> Arc<Self> {
        Arc::new(Self { name, inner })
    }
}

impl pantheon_gateway::Channel for NamedChannel {
    fn name(&self) -> &str {
        &self.name
    }
    fn send(
        &self,
        envelope: pantheon_gateway::ChannelEnvelope,
    ) -> Result<(), pantheon_gateway::ChannelError> {
        self.inner.send(envelope)
    }
    fn poll(&self) -> Vec<pantheon_gateway::ChannelEvent> {
        self.inner.poll()
    }
}

/// Build the channel plan from config + secrets.
///
/// Explicit mode: any `[gateway.channels.<name>]` entry that declares a
/// platform becomes its own channel — one poller per entry, so two
/// entries can both be `telegram` with different bot tokens and different
/// agent profiles. The token comes from `token_secret` through the
/// secrets broker (process env wins, then `<data_dir>/gateway.env`),
/// falling back to the platform's standard token.
///
/// Legacy mode: no entry declares a platform — today's behavior exactly:
/// at most one `telegram` and one `discord` channel from the standard
/// tokens, served by the default profile.
///
/// A channel starts only when its token is present AND the allowlist is
/// non-empty: a token without an allowlist would hand the host to anyone
/// who finds the bot.
fn channel_specs(
    cfg: &Option<crate::config::Config>,
    data_dir: &Path,
    discord_token: Option<String>,
    telegram_token: Option<String>,
    allow: &HashSet<String>,
) -> Vec<ChannelSpec> {
    let gated = !allow.is_empty();
    let mut explicit: Vec<(&String, &pantheon_api::config::GatewayChannelSection)> = cfg
        .as_ref()
        .and_then(|c| c.gateway.as_ref())
        .map(|g| {
            g.channels
                .iter()
                .filter(|(_, ch)| ch.is_explicit())
                .collect()
        })
        .unwrap_or_default();
    explicit.sort_by(|a, b| a.0.cmp(b.0));
    if !explicit.is_empty() {
        let mut specs = Vec::new();
        for (name, ch) in explicit {
            let Some(platform) = ch.resolved_platform(name) else {
                // validate() flags the bad platform; skip rather than guess.
                continue;
            };
            let token = ch
                .token_secret
                .as_deref()
                .and_then(|s| pantheon_secrets::gateway_token(data_dir, s))
                .map(|v| v.expose().to_string())
                .filter(|t| !t.trim().is_empty())
                .or_else(|| match platform.as_str() {
                    "telegram" => telegram_token.clone(),
                    "discord" => discord_token.clone(),
                    _ => None,
                });
            match token {
                Some(token) if gated => specs.push(ChannelSpec {
                    name: (*name).clone(),
                    platform,
                    token,
                    profile: ch.profile.clone(),
                    voice_replies: ch.voice_replies,
                }),
                _ => eprintln!(
                    "gateway: channel '{name}' skipped ({}).",
                    if !gated {
                        "the allowlist is empty"
                    } else {
                        "no token found"
                    }
                ),
            }
        }
        return specs;
    }
    // Legacy shape: exactly today's behavior.
    let plan = pantheon_gateway::ChannelPlan::from_env(
        discord_token.as_deref(),
        telegram_token.as_deref(),
        allow,
    );
    if plan.is_empty() {
        eprintln!("gateway: {}", pantheon_gateway::channels_disabled_note());
    }
    let voice_replies = |name: &str| {
        cfg.as_ref()
            .and_then(|c| c.gateway.as_ref())
            .and_then(|g| g.channels.get(name))
            .map(|c| c.voice_replies)
            .unwrap_or(false)
    };
    let mut specs = Vec::new();
    if let Some(token) = telegram_token.filter(|_| plan.telegram) {
        specs.push(ChannelSpec {
            name: "telegram".to_string(),
            platform: "telegram".to_string(),
            token,
            profile: None,
            voice_replies: voice_replies("telegram"),
        });
    }
    if let Some(token) = discord_token.filter(|_| plan.discord) {
        specs.push(ChannelSpec {
            name: "discord".to_string(),
            platform: "discord".to_string(),
            token,
            profile: None,
            voice_replies: voice_replies("discord"),
        });
    }
    specs
}

/// Cursor file for a channel. Legacy channels keep their historical names
/// so cursors survive the upgrade; explicit channels get `cursor-<name>`.
fn cursor_file(state_dir: &Path, spec: &ChannelSpec) -> PathBuf {
    match spec.name.as_str() {
        "telegram" => state_dir.join("tg-cursor"),
        "discord" => state_dir.join("discord-cursor"),
        _ => state_dir.join(format!("cursor-{}", spec.name)),
    }
}

fn run_gateway_foreground() {
    // The scheduler loop starts unconditionally: a scheduler-only install
    // (no bot tokens) is the main always-on use case, and a service that
    // exits here would crash-loop under the service manager. Chat surfaces
    // start only when their token and the allowlist are present; otherwise
    // the process warns and keeps running.
    let data_dir = crate::terminal::data_dir();
    // MCP boot health check: probe enabled MCP servers at startup and log
    // unreachable ones, so a broken server is visible instead of silently
    // absent from the tool list.
    pantheon_gateway::mcp_boot::check_and_log(&data_dir, std::time::Duration::from_secs(10));
    // Tokens come from the secrets store (<data_dir>/gateway.env, written
    // by the Full Setup wizard's Gateway screen) with process env vars
    // winning when both are set.
    let (discord_token, telegram_token, allow) = pantheon_gateway::read_channel_tokens(&data_dir);
    // The gateway runs the same policy the config names. It used to read
    // PANTHEON_GATEWAY_POLICY only, so a user with a working `policy =
    // "reader"` config still got coder on Discord and Telegram.
    let gw_cfg = crate::config::Config::load_or_report(&data_dir);
    // One poller per channel: in explicit mode several channels can share
    // a platform (two Telegram bots, two profiles); in legacy mode this is
    // exactly the old telegram/discord pair.
    let specs = channel_specs(&gw_cfg, &data_dir, discord_token, telegram_token, &allow);
    // One outbound queue per channel. The old single shared queue let the
    // Telegram daemon grab Discord replies (send fails → infinite requeue)
    // and the Discord daemon swallow Telegram ones; keyed queues make
    // cross-channel pickup impossible.
    let mut queues: HashMap<String, Arc<Mutex<Vec<pantheon_gateway::OutboundMessage>>>> =
        HashMap::new();
    for spec in &specs {
        queues.insert(spec.name.clone(), Arc::new(Mutex::new(Vec::new())));
    }
    // Voice for the gateway channels: the [stt]/[tts] backends the setup
    // wizard writes, double-gated by the [tools] voice toggle (see
    // pantheon_gateway::channel_voice::VoicePipes). voice_replies is
    // per-channel ([gateway.channels.<name>]), default off — text stays
    // the default.
    let secrets = crate::config::chat_secrets(gw_cfg.as_ref());
    // Voice pipes per channel: the [stt]/[tts] backends the setup wizard
    // writes, double-gated by the [tools] voice toggle (see
    // pantheon_gateway::channel_voice::VoicePipes). voice_replies is
    // per-channel ([gateway.channels.<name>]), default off — text stays
    // the default.
    let mut voices: HashMap<String, pantheon_gateway::VoicePipes> = HashMap::new();
    for spec in &specs {
        let pipes = pantheon_gateway::VoicePipes::from_config(
            gw_cfg.as_ref().and_then(|c| c.tools.as_ref()),
            gw_cfg.as_ref().and_then(|c| c.stt.as_ref()),
            gw_cfg.as_ref().and_then(|c| c.tts.as_ref()),
            &secrets,
            spec.voice_replies,
        );
        eprintln!("gateway: voice {}({})", spec.name, pipes.describe());
        voices.insert(spec.name.clone(), pipes);
    }
    // The config-default policy. PANTHEON_GATEWAY_POLICY still wins when
    // set (see RuntimeSink::policy_for); a channel with an agent otherwise
    // runs its agent's resolved preset.
    let policy = crate::config_schema::policy_for_config(&gw_cfg);
    // One agent runtime per channel: each channel is served by its own
    // profile, so a message on bot A is answered by profile A with A's
    // memory, and bot B never sees it. An unknown profile fails fast with
    // a clear error instead of silently serving the wrong agent. Channels
    // with no resolvable profile stay anonymous, as before.
    let mut agents: HashMap<String, pantheon_runtime::AgentRuntime> = HashMap::new();
    if let Some(cfg) = gw_cfg.as_ref() {
        match pantheon_runtime::Supervisor::open(data_dir.clone()) {
            Ok(supervisor) => {
                for spec in &specs {
                    match cfg.resolve_profile(spec.profile.as_deref()) {
                        Ok(Some(effective)) => {
                            let built = cfg
                                .profile_registry()
                                .map_err(pantheon_runtime::profile_err)
                                .and_then(|reg| {
                                    pantheon_runtime::AgentRuntime::new(
                                        supervisor.clone(),
                                        reg,
                                        effective,
                                        data_dir.clone(),
                                    )
                                });
                            match built {
                                Ok(agent) => {
                                    agents.insert(spec.name.clone(), agent);
                                }
                                Err(e) => {
                                    eprintln!(
                                        "gateway: channel '{}': cannot start agent: {e}",
                                        spec.name
                                    );
                                    std::process::exit(1);
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            eprintln!(
                                "gateway: channel '{}': {}",
                                spec.name,
                                pantheon_runtime::profile_err(e)
                            );
                            std::process::exit(1);
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("gateway: supervisor open failed: {e}");
                std::process::exit(1);
            }
        }
    }
    let sink = Arc::new(
        RuntimeSink::new(data_dir.clone(), policy, queues.clone(), Some(allow)).with_agents(agents),
    );
    let state_dir = data_dir.join("gateway");
    let _ = std::fs::create_dir_all(&state_dir);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

    // One poller thread per channel. Two channels on the same platform
    // are fully independent: separate transports, separate cursors,
    // separate queues, and (via the sink) separate agents.
    for spec in specs {
        let queue = match queues.get(&spec.name).cloned() {
            Some(q) => q,
            None => continue,
        };
        let voice = voices.remove(&spec.name);
        let sink = sink.clone();
        let stop = stop.clone();
        let state_dir = state_dir.clone();
        handles.push(std::thread::spawn(move || {
            let surface = SurfaceSink {
                inner: sink.as_ref(),
                gateway: &spec.name,
            };
            let cursor = cursor_file(&state_dir, &spec);
            match spec.platform.as_str() {
                "telegram" => {
                    let channel = Arc::new(
                        pantheon_gateway::TelegramChannel::rest(&spec.token)
                            .with_voice(voice.unwrap_or_default()),
                    );
                    let named = NamedChannel::new(spec.name.clone(), channel.clone() as Arc<_>);
                    let transport =
                        Arc::new(pantheon_gateway::TelegramRestTransport::new(&spec.token));
                    let daemon = pantheon_gateway::ChannelDaemon::new(cursor);
                    daemon.run(
                        vec![named as Arc<dyn pantheon_gateway::Channel>],
                        Some((&channel, &transport)),
                        &surface,
                        queue.as_ref(),
                        &|| stop.load(std::sync::atomic::Ordering::Acquire),
                    );
                }
                "discord" => {
                    // The Discord channel owns both directions: the gateway
                    // websocket feeds its inbox, the daemon drains it, and replies
                    // go out over Discord REST. (It used to be a MemoryChannel
                    // whose outbox nobody read — replies accumulated in RAM and
                    // the user got silence.)
                    let discord = Arc::new(
                        pantheon_gateway::DiscordChannel::rest(&spec.token)
                            .with_voice(voice.unwrap_or_default()),
                    );
                    let named = NamedChannel::new(spec.name.clone(), discord.clone() as Arc<_>);
                    let gateway =
                        pantheon_gateway::discord_gateway::DiscordGateway::new(&spec.token);
                    let gw_inbox = discord.clone();
                    let gw_stop = stop.clone();
                    let gw = std::thread::spawn(move || {
                        gateway.run(&gw_inbox, &|| {
                            gw_stop.load(std::sync::atomic::Ordering::Acquire)
                        })
                    });
                    // Drain thread: route inbox events through the daemon plumbing.
                    let daemon = pantheon_gateway::ChannelDaemon::new(cursor);
                    daemon.run(
                        vec![named as Arc<dyn pantheon_gateway::Channel>],
                        None,
                        &surface,
                        queue.as_ref(),
                        &|| stop.load(std::sync::atomic::Ordering::Acquire),
                    );
                    let _ = gw.join();
                }
                other => {
                    eprintln!(
                        "gateway: channel '{}': unsupported platform '{other}'; skipping",
                        spec.name
                    );
                }
            }
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

#[cfg(test)]
mod gateway_multi_agent_tests {
    use super::*;
    use pantheon_gateway::Channel as _;

    fn explicit_cfg() -> crate::config::Config {
        let mut cfg = crate::config::Config::default();
        let mut gw = pantheon_api::config::GatewaySection::default();
        gw.channels.insert(
            "bot-a".to_string(),
            pantheon_api::config::GatewayChannelSection {
                platform: Some("telegram".to_string()),
                token_secret: Some("BOT_A_TOKEN".to_string()),
                profile: Some("support".to_string()),
                voice_replies: false,
            },
        );
        gw.channels.insert(
            "bot-b".to_string(),
            pantheon_api::config::GatewayChannelSection {
                platform: Some("telegram".to_string()),
                token_secret: None,
                profile: Some("coder".to_string()),
                voice_replies: true,
            },
        );
        cfg.gateway = Some(gw);
        cfg
    }

    fn allow_one() -> HashSet<String> {
        HashSet::from(["123".to_string()])
    }

    /// Scratch data dir with a gateway.env holding BOT_A_TOKEN.
    fn data_dir_with_secret() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pantheon-gw-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("gateway.env"), "BOT_A_TOKEN=secret-token-a\n").unwrap();
        dir
    }

    #[test]
    fn two_telegram_bots_get_separate_specs() {
        let data_dir = data_dir_with_secret();
        let specs = channel_specs(
            &Some(explicit_cfg()),
            &data_dir,
            None,
            Some("fallback-tg-token".to_string()),
            &allow_one(),
        );
        let _ = std::fs::remove_dir_all(&data_dir);
        assert_eq!(specs.len(), 2, "one poller per explicit channel");
        // Sorted by channel name for deterministic startup order.
        assert_eq!(specs[0].name, "bot-a");
        assert_eq!(specs[1].name, "bot-b");
        for s in &specs {
            assert_eq!(s.platform, "telegram");
        }
        // bot-a's token comes from its token_secret; bot-b falls back to
        // the platform's standard token.
        assert_eq!(specs[0].token, "secret-token-a");
        assert_eq!(specs[1].token, "fallback-tg-token");
        assert_ne!(specs[0].token, specs[1].token);
        // Each bot keeps its own profile.
        assert_eq!(specs[0].profile.as_deref(), Some("support"));
        assert_eq!(specs[1].profile.as_deref(), Some("coder"));
        assert!(!specs[0].voice_replies);
        assert!(specs[1].voice_replies);
    }

    #[test]
    fn legacy_shape_uses_env_tokens_and_default_profile() {
        let data_dir = std::env::temp_dir();
        let specs = channel_specs(
            &Some(crate::config::Config::default()),
            &data_dir,
            None,
            Some("tg-token".to_string()),
            &allow_one(),
        );
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].name, "telegram");
        assert_eq!(specs[0].platform, "telegram");
        assert_eq!(specs[0].token, "tg-token");
        assert_eq!(specs[0].profile, None);
    }

    #[test]
    fn empty_allowlist_starts_no_channels() {
        let data_dir = std::env::temp_dir();
        let specs = channel_specs(
            &Some(explicit_cfg()),
            &data_dir,
            None,
            Some("tg-token".to_string()),
            &HashSet::new(),
        );
        assert!(
            specs.is_empty(),
            "a token without an allowlist starts nothing"
        );
    }

    #[test]
    fn no_tokens_means_no_specs() {
        let data_dir = std::env::temp_dir();
        let specs = channel_specs(&None, &data_dir, None, None, &allow_one());
        assert!(specs.is_empty());
    }

    #[test]
    fn thread_key_namespaces_by_channel() {
        // Two bots on the same platform hand out overlapping thread ids;
        // the namespaced keys must differ.
        let a = thread_key("bot-a", "42");
        let b = thread_key("bot-b", "42");
        assert_ne!(a, b);
        assert_eq!(thread_key("bot-a", "42"), thread_key("bot-a", "42"));
        assert!(a.contains("bot-a"));
    }

    #[test]
    fn named_channel_reports_the_configured_name() {
        let inner = Arc::new(pantheon_gateway::MemoryChannel::new("telegram"));
        let named = NamedChannel::new("bot-a".to_string(), inner);
        assert_eq!(named.name(), "bot-a");
        // The inner channel still does the transport work.
        assert!(named.poll().is_empty());
    }

    fn spec(name: &str, platform: &str) -> ChannelSpec {
        ChannelSpec {
            name: name.to_string(),
            platform: platform.to_string(),
            token: "t".to_string(),
            profile: None,
            voice_replies: false,
        }
    }

    #[test]
    fn cursor_files_keep_legacy_names() {
        let dir = Path::new("/tmp/state");
        // Legacy channels keep their historical cursor names so cursors
        // survive the upgrade; explicit channels get cursor-<name>.
        assert_eq!(
            cursor_file(dir, &spec("telegram", "telegram")),
            dir.join("tg-cursor")
        );
        assert_eq!(
            cursor_file(dir, &spec("discord", "discord")),
            dir.join("discord-cursor")
        );
        assert_eq!(
            cursor_file(dir, &spec("bot-a", "telegram")),
            dir.join("cursor-bot-a")
        );
    }
}
