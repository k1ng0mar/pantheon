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

use pantheon_core::capability::Policy;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// The runtime-backed sink. Maps channel threads to run ids through a
/// thread→run map so a conversation keeps its run across messages.
struct RuntimeSink {
    data_dir: PathBuf,
    policy: Policy,
    threads: Mutex<std::collections::HashMap<String, String>>,
    outbound: Arc<Mutex<Vec<pantheon_gateway::OutboundMessage>>>,
}

fn open_session(
    data_dir: &PathBuf,
    policy: Policy,
) -> Result<pantheon_runtime::session::Session, pantheon_core::error::PantheonError> {
    let default = pantheon_core::model::DefaultModel {
        provider: std::env::var("PANTHEON_PROVIDER").unwrap_or_else(|_| "local".into()),
        model: std::env::var("PANTHEON_MODEL").unwrap_or_else(|_| "llama3.2".into()),
    };
    let model_policy = pantheon_core::model::ModelPolicy {
        default,
        fallbacks: pantheon_core::model::FallbackChain::default(),
        auxiliaries: vec![],
    };
    let api_key = std::env::var("PANTHEON_API_KEY").unwrap_or_default();
    pantheon_runtime::session::Session::new(data_dir.clone(), policy, model_policy, api_key)
}

impl RuntimeSink {
    fn frame_to_text(frame: &pantheon_gateway::UiFrame) -> String {
        pantheon_gateway::format_text(frame)
    }
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
    fn on_message(&self, thread_id: &str, text: &str) {
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
                    pantheon_agent::LoopOutcome::Answered(t) => t,
                    pantheon_agent::LoopOutcome::AwaitingApproval { capability } => format!(
                        "approval needed: {capability:?}. Reply 'grant {run_id} <scope>' or 'deny'."
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
                self.push_outbound(thread_id, format!("error: {}", e.code));
            }
        }
    }

    fn on_approval(&self, thread_id: &str, scope: &str, grant: bool) {
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
            Err(e) => self.push_outbound(thread_id, format!("error: {}", e.code)),
        }
    }
}

pub fn cmd_gateway(_args: &[String]) {
    let discord_token = std::env::var("PANTHEON_DISCORD_TOKEN").ok();
    let telegram_token = std::env::var("PANTHEON_TELEGRAM_BOT_TOKEN").ok();
    if discord_token.is_none() && telegram_token.is_none() {
        eprintln!("gateway: set PANTHEON_DISCORD_TOKEN and/or PANTHEON_TELEGRAM_BOT_TOKEN");
        std::process::exit(2);
    }
    let data_dir = super::data_dir();
    let outbound = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::new(RuntimeSink {
        data_dir: data_dir.clone(),
        policy: Policy::coder_with_memory(),
        threads: Mutex::new(std::collections::HashMap::new()),
        outbound: outbound.clone(),
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

    eprintln!("gateway running; Ctrl+C to stop");
    for h in handles {
        let _ = h.join();
    }
}
