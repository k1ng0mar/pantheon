//! AG-UI CLI verbs: serve / stream / channel. Thin surface over
//! pantheon-api + pantheon-gateway; no business logic here.
use super::{data_dir, ext_dir};
use pantheon_gateway::SseEncoder;
use std::path::PathBuf;
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
fn threads_file() -> PathBuf {
    data_dir().join("threads.json")
}
fn read_thread(run_id: &str) -> Option<String> {
    let raw = std::fs::read_to_string(threads_file()).ok()?;
    let map: std::collections::HashMap<String, String> = serde_json::from_str(&raw).ok()?;
    map.get(run_id).cloned()
}
pub fn cmd_serve(args: &[String]) {
    use crate::config_doc;
    use crate::session_cli::build_model_policy;
    use std::sync::Arc;

    // Resolve the Session exactly as `pantheon chat` and the REPL do: the
    // config document, its key names, and its auxiliary models. The AG-UI
    // surface used to translate a subset of config.toml into environment
    // variables and let the runtime read those, which silently dropped
    // every auxiliary section and any key name the runtime does not know.
    // A builder keeps one definition of "what a turn runs on".
    let factory_dir = data_dir();
    let file_cfg = config_doc::Config::load_or_report(&factory_dir);
    let startup_cfg = file_cfg.clone();
    pantheon_api::agui::set_session_factory(Arc::new(move |dir: &std::path::PathBuf| {
        // Reload per turn so a config edited while the server runs is
        // picked up without a restart.
        let cfg = config_doc::Config::load(dir)
            .ok()
            .or_else(|| file_cfg.clone());
        let model_policy = build_model_policy(&cfg, None, None);
        let policy = crate::config_schema::policy_for_config(&cfg);
        let secrets = config_doc::chat_secrets(cfg.as_ref());
        pantheon_runtime::session::Session::new(dir.clone(), policy, model_policy, secrets)
    }));

    // Flags win, then `[server]`, then the built-in default. A flag that is
    // present but unparseable is an error: silently binding 18789 after
    // `--port abc` leaves the user talking to the wrong server.
    let server = startup_cfg.as_ref().and_then(|c| c.server.clone());
    let port: u16 = match flag(args, "--port") {
        Some(v) => v
            .parse()
            .unwrap_or_else(|_| fatal_bad_flag("--port", &v, "a port number")),
        None => server.as_ref().map(|s| s.port).unwrap_or(18789),
    };
    let host = flag(args, "--host")
        .or_else(|| server.as_ref().map(|s| s.host.clone()))
        .unwrap_or_else(|| "127.0.0.1".into());
    let base = std::env::var("PANTHEON_GENUI_BASE")
        .unwrap_or_else(|_| format!("http://{host}:{port}/agui/blob"));
    let cfg = pantheon_api::ServeConfig {
        data_dir: data_dir(),
        host,
        port,
        genui_base: base,
        auth_token: std::env::var("PANTHEON_SERVE_TOKEN")
            .ok()
            .filter(|t| !t.is_empty()),
    };
    if let Err(e) = pantheon_api::serve(cfg) {
        eprintln!("serve: {e}");
        std::process::exit(1);
    }
}
pub fn cmd_stream(args: &[String]) {
    if args.len() < 3 {
        eprintln!("usage: pantheon stream <run_id> [--thread T] [--after N]");
        std::process::exit(2);
    }
    let run_id = args[2].clone();
    let thread = flag(args, "--thread")
        .or_else(|| read_thread(&run_id))
        .unwrap_or_else(|| format!("cli:{run_id}"));
    let after: i64 = flag(args, "--after")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let frames = pantheon_api::snapshot_frames(&data_dir(), &run_id, &thread, after);
    print!("{}", SseEncoder.frames(&frames));
}
/// Continue a parked run after a grant: an empty turn rebuilds the
/// transcript from the ledger and settles the granted call. Never resends
/// the original user message, which would append a duplicate turn.
pub fn resume_after_grant(run_id: &str) {
    use crate::config_doc;
    use crate::session_cli::build_model_policy;
    let file_cfg = config_doc::Config::load_or_report(&data_dir());
    let model_policy = build_model_policy(&file_cfg, None, None);
    let allow_memory = file_cfg
        .as_ref()
        .map(|c| c.policy == Some(crate::config_schema::PolicyPreset::CoderMemory))
        .unwrap_or_else(|| {
            std::env::var("PANTHEON_ALLOW_MEMORY")
                .map(|v| v == "1" || v == "true")
                .unwrap_or(false)
        });
    let policy = if allow_memory {
        pantheon_core::capability::Policy::coder_with_memory()
    } else {
        pantheon_core::capability::Policy::coder()
    };
    let secrets = config_doc::chat_secrets(file_cfg.as_ref());
    let session =
        match pantheon_runtime::session::Session::new(data_dir(), policy, model_policy, secrets) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("open session: {e}");
                std::process::exit(1);
            }
        };
    match session.chat_turn(run_id, "", "") {
        Ok(_) => println!("run {run_id} continued"),
        Err(e) => {
            eprintln!("resume {run_id}: {e}");
            std::process::exit(1);
        }
    }
}
/// Channel demo: replay a run's frames through the transport seam into an
/// in-memory surface and print the shared text fallback. Proves discord /
/// slack / web consume the same stream without a live surface.
pub fn cmd_channel(args: &[String]) {
    use pantheon_gateway::{format_text, Channel, ChannelEnvelope, MemoryChannel};
    if args.len() < 3 {
        eprintln!("usage: pantheon channel <run_id> [--thread T]");
        std::process::exit(2);
    }
    let run_id = args[2].clone();
    let thread = flag(args, "--thread")
        .or_else(|| read_thread(&run_id))
        .unwrap_or_else(|| format!("cli:{run_id}"));
    let frames = pantheon_api::snapshot_frames(&data_dir(), &run_id, &thread, 0);
    let web = MemoryChannel::new("web");
    let _ = ext_dir;
    for f in &frames {
        let _ = web.send(ChannelEnvelope {
            thread_id: thread.clone(),
            frame: f.clone(),
        });
    }
    for env in web.drain_outbound() {
        println!("{}", format_text(&env.frame));
    }
}
