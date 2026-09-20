//! pantheon CLI: thin surface over the runtime. No business logic here.
use pantheon_core::events::Event;
use pantheon_extensions::{doctor, ExtensionManager, Hook, RunnerConfig};
use pantheon_runtime::{new_run_id, Supervisor};
use std::collections::HashSet;
use std::path::PathBuf;

fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("PANTHEON_DATA_DIR") { return PathBuf::from(d); }
    if let Ok(h) = std::env::var("HOME") { return PathBuf::from(h).join(".pantheon"); }
    PathBuf::from(".pantheon-data")
}
fn ext_dir() -> PathBuf {
    if let Ok(d) = std::env::var("PANTHEON_EXT_DIR") { return PathBuf::from(d); }
    data_dir().join("extensions")
}
fn usage() -> String {
    "pantheon <run|explain|status|extensions|hook|doctor> ...\n\
     \u{20} run [--id ID] [--say TEXT] [--tool NAME] [--fail CODE] [--ext] [--platform P]\n\
     \u{20} explain <run_id>\n\
     \u{20} status <run_id>\n\
     \u{20} extensions  list loaded extensions\n\
     \u{20} hook <name> [--session S] [--platform P]  fire a hook\n\
     \u{20} doctor <plugin_dir>  loud preflight report\n".into()
}
fn load_mgr() -> ExtensionManager {
    let mut m = ExtensionManager::new(RunnerConfig::default());
    let d = ext_dir();
    if d.exists() { let _ = m.load_dir(&d); }
    m
}

/// Persisted seen-(plugin, hook, session) set so `hook` CLI invocations
/// (fresh process per call) still honor once-per-session. Daemon/API path
/// uses the in-memory set; CLI path merges file state then writes back.
fn seen_file() -> PathBuf {
    data_dir().join("hook_seen.json")
}

fn read_seen() -> HashSet<(String, String, String)> {
    let mut out = HashSet::new();
    let Ok(text) = std::fs::read_to_string(seen_file()) else { return out; };
    if let Ok(arr) = serde_json::from_str::<Vec<Vec<String>>>(&text) {
        for row in arr {
            if row.len() == 3 {
                out.insert((row[0].clone(), row[1].clone(), row[2].clone()));
            }
        }
    }
    out
}

fn write_seen(keys: &HashSet<(String, String, String)>) {
    let mut arr: Vec<Vec<String>> = keys.iter().map(|(a, b, c)| vec![a.clone(), b.clone(), c.clone()]).collect();
    arr.sort();
    if let Some(parent) = seen_file().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string(&arr) {
        let _ = std::fs::write(seen_file(), text);
    }
}

fn cli_fire(hook: Hook, session: &str, platform: &str) -> Option<String> {
    let mgr = load_mgr();
    mgr.preseed_seen(read_seen());
    let out = mgr.fire(hook, session, platform);
    write_seen(&mgr.seen_snapshot());
    out
}
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 { eprint!("{}", usage()); std::process::exit(2); }
    match args[1].as_str() {
        "run" => {
            let mut id: Option<String> = None;
            let mut say: Option<String> = None;
            let mut tool: Option<String> = None;
            let mut fail: Option<String> = None;
            let mut with_ext = false;
            let mut platform = String::from("cli");
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--id" => { i += 1; if i < args.len() { id = Some(args[i].clone()); } }
                    "--say" => { i += 1; if i < args.len() { say = Some(args[i].clone()); } }
                    "--tool" => { i += 1; if i < args.len() { tool = Some(args[i].clone()); } }
                    "--fail" => { i += 1; if i < args.len() { fail = Some(args[i].clone()); } }
                    "--ext" => { with_ext = true; }
                    "--platform" => { i += 1; if i < args.len() { platform = args[i].clone(); } }
                    _ => {}
                }
                i += 1;
            }
            let sup = match Supervisor::open(data_dir()) {
                Ok(s) => s, Err(e) => { eprintln!("open runtime: {e}"); std::process::exit(1); }
            };
            let run_id = id.unwrap_or_else(new_run_id);
            let recovered = sup.start_run(&run_id).unwrap_or_else(|e| {
                eprintln!("start: {e}"); std::process::exit(1); });
            if recovered { println!("(recovered unfinished run {run_id})"); }
            if with_ext {
                let mgr = load_mgr();
                mgr.preseed_seen(read_seen());
                let fired = mgr.fire(Hook::PreLlmCall, &run_id, &platform);
                write_seen(&mgr.seen_snapshot());
                if let Some(ctx) = fired {
                    sup.emit(Event::RunProgress {
                        run_id: run_id.clone(),
                        detail: format!("ext pre_llm_call injected {} chars", ctx.len()),
                    }).unwrap();
                    println!("--- injected context ---\n{ctx}\n--- end ---");
                } else {
                    println!("(no extension context)");
                }
            }
            if let Some(t) = tool {
                sup.emit(Event::ToolStarted { run_id: run_id.clone(), tool: t }).unwrap();
            }
            if let Some(s) = say {
                sup.emit(Event::RunProgress { run_id: run_id.clone(), detail: s }).unwrap();
            }
            if let Some(code) = fail { sup.fail(&run_id, &code).unwrap(); }
            else { sup.complete(&run_id).unwrap(); }
            println!("{run_id}");
        }
        "explain" => {
            if args.len() < 3 { eprintln!("usage: pantheon explain <run_id>"); std::process::exit(2); }
            let sup = Supervisor::open(data_dir()).unwrap_or_else(|e| {
                eprintln!("open runtime: {e}"); std::process::exit(1); });
            match sup.explain(&args[2]) {
                Ok(t) => println!("{t}"),
                Err(e) => { eprintln!("explain: {e}"); std::process::exit(1); }
            }
        }
        "status" => {
            if args.len() < 3 { eprintln!("usage: pantheon status <run_id>"); std::process::exit(2); }
            let sup = Supervisor::open(data_dir()).unwrap_or_else(|e| {
                eprintln!("open runtime: {e}"); std::process::exit(1); });
            match sup.ledger_status(&args[2]) {
                Ok(s) => println!("{}", s.as_deref().unwrap_or("unknown")),
                Err(e) => { eprintln!("status: {e}"); std::process::exit(1); }
            }
        }
        "extensions" => {
            let mgr = load_mgr();
            for n in mgr.names() { println!("{n}"); }
        }
        "hook" => {
            if args.len() < 3 { eprintln!("usage: pantheon hook <name> [--session S]"); std::process::exit(2); }
            let hook = match Hook::parse(&args[2]) {
                Some(h) => h, None => { eprintln!("unknown hook {}", args[2]); std::process::exit(2); }
            };
            let mut session = String::from("default");
            let mut platform = String::from("cli");
            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "--session" => { i += 1; if i < args.len() { session = args[i].clone(); } }
                    "--platform" => { i += 1; if i < args.len() { platform = args[i].clone(); } }
                    _ => {}
                }
                i += 1;
            }
            match cli_fire(hook, &session, &platform) {
                Some(c) => println!("{c}"),
                None => println!("(silent)"),
            }
        }
        "doctor" => {
            if args.len() < 3 { eprintln!("usage: pantheon doctor <plugin_dir>"); std::process::exit(2); }
            let rep = doctor(std::path::Path::new(&args[2]));
            println!("{}", serde_json::to_string_pretty(&rep).unwrap());
            if !rep.ok { std::process::exit(1); }
        }
        _ => { eprint!("{}", usage()); std::process::exit(2); }
    }
}
