//! pantheon CLI: thin surface over the runtime. No business logic here.
use pantheon_core::capability::Policy;
use pantheon_core::events::Event;
use pantheon_exec::safewrite::{preview_edit, SafeWriter};
use pantheon_extensions::{doctor, ExtensionManager, Hook, RunnerConfig};
use pantheon_memory::{markdown, BackendSelection, LayerKind, MemoryStore, Proposal, Provenance};
use pantheon_runtime::{new_run_id, Supervisor};
use pantheon_secrets::SecretVault;
use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

pub fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("PANTHEON_DATA_DIR") {
        return PathBuf::from(d);
    }
    if let Ok(h) = std::env::var("HOME") {
        return PathBuf::from(h).join(".pantheon");
    }
    PathBuf::from(".pantheon-data")
}
/// Report a fatal error and exit. Every user-reachable failure path goes
/// through here so the process never panics on bad state.
fn die(msg: &str) -> ! {
    eprintln!("pantheon: {msg}");
    std::process::exit(1);
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
fn load_backend_selection(data_dir: &Path) -> BackendSelection {
    pantheon_memory::load_selection(data_dir)
}
fn save_backend_selection(data_dir: &Path, sel: &BackendSelection) {
    if let Err(e) = pantheon_memory::save_selection(data_dir, sel) {
        eprintln!("memory backend: {e}");
    }
}
fn usage() -> String {
    "pantheon <chat|run|schedule|swarm|explain|status|providers|extensions|hook|doctor|memory|plugins|preview|stage|apply|checkpoint|rollback|serve|stream|grant|deny|sign|setup|session|reset|gateway|skills|migrate|mcp|audit|pipeline> ...\n  chat [--id ID] [--model M] [--provider P] [--key K] \"message\"\n  run [--id ID] [--say TEXT] [--tool NAME] [--fail CODE] [--ext] [--platform P]\n  schedule <task> --30m [--agent NAME] | list|pause|resume|cancel|run <id>\n  swarm <N> \"<task>\" [roles...] [--delivery telegram]\n  explain <run_id>\n  status <run_id>\n  extensions  list loaded extensions\n  hook <name> [--session S] [--platform P]  fire a hook\n  doctor <plugin_dir>  loud preflight report\n  preview <path> <file-with-new-content>  read-only diff preview\n  stage <path> <file-with-new-content> [--expect HASH]  stage one edit\n  apply <path> <file-with-new-content> [--expect HASH] [--run ID]  checkpoint + atomic write\n  checkpoint <path>... [--run ID]  snapshot pre-images\n  rollback (--ckpt ID | --seq N)  restore a checkpoint\n  serve [--port N] [--host H]  AG-UI SSE + RPC server (cline-style interactive)\n  stream <run_id> [--thread T] [--after N]  print SSE frames for a run\n  grant <run_id> <scope>  approve a parked tool call\n  deny <run_id> [scope]  refuse a parked tool call\n  sign <task_id> [--mime M] [--ttl MS]  mint a signed generative-UI URL\n  channel <run_id> [--thread T]  replay frames through the transport seam\n  gateway                    run Discord/Telegram surfaces (env tokens)\n  setup                      interactive wizard: API key, default model, policy\n  model [--list] [--auxiliary KIND]  provider picker, stacked keys → .env, live model fetch\n  provider <add|list|remove>  custom-endpoint registry (keys → <data_dir>/.env)\n  session                    start the interactive REPL (default if no args)\n  reset [--config|--state|--everything] [--yes]  wipe data with confirmation\n  audit <run_id> [OUT.jsonl]  sequence-validated JSONL trajectory export\n  pipeline <run_id> [--approve STAGE|--deny STAGE]  six-stage orchestration gates\n  providers                  list cataloged providers and models\n  mcp list [--json]                     MCP servers declared by a migration, and whether they can register
  migrate <detect|show|plan|apply|validate> <hermes|openclaw|omp> [path] [--kind K] [--json] [--yes] [--merge-providers]  section 23 import pipeline
  skills list|import <name>|doctor  discover/import/check SKILL.md skills\n"
        .into()
}
/// OMP RESERVED_TOP_LEVEL_WORDS guard: every top-level dispatch target,
/// kept in sync with the `match args[1]` arms in `main`.
const KNOWN_VERBS: &[&str] = &[
    "apply",
    "audit",
    "channel",
    "chat",
    "checkpoint",
    "deny",
    "doctor",
    "explain",
    "extensions",
    "gateway",
    "grant",
    "hook",
    "mcp",
    "memory",
    "migrate",
    "model",
    "pipeline",
    "plugins",
    "preview",
    "provider",
    "providers",
    "reset",
    "rollback",
    "run",
    "schedule",
    "serve",
    "session",
    "setup",
    "sign",
    "skills",
    "stage",
    "status",
    "stream",
    "swarm",
];

/// Classification of argv[1] before it can become a prompt or session input.
#[derive(Debug, PartialEq, Eq)]
enum FirstArg {
    /// No argv[1] (bare `pantheon`): TUI/REPL path, behavior unchanged.
    NoArgs,
    /// A `-`/`--` flag (e.g. `--help`, `--resume`): never a verb.
    Flag,
    /// A known dispatch verb.
    Known,
    /// Anything else: rejected with exit code 2, never swallowed.
    Unknown(String),
}

fn is_flag_arg(s: &str) -> bool {
    s.starts_with('-')
}

fn is_known_verb(s: &str) -> bool {
    KNOWN_VERBS.contains(&s)
}

fn classify_first_arg(argv: &[String]) -> FirstArg {
    if argv.len() < 2 {
        return FirstArg::NoArgs;
    }
    let first = argv[1].as_str();
    if is_flag_arg(first) {
        return FirstArg::Flag;
    }
    if is_known_verb(first) {
        return FirstArg::Known;
    }
    FirstArg::Unknown(first.to_string())
}

/// Tiny Levenshtein distance over chars (verbs are ASCII; no new deps).
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(sub);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Closest known verbs to `unknown`, nearest first (up to `max_n`).
/// Only candidates within edit distance 3 are returned.
fn suggest_verbs(unknown: &str, max_n: usize) -> Vec<&'static str> {
    let mut scored: Vec<(usize, &'static str)> = KNOWN_VERBS
        .iter()
        .map(|&v| (levenshtein(unknown, v), v))
        .collect();
    scored.sort_by(|x, y| x.0.cmp(&y.0).then(x.1.cmp(y.1)));
    scored
        .into_iter()
        .filter(|(d, _)| *d <= 3)
        .take(max_n)
        .map(|(_, v)| v)
        .collect()
}

fn reject_unknown_verb(unknown: &str) -> ! {
    eprintln!("pantheon: unknown verb '{unknown}'");
    let suggestions = suggest_verbs(unknown, 3);
    if !suggestions.is_empty() {
        let list = suggestions
            .iter()
            .map(|s| format!("'{s}'"))
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!("did you mean {list}?");
    }
    // Full verb list (usage header names every verb).
    eprint!("{}", usage());
    std::process::exit(2);
}
mod agui_cli;
mod cli_args;
mod config_doc;
mod config_schema;
mod doctor_cli;
mod dotenv;
#[cfg(test)]
#[path = "dotenv_tests.rs"]
mod dotenv_tests;
mod gateway_cli;
mod mcp_cli;
mod migrate_cli;
mod model_cli;
#[cfg(test)]
#[path = "model_cli_tests.rs"]
mod model_cli_tests;
mod pipeline_cli;
mod provider_cli;
#[cfg(test)]
#[path = "provider_cli_tests.rs"]
mod provider_cli_tests;
mod reset_cli;
mod schedule_cli;
mod session_cli;
mod setup_cli;
mod setup_entry;
mod skills_cli;
mod swarm_cli;
mod tui;

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
    let out = mgr.fire(hook, session, platform, Default::default());
    write_seen(&mgr.seen_snapshot());
    out
}

