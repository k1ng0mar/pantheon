//! pantheon CLI: thin surface over the runtime. No business logic here.
use pantheon_core::events::Event;
use pantheon_exec::safewrite::{preview_edit, SafeWriter};
use pantheon_extensions::{doctor, ExtensionManager, Hook, RunnerConfig};
use pantheon_runtime::{new_run_id, Supervisor};
use std::collections::HashSet;
use std::path::PathBuf;

fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("PANTHEON_DATA_DIR") {
        return PathBuf::from(d);
    }
    if let Ok(h) = std::env::var("HOME") {
        return PathBuf::from(h).join(".pantheon");
    }
    PathBuf::from(".pantheon-data")
}
fn ext_dir() -> PathBuf {
    if let Ok(d) = std::env::var("PANTHEON_EXT_DIR") {
        return PathBuf::from(d);
    }
    data_dir().join("extensions")
}
fn safewrite_dir() -> PathBuf {
    data_dir().join("safewrite")
}
fn usage() -> String {
    "pantheon <chat|run|explain|status|extensions|hook|doctor|preview|stage|apply|checkpoint|rollback> ...\n\
     \u{20} chat [--id ID] [--model M] [--provider P] [--key K] \"message\"\n\
     \u{20} run [--id ID] [--say TEXT] [--tool NAME] [--fail CODE] [--ext] [--platform P]\n\
     \u{20} explain <run_id>\n\
     \u{20} status <run_id>\n\
     \u{20} extensions  list loaded extensions\n\
     \u{20} hook <name> [--session S] [--platform P]  fire a hook\n\
     \u{20} doctor <plugin_dir>  loud preflight report\n\
     \u{20} preview <path> <file-with-new-content>  read-only diff preview\n\
     \u{20} stage <path> <file-with-new-content> [--expect HASH]  stage one edit\n\
     \u{20} apply <path> <file-with-new-content> [--expect HASH] [--run ID]  checkpoint + atomic write\n\
     \u{20} checkpoint <path>... [--run ID]  snapshot pre-images\n\
     \u{20} rollback (--ckpt ID | --seq N)  restore a checkpoint\n"
        .into()
}
fn load_mgr() -> ExtensionManager {
    let mut m = ExtensionManager::new(RunnerConfig::default());
    let d = ext_dir();
    if d.exists() {
        let _ = m.load_dir(&d);
    }
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
    let Ok(text) = std::fs::read_to_string(seen_file()) else {
        return out;
    };
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
    let mut arr: Vec<Vec<String>> = keys
        .iter()
        .map(|(a, b, c)| vec![a.clone(), b.clone(), c.clone()])
        .collect();
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
    if args.len() < 2 {
        eprint!("{}", usage());
        std::process::exit(2);
    }
    match args[1].as_str() {
        "chat" => {
            let mut id: Option<String> = None;
            let mut model: Option<String> = None;
            let mut provider: Option<String> = None;
            let mut key: Option<String> = None;
            let mut message = String::new();
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--id" => {
                        i += 1;
                        if i < args.len() {
                            id = Some(args[i].clone());
                        }
                    }
                    "--model" => {
                        i += 1;
                        if i < args.len() {
                            model = Some(args[i].clone());
                        }
                    }
                    "--provider" => {
                        i += 1;
                        if i < args.len() {
                            provider = Some(args[i].clone());
                        }
                    }
                    "--key" => {
                        i += 1;
                        if i < args.len() {
                            key = Some(args[i].clone());
                        }
                    }
                    _ if message.is_empty() => message = args[i].clone(),
                    _ => {}
                }
                i += 1;
            }
            if message.is_empty() {
                eprintln!("usage: pantheon chat [--id ID] [--model M] [--provider P] \"message\"");
                std::process::exit(2);
            }
            // Model policy: default from env or flags. No routing.
            let default = pantheon_core::model::DefaultModel {
                provider: provider
                    .or_else(|| std::env::var("PANTHEON_PROVIDER").ok())
                    .unwrap_or_else(|| "local".into()),
                model: model
                    .or_else(|| std::env::var("PANTHEON_MODEL").ok())
                    .unwrap_or_else(|| "llama3.2".into()),
            };
            let model_policy = pantheon_core::model::ModelPolicy {
                default,
                fallbacks: pantheon_core::model::FallbackChain::default(),
                auxiliaries: vec![],
            };
            let api_key = key
                .or_else(|| std::env::var("PANTHEON_API_KEY").ok())
                .unwrap_or_default();
            let session = match pantheon_runtime::session::Session::new(
                data_dir(),
                pantheon_core::capability::Policy::coder(),
                model_policy,
                api_key,
            ) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("open session: {e}");
                    std::process::exit(1);
                }
            };
            let run_id = id.unwrap_or_else(pantheon_runtime::new_run_id);
            match session.chat(&run_id, &message) {
                Ok(_) => eprintln!("[run {run_id}]"),
                Err(e) => {
                    eprintln!("run failed: {e}");
                    std::process::exit(1);
                }
            }
        }
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
                    "--id" => {
                        i += 1;
                        if i < args.len() {
                            id = Some(args[i].clone());
                        }
                    }
                    "--say" => {
                        i += 1;
                        if i < args.len() {
                            say = Some(args[i].clone());
                        }
                    }
                    "--tool" => {
                        i += 1;
                        if i < args.len() {
                            tool = Some(args[i].clone());
                        }
                    }
                    "--fail" => {
                        i += 1;
                        if i < args.len() {
                            fail = Some(args[i].clone());
                        }
                    }
                    "--ext" => {
                        with_ext = true;
                    }
                    "--platform" => {
                        i += 1;
                        if i < args.len() {
                            platform = args[i].clone();
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            let sup = match Supervisor::open(data_dir()) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("open runtime: {e}");
                    std::process::exit(1);
                }
            };
            let run_id = id.unwrap_or_else(new_run_id);
            let recovered = sup.start_run(&run_id).unwrap_or_else(|e| {
                eprintln!("start: {e}");
                std::process::exit(1);
            });
            if recovered {
                println!("(recovered unfinished run {run_id})");
            }
            if with_ext {
                let mgr = load_mgr();
                mgr.preseed_seen(read_seen());
                let fired = mgr.fire(Hook::PreLlmCall, &run_id, &platform);
                write_seen(&mgr.seen_snapshot());
                if let Some(ctx) = fired {
                    sup.emit(Event::RunProgress {
                        run_id: run_id.clone(),
                        detail: format!("ext pre_llm_call injected {} chars", ctx.len()),
                    })
                    .unwrap();
                    println!("--- injected context ---\n{ctx}\n--- end ---");
                } else {
                    println!("(no extension context)");
                }
            }
            if let Some(t) = tool {
                sup.emit(Event::ToolStarted {
                    run_id: run_id.clone(),
                    call_id: "cli".into(),
                    tool: t,
                    args: String::new(),
                })
                .unwrap();
            }
            if let Some(s) = say {
                sup.emit(Event::RunProgress {
                    run_id: run_id.clone(),
                    detail: s,
                })
                .unwrap();
            }
            if let Some(code) = fail {
                sup.fail(&run_id, &code).unwrap();
            } else {
                sup.complete(&run_id).unwrap();
            }
            println!("{run_id}");
        }
        "explain" => {
            if args.len() < 3 {
                eprintln!("usage: pantheon explain <run_id>");
                std::process::exit(2);
            }
            let sup = Supervisor::open(data_dir()).unwrap_or_else(|e| {
                eprintln!("open runtime: {e}");
                std::process::exit(1);
            });
            match sup.explain(&args[2]) {
                Ok(t) => println!("{t}"),
                Err(e) => {
                    eprintln!("explain: {e}");
                    std::process::exit(1);
                }
            }
        }
        "status" => {
            if args.len() < 3 {
                eprintln!("usage: pantheon status <run_id>");
                std::process::exit(2);
            }
            let sup = Supervisor::open(data_dir()).unwrap_or_else(|e| {
                eprintln!("open runtime: {e}");
                std::process::exit(1);
            });
            match sup.ledger_status(&args[2]) {
                Ok(s) => println!("{}", s.as_deref().unwrap_or("unknown")),
                Err(e) => {
                    eprintln!("status: {e}");
                    std::process::exit(1);
                }
            }
        }
        "extensions" => {
            let mgr = load_mgr();
            for n in mgr.names() {
                println!("{n}");
            }
        }
        "hook" => {
            if args.len() < 3 {
                eprintln!("usage: pantheon hook <name> [--session S]");
                std::process::exit(2);
            }
            let hook = match Hook::parse(&args[2]) {
                Some(h) => h,
                None => {
                    eprintln!("unknown hook {}", args[2]);
                    std::process::exit(2);
                }
            };
            let mut session = String::from("default");
            let mut platform = String::from("cli");
            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "--session" => {
                        i += 1;
                        if i < args.len() {
                            session = args[i].clone();
                        }
                    }
                    "--platform" => {
                        i += 1;
                        if i < args.len() {
                            platform = args[i].clone();
                        }
                    }
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
            if args.len() < 3 {
                eprintln!("usage: pantheon doctor <plugin_dir>");
                std::process::exit(2);
            }
            let rep = doctor(std::path::Path::new(&args[2]));
            println!("{}", serde_json::to_string_pretty(&rep).unwrap());
            if !rep.ok {
                std::process::exit(1);
            }
        }
        "preview" => {
            if args.len() < 4 {
                eprintln!("usage: pantheon preview <path> <file-with-new-content>");
                std::process::exit(2);
            }
            let new_bytes = std::fs::read(&args[3]).unwrap_or_else(|e| {
                eprintln!("read new content {}: {e}", args[3]);
                std::process::exit(1);
            });
            match preview_edit(std::path::Path::new(&args[2]), &new_bytes) {
                Ok(pv) => println!("{}", serde_json::to_string_pretty(&pv).unwrap()),
                Err(e) => {
                    eprintln!("preview: {e}");
                    std::process::exit(1);
                }
            }
        }
        "stage" => {
            if args.len() < 4 {
                eprintln!("usage: pantheon stage <path> <file-with-new-content> [--expect HASH]");
                std::process::exit(2);
            }
            let mut expect: Option<String> = None;
            let mut i = 4;
            while i < args.len() {
                if args[i] == "--expect" && i + 1 < args.len() {
                    expect = Some(args[i + 1].clone());
                    i += 1;
                }
                i += 1;
            }
            let new_bytes = std::fs::read(&args[3]).unwrap_or_else(|e| {
                eprintln!("read new content {}: {e}", args[3]);
                std::process::exit(1);
            });
            let w = SafeWriter::new(safewrite_dir()).unwrap_or_else(|e| {
                eprintln!("open safewrite state: {e}");
                std::process::exit(1);
            });
            let edit = pantheon_exec::safewrite::FileEdit {
                path: PathBuf::from(&args[2]),
                new_content: new_bytes,
                expected_hash: expect,
            };
            match w.stage_edits(vec![edit]) {
                Ok(b) => println!("{}", serde_json::to_string_pretty(&b).unwrap()),
                Err(e) => {
                    eprintln!("stage: {e}");
                    std::process::exit(1);
                }
            }
        }
        "apply" => {
            if args.len() < 4 {
                eprintln!("usage: pantheon apply <path> <file-with-new-content> [--expect HASH] [--run ID]");
                std::process::exit(2);
            }
            let mut expect: Option<String> = None;
            let mut run_id: Option<String> = None;
            let mut i = 4;
            while i < args.len() {
                if args[i] == "--expect" && i + 1 < args.len() {
                    expect = Some(args[i + 1].clone());
                    i += 1;
                } else if args[i] == "--run" && i + 1 < args.len() {
                    run_id = Some(args[i + 1].clone());
                    i += 1;
                }
                i += 1;
            }
            let new_bytes = std::fs::read(&args[3]).unwrap_or_else(|e| {
                eprintln!("read new content {}: {e}", args[3]);
                std::process::exit(1);
            });
            let sup = Supervisor::open(data_dir()).unwrap_or_else(|e| {
                eprintln!("open runtime: {e}");
                std::process::exit(1);
            });
            let seq = sup.max_seq().unwrap_or(0);
            let w = SafeWriter::new(safewrite_dir()).unwrap_or_else(|e| {
                eprintln!("open safewrite state: {e}");
                std::process::exit(1);
            });
            let edit = pantheon_exec::safewrite::FileEdit {
                path: PathBuf::from(&args[2]),
                new_content: new_bytes,
                expected_hash: expect,
            };
            match w.apply_edits(vec![edit], seq) {
                Ok(r) => {
                    let rid = run_id.unwrap_or_else(pantheon_runtime::new_run_id);
                    let _ = sup.emit(Event::RunProgress {
                        run_id: rid,
                        detail: format!(
                            "safewrite apply ckpt={} files={}",
                            r.checkpoint_id,
                            r.files.len()
                        ),
                    });
                    println!("{}", serde_json::to_string_pretty(&r).unwrap());
                }
                Err(e) => {
                    eprintln!("apply: {e}");
                    std::process::exit(1);
                }
            }
        }
        "checkpoint" => {
            if args.len() < 3 {
                eprintln!("usage: pantheon checkpoint <path>... [--run ID]");
                std::process::exit(2);
            }
            let mut paths: Vec<PathBuf> = vec![];
            let mut i = 2;
            while i < args.len() {
                if args[i] == "--run" {
                    i += 2;
                    continue;
                }
                paths.push(PathBuf::from(&args[i]));
                i += 1;
            }
            let sup = Supervisor::open(data_dir()).unwrap_or_else(|e| {
                eprintln!("open runtime: {e}");
                std::process::exit(1);
            });
            let seq = sup.max_seq().unwrap_or(0);
            let w = SafeWriter::new(safewrite_dir()).unwrap_or_else(|e| {
                eprintln!("open safewrite state: {e}");
                std::process::exit(1);
            });
            match w.checkpoint(&paths, seq) {
                Ok(cp) => println!("{}", serde_json::to_string_pretty(&cp).unwrap()),
                Err(e) => {
                    eprintln!("checkpoint: {e}");
                    std::process::exit(1);
                }
            }
        }
        "rollback" => {
            let mut ckpt: Option<String> = None;
            let mut seq: Option<i64> = None;
            let mut i = 2;
            while i < args.len() {
                if args[i] == "--ckpt" && i + 1 < args.len() {
                    ckpt = Some(args[i + 1].clone());
                    i += 1;
                } else if args[i] == "--seq" && i + 1 < args.len() {
                    seq = args[i + 1].parse().ok();
                    i += 1;
                }
                i += 1;
            }
            let w = SafeWriter::new(safewrite_dir()).unwrap_or_else(|e| {
                eprintln!("open safewrite state: {e}");
                std::process::exit(1);
            });
            if let Some(id) = ckpt {
                match w.restore_checkpoint(&id) {
                    Ok(paths) => println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &serde_json::json!({"checkpoint": id, "restored": paths})
                        )
                        .unwrap()
                    ),
                    Err(e) => {
                        eprintln!("rollback: {e}");
                        std::process::exit(1);
                    }
                }
            } else if let Some(n) = seq {
                match w.rollback_to_seq(n) {
                    Ok((id, paths)) => println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &serde_json::json!({"checkpoint": id, "restored": paths})
                        )
                        .unwrap()
                    ),
                    Err(e) => {
                        eprintln!("rollback: {e}");
                        std::process::exit(1);
                    }
                }
            } else {
                eprintln!("usage: pantheon rollback (--ckpt ID | --seq N)");
                std::process::exit(2);
            }
        }
        _ => {
            eprint!("{}", usage());
            std::process::exit(2);
        }
    }
}
