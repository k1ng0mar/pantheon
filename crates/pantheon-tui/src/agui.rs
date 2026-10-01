//! AG-UI CLI verb: serve, plus the post-grant resume. Thin surface over
//! pantheon-api + pantheon-gateway; no business logic here.
//!
//! `pantheon serve` and `pantheon dashboard` are the same unified gateway
//! listener: one token, one port, serving the dashboard control plane
//! (`/api/*` + PWA assets) and the AG-UI surface (`/agui/*`, voice
//! endpoints) together. The shared pieces below (session factory,
//! voice edge, live-voice config, genui base, the [`AguiServeConfig`])
//! live in one builder so the two commands cannot drift apart.
use crate::terminal::data_dir;
/// Reject a flag whose value cannot be used, naming the flag and what it
/// expected. Silently falling back to a default meant the user talked to a
/// different server than the one they asked for.
fn fatal_bad_flag(flag: &str, value: &str, expected: &str) -> ! {
    eprintln!("serve: {flag} expects {expected}, got {value:?}");
    std::process::exit(2);
}

fn flag(args: &[String], name: &str) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == name && i + 1 < args.len() {
            return Some(args[i + 1].clone());
        }
        i += 1;
    }
    None
}

/// The listener's single token: `PANTHEON_SERVE_TOKEN` when non-empty,
/// else a fresh 256-bit token, printed once at startup. The dashboard
/// control plane and the AG-UI routes share it — one front door.
///
/// INVARIANT: the dashboard `App.token` and the gateway `AuthCtx.token`
/// are the same value. Callers resolve the token once and pass it to
/// both [`build_agui_serve_parts`] and the dashboard mount; the
/// `AuthCtx` is built from the dashboard mount's own `auth_ctx()` so the
/// two cannot drift.
pub(crate) fn resolve_serve_token(command: &str) -> String {
    match std::env::var("PANTHEON_SERVE_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
    {
        Some(t) => t,
        None => {
            let t = pantheon_gateway::http::generate_token();
            eprintln!("{command}: no PANTHEON_SERVE_TOKEN set; generated a one-time token:");
            eprintln!("  {t}");
            t
        }
    }
}

/// The CLI's approval callback: no live session to resume here, so the
/// approval decision is surfaced on stderr for the operator.
pub(crate) fn approval_callback() -> pantheon_dashboard::ApprovalCallback {
    std::sync::Arc::new(|run_id: &str, granted: bool| {
        eprintln!(
            "dashboard: approval {} for run {run_id}",
            if granted { "granted" } else { "denied" }
        );
    })
}

/// Build the AG-UI half of the unified serve surface: registers the
/// session factory, builds the voice edge / live-voice config / genui
/// base, and returns the [`AguiServeConfig`].
///
/// `token` is the listener's single token ([`resolve_serve_token`]); the
/// AG-UI web client needs it injected so its fetch calls carry auth.
pub(crate) fn build_agui_serve_parts(
    data_dir: &std::path::Path,
    host: &str,
    port: u16,
    token: &str,
) -> pantheon_runtime::agui_serve::AguiServeConfig {
    use crate::config;
    use crate::config::build_model_policy;
    use std::sync::Arc;

    // Resolve the Session exactly as the terminal interface does: the
    // config document, its key names, and its auxiliary models. The AG-UI
    // surface used to translate a subset of config.toml into environment
    // variables and let the runtime read those, which silently dropped
    // every auxiliary section and any key name the runtime does not know.
    // A builder keeps one definition of "what a turn runs on".
    let file_cfg = config::Config::load_or_report(data_dir);
    let startup_cfg = file_cfg.clone();
    pantheon_runtime::agui::set_session_factory(Arc::new(move |dir: &std::path::PathBuf| {
        // Reload per turn so a config edited while the server runs is
        // picked up without a restart.
        let cfg = config::Config::load(dir).ok().or_else(|| file_cfg.clone());
        let model_policy = build_model_policy(cfg.as_ref(), None, None);
        let policy = crate::config_schema::policy_for_config(&cfg);
        let secrets = config::chat_secrets(cfg.as_ref());
        pantheon_runtime::session::Session::new(dir.clone(), policy, model_policy, secrets).inspect(
            |s| {
                config::apply_tool_enablement(s, cfg.as_ref());
                config::apply_budget_tiers(s, cfg.as_ref());
            },
        )
    }));

    let base = std::env::var("PANTHEON_GENUI_BASE")
        .unwrap_or_else(|_| format!("http://{host}:{port}/agui/blob"));
    // Voice edge for the mobile app's speech endpoints: resolved from the
    // [stt]/[tts] sections the setup wizard writes. Absent sections mean
    // the routes 400 with `voice_not_configured`.
    let voice = pantheon_gateway::voice::VoiceEdge::from_config(
        startup_cfg.as_ref().and_then(|c| c.tools.as_ref()),
        startup_cfg.as_ref().and_then(|c| c.stt.as_ref()),
        startup_cfg.as_ref().and_then(|c| c.tts.as_ref()),
        &config::chat_secrets(startup_cfg.as_ref()),
    );
    // Live voice mode (`GET /agui/voice/live`): same double gate as the
    // edge, plus the `[voice]` limits. The per-session gate refuses when
    // `live_enabled` is false or a backend is missing.
    let live_voice = Arc::new(pantheon_gateway::live_voice::LiveVoiceConfig::from_config(
        startup_cfg.as_ref().and_then(|c| c.tools.as_ref()),
        startup_cfg.as_ref().and_then(|c| c.stt.as_ref()),
        startup_cfg.as_ref().and_then(|c| c.tts.as_ref()),
        startup_cfg
            .as_ref()
            .map(|c| c.live_voice())
            .unwrap_or_default(),
        &config::chat_secrets(startup_cfg.as_ref()),
    ));
    pantheon_runtime::agui_serve::AguiServeConfig {
        data_dir: data_dir.to_path_buf(),
        host: host.to_string(),
        port,
        genui_base: base,
        auth_token: Some(token.to_string()),
        voice,
        live_voice,
    }
}

pub fn cmd_serve(args: &[String]) {
    use crate::config;

    // Help is a question with no side effects: it must print and exit
    // BEFORE any config load, token generation, or bind. (A generated
    // token printed for a `--help` invocation is a live credential on a
    // terminal nobody asked to serve from.)
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "pantheon serve [--port 18789] [--host 127.0.0.1] [--bind 127.0.0.1]\n\
             \n\
             Start the unified serve surface: the dashboard control plane\n\
             (/api/* + PWA assets) and the AG-UI surface (/agui/*, voice\n\
             endpoints) on one listener with one token.\n\
             \n\
             --host is the canonical bind-address flag; --bind is accepted\n\
             as an alias so `serve` and `dashboard` take the same flags.\n\
             The [server] table in config.toml supplies defaults when the\n\
             flags are absent.\n\
             \n\
             The per-instance token prints at startup (or set\n\
             PANTHEON_SERVE_TOKEN to pin it). Keep it on 127.0.0.1;\n\
             exposing it publicly requires a reverse proxy with real\n\
             authentication."
        );
        return;
    }

    // Flags win, then `[server]`, then the built-in default. A flag that is
    // present but unparseable is an error: silently binding 18789 after
    // `--port abc` leaves the user talking to the wrong server.
    let dir = data_dir();
    let file_cfg = config::Config::load_or_report(&dir);
    let server = file_cfg.as_ref().and_then(|c| c.server.clone());
    let port: u16 = match flag(args, "--port") {
        Some(v) => v
            .parse()
            .unwrap_or_else(|_| fatal_bad_flag("--port", &v, "a port number")),
        None => server.as_ref().map(|s| s.port).unwrap_or(18789),
    };
    // B-14: `--host` is canonical; `--bind` (the dashboard's historic
    // name) is accepted as an alias on both commands.
    let host = flag(args, "--host")
        .or_else(|| flag(args, "--bind"))
        .or_else(|| server.as_ref().map(|s| s.host.clone()))
        .unwrap_or_else(|| "127.0.0.1".into());
    let token = resolve_serve_token("pantheon serve");
    let agui_cfg = build_agui_serve_parts(&dir, &host, port, &token);
    // The dashboard control plane mounts on the same listener (one token,
    // one port). `open_browser` stays off for `serve`; the dashboard
    // command owns that flag.
    let bind_all = host == "0.0.0.0" || host == "::";
    let dash_mount = std::sync::Arc::new(pantheon_dashboard::DashboardMount::new(
        pantheon_dashboard::App {
            data_dir: dir.clone(),
            token,
            bind: host.clone(),
            bind_all,
            on_approval: Some(approval_callback()),
            send_locks: Default::default(),
            turn_children: Default::default(),
            swarm: pantheon_dashboard::swarm::orchestrator_for(&dir),
        },
    ));
    // Single-token invariant: the gateway's auth context is built from the
    // dashboard mount's own `auth_ctx()`, so the token the AG-UI routes
    // and the dashboard routes enforce is the same value.
    let auth = dash_mount.auth_ctx();
    let agui_mount = std::sync::Arc::new(pantheon_runtime::agui_serve::AguiMount { cfg: agui_cfg });
    let cfg = pantheon_gateway::http::ServerConfig {
        bind_addr: format!("{host}:{port}"),
        auth,
        mounts: vec![dash_mount, agui_mount],
        label: "pantheon serve".to_string(),
    };
    if let Err(e) = pantheon_gateway::http::serve(cfg) {
        eprintln!("serve: {e}");
        std::process::exit(1);
    }
}
/// Continue a parked run after a grant: an empty turn rebuilds the
/// transcript from the ledger and settles the granted call. Never resends
/// the original user message, which would append a duplicate turn.
pub fn resume_after_grant(run_id: &str) {
    use crate::config;
    use crate::config::build_model_policy;
    let file_cfg = config::Config::load_or_report(&data_dir());
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
    let session =
        match pantheon_runtime::session::Session::new(data_dir(), policy, model_policy, secrets) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("open session: {e}");
                std::process::exit(1);
            }
        };
    config::apply_tool_enablement(&session, file_cfg.as_ref());
    config::apply_budget_tiers(&session, file_cfg.as_ref());
    match session.chat_turn(run_id, "", "") {
        Ok(_) => {
            println!("run {run_id} continued");
            crate::terminal::drain_queued_turns(&session, run_id, "session");
        }
        Err(e) => {
            eprintln!("resume {run_id}: {e}");
            std::process::exit(1);
        }
    }
}
