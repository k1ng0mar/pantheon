//! AG-UI CLI verbs: serve / stream / grant / deny / sign. Thin surface over
//! pantheon-api + pantheon-gateway; no business logic here.
use super::{data_dir, ext_dir};
use pantheon_gateway::{valid_task_id, GenUiSigner, SseEncoder};
use std::path::PathBuf;
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
    let file_cfg = config_doc::Config::load(&factory_dir).ok();
    pantheon_api::agui::set_session_factory(Arc::new(move |dir: &std::path::PathBuf| {
        // Reload per turn so a config edited while the server runs is
        // picked up without a restart.
        let cfg = config_doc::Config::load(dir)
            .ok()
            .or_else(|| file_cfg.clone());
        let model_policy = build_model_policy(&cfg, None, None);
        let allow_memory = cfg
            .as_ref()
            .map(|c| c.policy == Some(crate::config_schema::PolicyPreset::CoderMemory))
            .unwrap_or(false);
        let policy = if allow_memory {
            pantheon_core::capability::Policy::coder_with_memory()
        } else {
            pantheon_core::capability::Policy::coder()
        };
        let secrets = config_doc::chat_secrets(cfg.as_ref());
        pantheon_runtime::session::Session::new(dir.clone(), policy, model_policy, secrets)
    }));

    let port: u16 = flag(args, "--port")
        .and_then(|v| v.parse().ok())
        .unwrap_or(18789);
    let host = flag(args, "--host").unwrap_or_else(|| "127.0.0.1".into());
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
pub fn cmd_grant(args: &[String]) {
    if args.len() < 4 {
        eprintln!("usage: pantheon grant <run_id> <scope>");
        std::process::exit(2);
    }
    let sup = pantheon_runtime::Supervisor::open(data_dir()).unwrap_or_else(|e| {
        eprintln!("open runtime: {e}");
        std::process::exit(1);
    });
    match sup.grant(&args[2], &args[3]) {
        Ok(()) => {
            println!("granted {} {}", args[2], args[3]);
            // A grant only records permission; the run is still parked and
            // the granted call has not executed. Unless the caller opted
            // out, continue it here so `grant` means "approve and finish",
            // not "approve and go read the docs to find the next command".
            if !args.iter().any(|a| a == "--no-resume") {
                resume_after_grant(&args[2]);
            }
        }
        Err(e) => {
            eprintln!("grant: {e}");
            std::process::exit(1);
        }
    }
}

/// Continue a parked run after a grant: an empty turn rebuilds the
/// transcript from the ledger and settles the granted call. Never resends
/// the original user message, which would append a duplicate turn.
fn resume_after_grant(run_id: &str) {
    use crate::config_doc;
    use crate::session_cli::build_model_policy;
    let file_cfg = config_doc::Config::load(&data_dir()).ok();
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
pub fn cmd_deny(args: &[String]) {
    if args.len() < 3 {
        eprintln!("usage: pantheon deny <run_id> [scope]");
        std::process::exit(2);
    }
    let sup = pantheon_runtime::Supervisor::open(data_dir()).unwrap_or_else(|e| {
        eprintln!("open runtime: {e}");
        std::process::exit(1);
    });
    let scope = if let Some(scope) = args.get(3) {
        scope.clone()
    } else {
        let entries = sup.replay(&args[2]).unwrap_or_else(|e| {
            eprintln!("replay: {e}");
            std::process::exit(1);
        });
        let resolved: std::collections::HashSet<String> = entries
            .iter()
            .filter_map(|entry| match &entry.event {
                pantheon_core::events::Event::ApprovalGranted { scope, .. }
                | pantheon_core::events::Event::ApprovalDenied { scope, .. } => Some(scope.clone()),
                _ => None,
            })
            .collect();
        entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.event {
                pantheon_core::events::Event::ApprovalRequested { scope, .. }
                    if !resolved.contains(scope) =>
                {
                    Some(scope.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| {
                eprintln!("deny: no pending approval scope; pass one explicitly");
                std::process::exit(1);
            })
    };
    match sup.deny(&args[2], &scope) {
        Ok(()) => println!("denied {}", args[2]),
        Err(e) => {
            eprintln!("deny: {e}");
            std::process::exit(1);
        }
    }
}
pub fn cmd_sign(args: &[String]) {
    if args.len() < 3 {
        eprintln!("usage: pantheon sign <task_id> [--mime M] [--ttl MS]");
        std::process::exit(2);
    }
    let task = &args[2];
    if !valid_task_id(task) {
        eprintln!("sign: bad task_id");
        std::process::exit(2);
    }
    let mime = flag(args, "--mime").unwrap_or_else(|| "application/octet-stream".into());
    let ttl: i64 = flag(args, "--ttl")
        .and_then(|v| v.parse().ok())
        .unwrap_or(3600_000);
    if ttl <= 0 {
        eprintln!("sign: ttl_ms must be positive");
        std::process::exit(2);
    }
    let base = std::env::var("PANTHEON_GENUI_BASE")
        .unwrap_or_else(|_| "http://127.0.0.1:18789/agui/blob".into());
    let secret = std::env::var("PANTHEON_GENUI_SECRET")
        .map(|s| s.into_bytes())
        .unwrap_or_else(|_| b"pantheon-dev-genui-secret".to_vec());
    let r = GenUiSigner::new(base, secret).sign(task, &mime, ttl);
    println!("{}", serde_json::to_string_pretty(&r).unwrap());
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
