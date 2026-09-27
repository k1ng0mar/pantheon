//! AG-UI CLI verb: serve, plus the post-grant resume. Thin surface over
//! pantheon-api + pantheon-gateway; no business logic here.
use super::data_dir;
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
pub fn cmd_serve(args: &[String]) {
    use crate::config_doc;
    use crate::config_doc::build_model_policy;
    use std::sync::Arc;

    // Resolve the Session exactly as the terminal interface does: the
    // config document, its key names, and its auxiliary models. The AG-UI
    // surface used to translate a subset of config.toml into environment
    // variables and let the runtime read those, which silently dropped
    // every auxiliary section and any key name the runtime does not know.
    // A builder keeps one definition of "what a turn runs on".
    let factory_dir = data_dir();
    let file_cfg = config_doc::Config::load_or_report(&factory_dir);
    let startup_cfg = file_cfg.clone();
    pantheon_runtime::agui::set_session_factory(Arc::new(move |dir: &std::path::PathBuf| {
        // Reload per turn so a config edited while the server runs is
        // picked up without a restart.
        let cfg = config_doc::Config::load(dir)
            .ok()
            .or_else(|| file_cfg.clone());
        let model_policy = build_model_policy(cfg.as_ref(), None, None);
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
    let cfg = pantheon_runtime::ServeConfig {
        data_dir: data_dir(),
        host,
        port,
        genui_base: base,
        auth_token: std::env::var("PANTHEON_SERVE_TOKEN")
            .ok()
            .filter(|t| !t.is_empty()),
    };
    if let Err(e) = pantheon_runtime::serve(cfg) {
        eprintln!("serve: {e}");
        std::process::exit(1);
    }
}
/// Continue a parked run after a grant: an empty turn rebuilds the
/// transcript from the ledger and settles the granted call. Never resends
/// the original user message, which would append a duplicate turn.
pub fn resume_after_grant(run_id: &str) {
    use crate::config_doc;
    use crate::config_doc::build_model_policy;
    let file_cfg = config_doc::Config::load_or_report(&data_dir());
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