/// Interactive model picker. Lists all cataloged provider/model pairs,
/// lets the user filter by typing, then selects by number. Returns
/// (provider_id, model_name) or None if cancelled.
fn pick_model() -> Option<(String, String)> {
    use std::io::{self, BufRead, Write};
    let stdin = io::stdin();
    let mut filter = String::new();

    loop {
        // Build the filtered list each iteration.
        let all: Vec<(String, String, String)> = pantheon_core::catalog::all_providers()
            .iter()
            .flat_map(|p| {
                p.models
                    .iter()
                    .map(move |m| {
                        (
                            p.id.clone(),
                            m.model.clone(),
                            format!("{} / {}", p.label, m.model),
                        )
                    })
                    .chain(std::iter::once((
                        p.id.clone(),
                        String::new(),
                        format!("{} / (any)", p.label),
                    )))
            })
            .filter(|(_, _, label)| {
                filter.is_empty() || label.to_lowercase().contains(&filter.to_lowercase())
            })
            .collect();

        // Terminal display: list + prompt.
        print!("\x1b[2J\x1b[H"); // clear screen
        println!("Pantheon model picker — type to filter, <Enter> on a number to select, /clear to reset, /q to cancel\n");
        if !filter.is_empty() {
            println!("filter: {}\n", filter);
        }
        if all.is_empty() {
            println!("(no matches)");
        }
        for (i, (_, _, label)) in all.iter().enumerate() {
            println!("  {:>3}  {}", i, label);
        }
        print!("\n> ");
        io::stdout().flush().ok()?;

        let mut line = String::new();
        if stdin.lock().read_line(&mut line).ok() == Some(0) {
            return None; // EOF
        }
        let line = line.trim();

        if line.is_empty() {
            continue;
        }
        if line == "/q" || line == "q" {
            return None;
        }
        if line == "/clear" || line == "c" {
            filter.clear();
            continue;
        }

        // Try to parse as a number (selection).
        if let Ok(n) = line.parse::<usize>() {
            if n < all.len() {
                let (pid, model, _) = &all[n];
                return Some((pid.clone(), model.clone()));
            }
            eprintln!("out of range");
            continue;
        }

        // Otherwise treat as a search filter.
        filter = line.to_string();
    }
}
fn memory_file() -> PathBuf {
    std::env::var_os("PANTHEON_MEMORY_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("MEMORY.md")
        })
}

