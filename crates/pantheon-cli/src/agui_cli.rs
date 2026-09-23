//! AG-UI CLI verbs: serve / stream / grant / deny / sign. Thin surface over
//! pantheon-api + pantheon-gateway; no business logic here.
use super::{data_dir, ext_dir};
use pantheon_gateway::{GenUiSigner, SseEncoder};
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
        Ok(()) => println!("granted {} {}", args[2], args[3]),
        Err(e) => {
            eprintln!("grant: {e}");
            std::process::exit(1);
        }
    }
}
pub fn cmd_deny(args: &[String]) {
    if args.len() < 3 {
        eprintln!("usage: pantheon deny <run_id> [scope]");
        std::process::exit(2);
    }
    let scope = args.get(3).cloned().unwrap_or_default();
    let sup = pantheon_runtime::Supervisor::open(data_dir()).unwrap_or_else(|e| {
        eprintln!("open runtime: {e}");
        std::process::exit(1);
    });
    match sup.fail(&args[2], &format!("APPROVAL_DENIED:{scope}")) {
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
    if task.contains('/') || task.contains('.') || task.is_empty() {
        eprintln!("sign: bad task_id");
        std::process::exit(2);
    }
    let mime = flag(args, "--mime").unwrap_or_else(|| "application/octet-stream".into());
    let ttl: i64 = flag(args, "--ttl")
        .and_then(|v| v.parse().ok())
        .unwrap_or(3600_000);
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