fn open_memory() -> MemoryStore {
    MemoryStore::open(&data_dir().join("memory.db")).unwrap_or_else(|e| {
        eprintln!("open memory: {e}");
        std::process::exit(1);
    })
}

fn memory_help() {
    eprintln!("usage: pantheon memory <import|export|recall|put|confirm|sync|backend|vault> ...");
    eprintln!("  import [FILE]       import MEMORY.md into native memory");
    eprintln!("  export [FILE]       export native agent memory to MEMORY.md");
    eprintln!("  sync [FILE]         reconcile MEMORY.md and the native store");
    eprintln!("  recall QUERY        search native memory");
    eprintln!("  put KEY VALUE       store an agent memory (explicit write)");
    eprintln!("  backend list        show registered memory backends");
    eprintln!("  backend select NAME [k=v] choose the active backend");
    eprintln!("  backend scaffold NAME [http|stdio]  create a custom plugin manifest");
    eprintln!("  vault search QUERY  search notes/archives in Obsidian vault");
    eprintln!("  vault read PATH     read document from Obsidian vault");
    eprintln!("  vault list [CAT]    list files in Obsidian vault");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // The pantheon folder's own key store: `<data_dir>/.env` fills in any
    // process env var that is not already set (exports always win).
    // Custom endpoints from config are registered before any verb runs.
    config_doc::init_env_and_catalog(&data_dir());
    if args.len() < 2 {
        // Bare `pantheon` opens the agent cockpit TUI when a TTY is available.
        // Falls back to the text REPL if stdin is not a tty or TUI init fails.
        if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
            if let Err(e) = tui::run_tui_session() {
                eprintln!("pantheon: TUI session failed: {e}");
            }
        } else {
            session_cli::run_session();
        }
        return;
    }
    // `pantheon --resume [id]` (or `pantheon --resume` with no id)
    // jumps straight into the interactive session on a specific run.
    if args.len() >= 2 && args[1] == "--resume" {
        let resume_id: Option<String> = args.get(2).cloned();
        session_cli::run_session_with_resume(resume_id);
        return;
    }
    match args[1].as_str() {
        "chat" => {
            let mut id: Option<String> = None;
            let mut model: Option<String> = None;
            let mut provider: Option<String> = None;
            let mut key: Option<String> = None;
            let mut choose = false;
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
                    "--choose" => {
                        choose = true;
                    }
                    // A leading "-" is a flag, not the prompt. Swallowing it
                    // as the message sends the user's typo to the model as if
                    // they had typed it, which is worse than an error.
                    a if a.starts_with('-') => {
                        eprintln!("pantheon chat: unknown option {a}");
                        eprintln!("usage: pantheon chat [--id ID] [--model M] [--provider P] [--key K] [--choose] \"message\"");
                        eprintln!(
                            "  --id: continue an existing run instead of starting a new session"
                        );
                        std::process::exit(2);
                    }
                    _ if message.is_empty() => message = args[i].clone(),
                    _ => {}
                }
                i += 1;
            }
            // Interactive model picker: search/filter the catalog, then
            // select by number. Falls back to message arg if stdin not a tty.
            if choose {
                match pick_model() {
                    Some((p, m)) => {
                        provider = Some(p);
                        model = Some(m);
                        // If no message on the command line, read from --say or prompt.
                    }
                    None => std::process::exit(0),
                }
            }
            if message.is_empty() && choose {
                // After picking, read message from stdin if available.
                use std::io::Read;
                let mut buf = String::new();
                if std::io::stdin().read_to_string(&mut buf).is_ok() {
                    message = buf.trim().to_string();
                }
            }
            if message.is_empty() {
                eprintln!("usage: pantheon chat [--id ID] [--model M] [--provider P] [--choose] \"message\"");
                eprintln!("  --choose: interactive catalog picker (searchable)");
                std::process::exit(2);
            }
            // Config file (from setup) provides defaults; flags and env win.
            let file_cfg = config_doc::Config::load(&data_dir()).ok();
            let cfg_model = file_cfg
                .as_ref()
                .and_then(|c| c.model.clone())
                .map(|m| (Some(m.provider), Some(m.model)));
            // Model policy: default from flags > env > config > fallback.
            let default = pantheon_core::model::DefaultModel {
                provider: provider
                    .or(cfg_model
                        .as_ref()
                        .and_then(|(p, _)| p.clone())
                        .or_else(|| std::env::var("PANTHEON_PROVIDER").ok()))
                    .unwrap_or_else(|| "local".into()),
                model: model
                    .or(cfg_model
                        .as_ref()
                        .and_then(|(_, m)| m.clone())
                        .or_else(|| std::env::var("PANTHEON_MODEL").ok()))
                    .unwrap_or_else(|| "llama3.2".into()),
            };
            let mut chain = pantheon_core::model::FallbackChain::default();
            if let Some(fallbacks) = file_cfg
                .as_ref()
                .and_then(|c| c.model.as_ref())
                .map(|m| m.fallbacks.clone())
            {
                for f in fallbacks {
                    chain.fallbacks.push(pantheon_core::model::DefaultModel {
                        provider: f.provider,
                        model: f.model,
                    });
                }
            }
            let model_policy = pantheon_core::model::ModelPolicy {
                default: default.clone(),
                fallbacks: chain,
                auxiliaries: config_doc::auxiliaries(file_cfg.as_ref(), &default),
            };
            // Resolve API key through the secrets broker: the config-named
            // env var, then PANTHEON_API_KEY, then --key. Keys travel as
            // SecretValue in vaults, never as plain Strings in session
            // state. --key uses with_vault_front so an explicit flag beats
            // the config- and env-seeded vaults.
            let mut secrets = config_doc::chat_secrets(file_cfg.as_ref());
            if let Some(k) = &key {
                let mem = pantheon_secrets::MemoryVault::new();
                let _ = mem.set(
                    "PANTHEON_API_KEY",
                    pantheon_secrets::SecretValue::new(k.clone()),
                );
                secrets = secrets.with_vault_front(Box::new(mem));
            }
            let allow_memory = file_cfg
                .as_ref()
                .map(|c| c.policy == Some(config_schema::PolicyPreset::CoderMemory))
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
            let session = match pantheon_runtime::session::Session::new(
                data_dir(),
                policy,
                model_policy,
                secrets,
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
        "memory" => {
            if args.len() < 3 {
                memory_help();
                std::process::exit(2);
            }
            let store = open_memory();
            let namespace =
                std::env::var("PANTHEON_MEMORY_NAMESPACE").unwrap_or_else(|_| "nyx".into());
            match args[2].as_str() {
                "import" => {
                    let path = args.get(3).map(PathBuf::from).unwrap_or_else(memory_file);
                    let n = markdown::import_agent(
                        &store,
                        &Policy::coder_with_memory(),
                        &namespace,
                        &path,
                    )
                    .unwrap_or_else(|e| {
                        eprintln!("memory import: {e}");
                        std::process::exit(1);
                    });
                    println!("imported {n} memories from {}", path.display());
                }
                "export" => {
                    let path = args.get(3).map(PathBuf::from).unwrap_or_else(memory_file);
                    markdown::export_agent(&store, &namespace, &path).unwrap_or_else(|e| {
                        eprintln!("memory export: {e}");
                        std::process::exit(1);
                    });
                    println!("exported agent memory to {}", path.display());
                }
                "sync" => {
                    let path = args.get(3).map(PathBuf::from).unwrap_or_else(memory_file);
                    let last_hash_path = path.with_extension("md.sync-hash");
                    let last_hash = std::fs::read_to_string(&last_hash_path).ok();
                    let policy = pantheon_core::capability::Policy::coder_with_memory();
                    if markdown::detect_conflict(&store, &namespace, &path, last_hash.as_deref())
                        .unwrap_or(None)
                        .is_some()
                    {
                        eprintln!(
                            "memory sync: conflict between file and store; resolve manually before syncing"
                        );
                        std::process::exit(2);
                    }
                    let report =
                        markdown::sync(&store, &policy, &namespace, &path, last_hash.as_deref())
                            .unwrap_or_else(|e| {
                                eprintln!("memory sync: {e}");
                                std::process::exit(1);
                            });
                    if let Err(e) = std::fs::write(&last_hash_path, &report.file_hash) {
                        eprintln!("memory sync: writing hash file failed: {e}");
                    }
                    if report.imported {
                        println!("imported MEMORY.md changes into the store");
                    }
                    if report.exported {
                        println!("wrote MEMORY.md from the store");
                    }
                    if !report.imported && !report.exported {
                        println!("already in sync");
                    }
                }
                "recall" => {
                    if args.len() < 4 {
                        memory_help();
                        std::process::exit(2);
                    }
                    let backend = pantheon_memory::open_selected(&data_dir()).unwrap_or_else(|e| {
                        eprintln!("memory recall: backend: {e}");
                        std::process::exit(1);
                    });
                    let hits = pantheon_memory::recall_via(
                        backend.as_ref(),
                        &Policy::coder(),
                        &[
                            LayerKind::TaskSession,
                            LayerKind::Project,
                            LayerKind::Agent,
                            LayerKind::Global,
                        ],
                        &args[3..].join(" "),
                        20,
                    )
                    .unwrap_or_else(|e| {
                        eprintln!("memory recall: {e}");
                        std::process::exit(1);
                    });
                    for hit in hits {
                        println!(
                            "[{:?}] {} = {} ({})",
                            hit.record.layer,
                            hit.record.key,
                            hit.record.value,
                            hit.record.provenance.origin
                        );
                    }
                }
                "confirm" => {
                    if args.len() < 4 {
                        eprintln!("usage: pantheon memory confirm KEY");
                        std::process::exit(2);
                    }
                    let key = args[3].clone();
                    let backend = pantheon_memory::open_selected(&data_dir()).unwrap_or_else(|e| {
                        eprintln!("memory confirm: backend: {e}");
                        std::process::exit(1);
                    });
                    let record = pantheon_memory::confirm_via(
                        backend.as_ref(),
                        &Policy::coder_with_memory(),
                        &namespace,
                        &key,
                    )
                    .unwrap_or_else(|e| {
                        eprintln!("memory confirm: {e}");
                        std::process::exit(1);
                    });
                    println!("confirmed {} (memory tier)", record.key);
                }
                "put" => {
                    if args.len() < 5 {
                        memory_help();
                        std::process::exit(2);
                    }
                    let key = args[3].clone();
                    let value = args[4..].join(" ");
                    let backend = pantheon_memory::open_selected(&data_dir()).unwrap_or_else(|e| {
                        eprintln!("memory put: backend: {e}");
                        std::process::exit(1);
                    });
                    let record = pantheon_memory::write_via(
                        backend.as_ref(),
                        &Policy::coder_with_memory(),
                        Proposal {
                            layer: LayerKind::Agent,
                            namespace,
                            key,
                            value,
                            provenance: Provenance {
                                source: "cli".into(),
                                origin: "user".into(),
                                trust: pantheon_core::provenance::TrustTier::User,
                                recorded_at_ms: 0,
                            },
                        },
                        4096,
                    )
                    .unwrap_or_else(|e| {
                        eprintln!("memory put: {e}");
                        std::process::exit(1);
                    });
                    println!("stored {}", record.key);
                }
                "vault" => {
                    let vault_dir = std::env::var("PANTHEON_VAULT_DIR")
                        .map(PathBuf::from)
                        .unwrap_or_else(|_| {
                            let home =
                                std::env::var("HOME").unwrap_or_else(|_| "/home/ubuntu".into());
                            PathBuf::from(home).join("vault")
                        });
                    let mut reg = pantheon_exec::tools::ToolRegistry::new();
                    pantheon_exec::vault_tools::register_vault_tools(
                        &mut reg,
                        pantheon_exec::vault_tools::VaultToolOptions { vault_dir },
                    );
                    match args.get(3).map(|s| s.as_str()) {
                        Some("search") => {
                            if args.len() < 5 {
                                eprintln!("usage: pantheon memory vault search <QUERY>");
                                std::process::exit(2);
                            }
                            let query = args[4..].join(" ");
                            let json_arg = serde_json::json!({ "query": query }).to_string();
                            match reg.execute("vault_search", &json_arg) {
                                Ok(res) => println!("{res}"),
                                Err(e) => {
                                    eprintln!("vault search: {e}");
                                    std::process::exit(1);
                                }
                            }
                        }
                        Some("read") => {
                            if args.len() < 5 {
                                eprintln!("usage: pantheon memory vault read <PATH>");
                                std::process::exit(2);
                            }
                            let p = &args[4];
                            let json_arg = serde_json::json!({ "path": p }).to_string();
                            match reg.execute("vault_read", &json_arg) {
                                Ok(res) => println!("{res}"),
                                Err(e) => {
                                    eprintln!("vault read: {e}");
                                    std::process::exit(1);
                                }
                            }
                        }
                        Some("list") => {
                            let cat = args.get(4).map(|s| s.as_str());
                            let json_arg = serde_json::json!({ "category": cat }).to_string();
                            match reg.execute("vault_list", &json_arg) {
                                Ok(res) => println!("{res}"),
                                Err(e) => {
                                    eprintln!("vault list: {e}");
                                    std::process::exit(1);
                                }
                            }
                        }
                        _ => {
                            eprintln!("usage: pantheon memory vault <search|read|list> ...");
                            std::process::exit(2);
                        }
                    }
                }
                "backend" => {
                    let dd = data_dir();
                    let registry = pantheon_memory::BackendRegistry::with_plugins(&dd);
                    match args.get(3).map(|s| s.as_str()) {
                        Some("list") => {
                            let sel = load_backend_selection(&dd);
                            for b in registry.list() {
                                let mark = if sel.name == b.name { " *" } else { "" };
                                println!("{}{}\t{}", b.name, mark, b.label);
                            }
                        }
                        Some("select") => {
                            if args.len() < 5 {
                                eprintln!("memory backend select <NAME>");
                                eprintln!(
                                    "registered: {}",
                                    registry
                                        .list()
                                        .iter()
                                        .map(|b| b.name.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                );
                                std::process::exit(2);
                            }
                            let name = &args[4];
                            if !registry.contains(name) {
                                eprintln!("memory backend select: unknown backend '{name}'");
                                std::process::exit(1);
                            }
                            // Extra args are `k=v` options persisted with
                            // the selection (e.g. url=http://127.0.0.1:8016/v1).
                            let mut options = HashMap::new();
                            for kv in &args[5..] {
                                match kv.split_once('=') {
                                    Some((k, v)) if !k.is_empty() => {
                                        options.insert(k.to_string(), v.to_string());
                                    }
                                    _ => {
                                        eprintln!("memory backend select: expected k=v option, got '{kv}'");
                                        std::process::exit(2);
                                    }
                                }
                            }
                            let sel = BackendSelection {
                                name: name.clone(),
                                options,
                            };
                            save_backend_selection(&dd, &sel);
                            println!("active backend: {}", name);
                            println!(
                                "selection saved: {}",
                                dd.join("memory-backend.toml").display()
                            );
                        }
                        Some("scaffold") => {
                            let name = match args.get(4) {
                                Some(n) if !n.is_empty() => n.clone(),
                                _ => {
                                    eprintln!(
                                        "usage: pantheon memory backend scaffold NAME [http|stdio]"
                                    );
                                    std::process::exit(2);
                                }
                            };
                            let kind = args.get(5).map(|s| s.as_str()).unwrap_or("http");
                            if kind != "http" && kind != "stdio" {
                                eprintln!("memory backend scaffold: kind must be http or stdio");
                                std::process::exit(2);
                            }
                            let dir = dd.join("memory-plugins");
                            if let Err(e) = std::fs::create_dir_all(&dir) {
                                eprintln!("memory backend scaffold: {e}");
                                std::process::exit(1);
                            }
                            let path = dir.join(format!("{name}.toml"));
                            let template = if kind == "http" {
                                format!(
                                    r#"# Custom memory plugin: edit url/key, then select with
#   pantheon memory backend select {name}
name = "{name}"
label = "{name} memory service"
kind = "http"
url = "http://127.0.0.1:9000"
# key = "token"
# prefix = "/v1/memory"
"#
                                )
                            } else {
                                format!(
                                    r#"# Custom memory plugin (subprocess bridge). The command must
# read ONE JSON request line and write ONE JSON response line.
#   requests:  {{"op":"recall"|"write"|"list_agent"|"get"|"forget"|"confirm", ...}}
#   responses: {{"ok":true,...}} or {{"ok":false,"code":"...","cause":"..."}}
name = "{name}"
label = "{name} bridge"
kind = "stdio"
command = "python3"
args = ["/absolute/path/to/{name}_bridge.py"]
timeout_ms = 5000
"#
                                )
                            };
                            if let Err(e) = std::fs::write(&path, template) {
                                eprintln!("memory backend scaffold: {e}");
                                std::process::exit(1);
                            }
                            println!("scaffolded {}", path.display());
                            println!("edit it, then: pantheon memory backend select {name}");
                        }
                        _ => {
                            memory_help();
                            std::process::exit(2);
                        }
                    }
                }
                _ => {
                    memory_help();
                    std::process::exit(2);
                }
            }
        }
        "plugins" => {
            let dd = data_dir();
            let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            match args.get(2).map(|s| s.as_str()) {
                Some("list") | None => {
                    let found = pantheon_exec::plugins::discover_plugins(&dd, &project_root);
                    if found.is_empty() {
                        println!("no plugins installed");
                    }
                    for p in &found {
                        let src = match p.location {
                            pantheon_exec::plugins::PluginLocation::User => "user",
                            pantheon_exec::plugins::PluginLocation::Project => "project",
                        };
                        let status = if p.manifest.enabled { "on" } else { "off" };
                        println!(
                            "{:30} {}\t{} ({} tools)",
                            p.manifest.name,
                            status,
                            src,
                            p.manifest.capabilities.len(),
                        );
                    }
                }
                Some("install") => {
                    if args.len() < 4 {
                        eprintln!("usage: pantheon plugins install <name>");
                        eprintln!(
                            "catalog plugins: {}",
                            pantheon_exec::plugins::catalog_names().join(", ")
                        );
                        std::process::exit(2);
                    }
                    let name = &args[3];
                    pantheon_exec::plugins::install_catalog(name, &dd, &project_root)
                        .unwrap_or_else(|e| {
                            eprintln!("plugins install: {e}");
                            std::process::exit(1);
                        });
                    println!("installed {}", name);
                }
                Some(cmd @ ("enable" | "disable")) => {
                    if args.len() < 4 {
                        eprintln!("usage: pantheon plugins {cmd} <name>");
                        std::process::exit(2);
                    }
                    let name = &args[3];
                    // Find the plugin in either scope; enable/disable only
                    // touches the manifest in place.
                    let found = pantheon_exec::plugins::discover_plugins(&dd, &project_root);
                    let plugin = found
                        .iter()
                        .find(|p| p.manifest.name == *name)
                        .unwrap_or_else(|| {
                            eprintln!("plugins {cmd}: no plugin named '{name}'");
                            std::process::exit(1);
                        });
                    pantheon_exec::plugins::set_enabled(plugin, cmd == "enable").unwrap_or_else(
                        |e| {
                            eprintln!("plugins {cmd}: {e}");
                            std::process::exit(1);
                        },
                    );
                    println!(
                        "{} {}",
                        if cmd == "enable" {
                            "enabled"
                        } else {
                            "disabled"
                        },
                        name
                    );
                }
                _ => {
                    eprintln!("usage: pantheon plugins <list|install|enable|disable>");
                    std::process::exit(2);
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
                let fired = mgr.fire(Hook::PreLlmCall, &run_id, &platform, Default::default());
                write_seen(&mgr.seen_snapshot());
                if let Some(ctx) = fired {
                    sup.emit(Event::RunProgress {
                        run_id: run_id.clone(),
                        detail: format!("ext pre_llm_call injected {} chars", ctx.len()),
                    })
                    .unwrap_or_else(|e| die(&format!("ledger: {e}")));
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
                    provenance: pantheon_core::provenance::Provenance::system("cli"),
                })
                .unwrap_or_else(|e| die(&format!("ledger: {e}")));
            }
            if let Some(s) = say {
                sup.emit(Event::RunProgress {
                    run_id: run_id.clone(),
                    detail: s,
                })
                .unwrap_or_else(|e| die(&format!("ledger: {e}")));
            }
            if let Some(code) = fail {
                sup.fail(&run_id, &code)
                    .unwrap_or_else(|e| die(&format!("ledger: {e}")));
            } else {
                sup.complete(&run_id)
                    .unwrap_or_else(|e| die(&format!("ledger: {e}")));
            }
            println!("{run_id}");
        }
        "schedule" => {
            schedule_cli::cmd_schedule(&args, &data_dir());
        }
        "swarm" => {
            swarm_cli::cmd_swarm(&args, &data_dir());
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
        "audit" => {
            // Export a run's ledger as a sequence-validated JSONL trajectory.
            // Usage: pantheon audit <run_id> [OUT]  (default: <run_id>.jsonl)
            if args.len() < 3 {
                eprintln!("usage: pantheon audit <run_id> [OUT.jsonl]");
                std::process::exit(2);
            }
            let sup = Supervisor::open(data_dir()).unwrap_or_else(|e| {
                eprintln!("open runtime: {e}");
                std::process::exit(1);
            });
            let entries = sup.replay(&args[2]).unwrap_or_else(|e| {
                eprintln!("audit: {e}");
                std::process::exit(1);
            });
            let out = args
                .get(3)
                .cloned()
                .unwrap_or_else(|| format!("{}.jsonl", args[2]));
            let n = pantheon_storage::export_jsonl(&entries, std::path::Path::new(&out))
                .unwrap_or_else(|e| {
                    eprintln!("audit: {e}");
                    std::process::exit(1);
                });
            println!("wrote {n} events to {out}");
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
            let names = mgr.names();
            if names.is_empty() {
                // Silence reads as "the command did nothing". Say what
                // happened and where to put an extension.
                println!("no extensions loaded");
                println!("drop a plugin.yaml in {}", ext_dir().display());
            } else {
                for n in &names {
                    println!("{n}");
                }
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
            if args.len() >= 3 {
                // Plugin-dir form: keep the original extension doctor.
                let rep = doctor(std::path::Path::new(&args[2]));
                println!("{}", serde_json::to_string_pretty(&rep).unwrap());
                if !rep.ok {
                    std::process::exit(1);
                }
            } else {
                // System doctor: config, model, ledger, memory, plugins.
                let rep = doctor_cli::run_system_doctor(&data_dir());
                println!("{}", serde_json::to_string_pretty(&rep).unwrap());
                if !rep.ok {
                    std::process::exit(1);
                }
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
        "serve" => {
            agui_cli::cmd_serve(&args);
        }
        "stream" => {
            agui_cli::cmd_stream(&args);
        }
        "grant" => {
            agui_cli::cmd_grant(&args);
        }
        "deny" => {
            agui_cli::cmd_deny(&args);
        }
        "sign" => {
            agui_cli::cmd_sign(&args);
        }
        "channel" => {
            agui_cli::cmd_channel(&args);
        }
        "gateway" => {
            gateway_cli::cmd_gateway(&args);
        }
        "setup" => {
            setup_entry::cmd_setup(&args);
        }
        "model" => {
            model_cli::cmd_model(&args);
        }
        "provider" => {
            provider_cli::cmd_provider(&args);
        }
        "session" => {
            session_cli::run_session();
        }
        "reset" => {
            reset_cli::cmd_reset(&args);
        }
        "pipeline" => {
            pipeline_cli::cmd_pipeline(&args);
        }
        "providers" => {
            // List cataloged providers and their models, plus the
            // custom-provider passthrough (any URL used with --provider URL).
            println!("cataloged providers:");
            for p in pantheon_core::catalog::all_providers() {
                let models: Vec<&str> = p.models.iter().map(|m| m.model.as_str()).collect();
                let mode = match p.api_mode {
                    pantheon_core::catalog::ApiMode::OpenAi => "OpenAI",
                    pantheon_core::catalog::ApiMode::Anthropic => "Anthropic",
                };
                let tag = if p.prominent { "*" } else { " " };
                let label = if p.models.is_empty() {
                    format!("({} only)", p.label)
                } else {
                    p.label.clone()
                };
                println!("  {tag} {} ({}): {} -- {}", label, p.id, mode, p.base_url);
                println!("    models: {}", models.join(", "));
                if !p.tag.is_empty() {
                    println!("    tag: {}", p.tag);
                }
            }
            println!();
            println!("custom: --provider <base-url> uses that URL directly (OpenAI shape)");
            println!(
                "example: pantheon chat --provider http://127.0.0.1:8015/v1 --model chat \"hi\""
            );
            println!("setup:   pantheon model  (picker → keys land in <data_dir>/.env)");
        }
        "skills" => {
            if args.len() < 3 {
                eprintln!("usage: pantheon skills <list|import|doctor> ...");
                std::process::exit(2);
            }
            match args[2].as_str() {
                "list" => skills_cli::cmd_skills_list(&args),
                "import" => skills_cli::cmd_skills_import(&args[2..]),
                "doctor" => skills_cli::cmd_skills_doctor(&args),
                _ => {
                    eprintln!("usage: pantheon skills <list|import|doctor> ...");
                    std::process::exit(2);
                }
            }
        }
        "migrate" => {
            migrate_cli::cmd_migrate(&args[2..]);
        }
        "mcp" => {
            mcp_cli::cmd_mcp(&args[2..]);
        }
        _ => {
            match classify_first_arg(&args) {
                FirstArg::Flag => {
                    eprint!("{}", usage());
                    std::process::exit(2);
                }
                FirstArg::Unknown(verb) => reject_unknown_verb(&verb),
                // Known/NoArgs are unreachable here (this is the
                // catch-all arm with len >= 2), handled defensively.
                FirstArg::Known | FirstArg::NoArgs => {
                    eprint!("{}", usage());
                    std::process::exit(2);
                }
            }
        }
    }
}

#[cfg(test)]
mod verb_guard_tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn exact_match_passes() {
        for v in KNOWN_VERBS {
            assert!(is_known_verb(v), "known verb '{v}' must pass");
            assert_eq!(classify_first_arg(&argv(&["pantheon", v])), FirstArg::Known);
        }
        assert!(!is_known_verb("statsu"));
        assert!(!is_known_verb(""));
    }

    #[test]
    fn typo_suggests_status() {
        for typo in ["stats", "statsu"] {
            let got = suggest_verbs(typo, 3);
            assert_eq!(got.first(), Some(&"status"), "typo '{typo}' -> {got:?}");
        }
    }

    #[test]
    fn empty_argv_is_noargs() {
        assert_eq!(classify_first_arg(&[]), FirstArg::NoArgs);
        assert_eq!(classify_first_arg(&argv(&["pantheon"])), FirstArg::NoArgs);
    }

    #[test]
    fn flags_are_not_verbs() {
        for f in ["--help", "-h", "--resume", "--version"] {
            assert!(is_flag_arg(f), "'{f}' must classify as flag");
            assert!(!is_known_verb(f));
            assert_eq!(classify_first_arg(&argv(&["pantheon", f])), FirstArg::Flag);
            assert!(
                !suggest_verbs(f, 3).contains(&"--help"),
                "flags must never be suggested as verbs"
            );
        }
    }

    #[test]
    fn unknown_is_rejected_not_swallowed() {
        assert_eq!(
            classify_first_arg(&argv(&["pantheon", "statsu"])),
            FirstArg::Unknown("statsu".into())
        );
        // Gibberish yields no misleading suggestion, still Unknown.
        assert!(suggest_verbs("zzzqqqx", 3).is_empty());
        assert_eq!(
            classify_first_arg(&argv(&["pantheon", "zzzqqqx"])),
            FirstArg::Unknown("zzzqqqx".into())
        );
    }

    #[test]
    fn verb_list_covers_dispatch() {
        // Every dispatch arm in main must be known: the 34-verb surface.
        // (An earlier comment said 28 + extras; the list below is the
        // whole truth and the test fails if a new arm is added without
        // registering it here and in KNOWN_VERBS.)
        for v in [
            "chat",
            "run",
            "explain",
            "status",
            "audit",
            "grant",
            "deny",
            "memory",
            "plugins",
            "extensions",
            "hook",
            "doctor",
            "preview",
            "stage",
            "apply",
            "checkpoint",
            "rollback",
            "serve",
            "stream",
            "sign",
            "channel",
            "gateway",
            "setup",
            "reset",
            "pipeline",
            "providers",
            "skills",
            "migrate",
            "schedule",
            "swarm",
            "model",
            "provider",
            "session",
            "mcp",
        ] {
            assert!(
                is_known_verb(v),
                "dispatch verb '{v}' missing from KNOWN_VERBS"
            );
        }
    }

    #[test]
    fn usage_mentions_every_known_verb() {
        // usage() is the operator's map of the surface; a verb missing
        // from it is discoverable only by source-diving.
        let u = usage();
        for v in KNOWN_VERBS {
            assert!(u.contains(v), "usage() omits verb '{v}'");
        }
    }
}
