//! pantheon terminal: thin dispatch over the runtime. No business logic here.
use pantheon_api::capability::Policy;
use pantheon_api::events::Event;
use pantheon_extensions::{doctor, ExtensionManager, Hook, RunnerConfig};
use pantheon_memory::{markdown, BackendSelection, LayerKind, MemoryStore, Proposal, Provenance};
use pantheon_runtime::{new_run_id, Supervisor};
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

/// `pantheon dashboard [--port 7171] [--host 127.0.0.1] [--open]`.
/// Starts the web control plane. The dashboard prints a tokenized URL at
/// startup; that token is the dashboard's password. Binding a
/// non-loopback address is allowed but warned about loudly at startup —
/// public exposure belongs behind a reverse proxy with real
/// authentication.
///
/// `--host` is the canonical bind-address flag (B-14); `--bind` is kept
/// as an alias so muscle memory and scripts keep working.
fn cmd_dashboard(args: &[String]) {
    let mut port: u16 = 7171;
    let mut bind = "127.0.0.1".to_string();
    let mut open = false;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                i += 1;
                port = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_else(|| die("--port needs a number"));
            }
            // Canonical name first; the historic alias still works.
            "--host" | "--bind" => {
                i += 1;
                bind = args
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| die("--host/--bind needs an address"));
            }
            "--open" => open = true,
            "--help" | "-h" => {
                println!(
                    "pantheon dashboard [--port 7171] [--host 127.0.0.1] [--open]\n\
                     \n\
                     Start the web control plane: runs, approvals, schedule,\n\
                     usage stats, memory, config, keys, logs, skills/MCP, and\n\
                     gateway — over a std-only HTTP server on localhost.\n\
                     \n\
                     --host is the canonical bind-address flag; --bind is an\n\
                     alias. Same flags as `pantheon serve`.\n\
                     \n\
                     Every /api/* request needs the per-instance token printed\n\
                     at startup. Keep it on 127.0.0.1; exposing it publicly\n\
                     requires a reverse proxy with real authentication."
                );
                return;
            }
            other => die(&format!("dashboard: unknown flag '{other}'")),
        }
        i += 1;
    }
    // Unified serve surface: the dashboard control plane and the AG-UI
    // routes share one listener and one token — one front door. This
    // command reuses `pantheon serve`'s builders; it just owns `--open`
    // and keeps the dashboard's historic default port.
    let dir = data_dir();
    let token = crate::agui::resolve_serve_token("pantheon dashboard");
    let bind_all = bind == "0.0.0.0" || bind == "::";
    let dash_mount = std::sync::Arc::new(pantheon_dashboard::DashboardMount::new(
        pantheon_dashboard::App {
            data_dir: dir.clone(),
            token: token.clone(),
            bind: bind.clone(),
            bind_all,
            on_approval: Some(crate::agui::approval_callback()),
            send_locks: Default::default(),
            turn_children: Default::default(),
            swarm: pantheon_dashboard::swarm::orchestrator_for(&dir),
        },
    ));
    // Single-token invariant: the gateway's auth context comes from the
    // dashboard mount's own `auth_ctx()`, so both mounts enforce the
    // same token.
    let auth = dash_mount.auth_ctx();
    let agui_cfg = crate::agui::build_agui_serve_parts(&dir, &bind, port, &token);
    let agui_mount = std::sync::Arc::new(pantheon_runtime::agui_serve::AguiMount { cfg: agui_cfg });
    let url = format!("http://{bind}:{port}/?token={token}");
    println!("pantheon dashboard on {url}");
    if bind_all {
        eprintln!(
            "WARNING: dashboard is bound to a non-loopback address. The token is the only \
             protection. Put a reverse proxy with real auth in front, or keep it on 127.0.0.1."
        );
    } else {
        eprintln!("keep this URL private: the token is the dashboard's password.");
    }
    if open {
        open_browser(&url);
    }
    let cfg = pantheon_gateway::http::ServerConfig {
        bind_addr: format!("{bind}:{port}"),
        auth,
        mounts: vec![dash_mount, agui_mount],
        label: "pantheon dashboard".to_string(),
    };
    if let Err(e) = pantheon_gateway::http::serve(cfg) {
        die(&format!("dashboard: {e}"));
    }
}

/// Best-effort `--open`: hand the URL to the OS browser; never fatal.
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let prog = "open";
    #[cfg(not(target_os = "macos"))]
    let prog = "xdg-open";
    if let Err(e) = std::process::Command::new(prog).arg(url).spawn() {
        eprintln!("dashboard: could not open browser ({prog}): {e}");
    }
}

/// Drain the run's FIFO queue after a turn settles: pop each queued
/// follow-up (oldest first) and run it as a new turn until the queue
/// is empty. Best-effort: a failed step is reported, not fatal.
pub(crate) fn drain_queued_turns(
    session: &pantheon_runtime::session::Session,
    run_id: &str,
    _channel: &str,
) {
    let ledger = match pantheon_storage::Ledger::open(&data_dir().join("ledger.db")) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("queue drain: open ledger: {e}");
            return;
        }
    };
    loop {
        let mut queued = match ledger.queued_messages(run_id) {
            Ok(q) => q,
            Err(e) => {
                eprintln!("queue drain: read queue: {e}");
                return;
            }
        };
        if queued.is_empty() {
            return;
        }
        let msg = queued.remove(0);
        // Pop-oldest: clear the queue, then re-append the remainder.
        if let Err(e) = ledger.set_queued_message(run_id, None) {
            eprintln!("queue drain: clear queue: {e}");
            return;
        }
        for rest in &queued {
            if let Err(e) = ledger.set_queued_message(run_id, Some(rest)) {
                eprintln!("queue drain: re-queue: {e}");
                return;
            }
        }
        let turn_id = pantheon_runtime::new_turn_id();
        match session.chat_turn(run_id, &turn_id, &msg) {
            Ok(_) => println!("run {run_id} continued (queued follow-up)"),
            Err(e) => {
                eprintln!("queue drain: turn failed: {e}");
                return;
            }
        }
    }
}
pub(crate) fn ext_dir() -> PathBuf {
    if let Ok(d) = std::env::var("PANTHEON_EXT_DIR") {
        return PathBuf::from(d);
    }
    data_dir().join("extensions")
}
fn load_backend_selection(data_dir: &Path) -> BackendSelection {
    pantheon_memory::load_selection(data_dir)
}
fn save_backend_selection(data_dir: &Path, sel: &BackendSelection) {
    if let Err(e) = pantheon_memory::save_selection(data_dir, sel) {
        eprintln!("memory backend: {e}");
    }
}
/// Print a value as pretty JSON, or a plain error if it cannot be
/// serialized. Serializing a struct that just round-tripped through serde is
/// not expected to fail, but a panic here would replace a report with a
/// backtrace, which is the worst possible output for whatever went wrong
/// upstream that the report was describing.
pub(crate) fn print_json<T: serde::Serialize>(label: &str, v: &T) {
    match serde_json::to_string_pretty(v) {
        Ok(s) => println!("{s}"),
        Err(e) => {
            eprintln!("{label}: could not serialize the report as JSON: {e}");
            std::process::exit(1);
        }
    }
}

/// `pantheon --help`. Grouped by what a user is trying to do, because the
/// flat verb list was 29 lines of equal weight with no way to tell the two
/// commands you need daily from the six you install once.
/// `pantheon --help`. Grouped by what a user is trying to do, because a
/// flat verb list gives the two commands you need daily the same weight as
/// the six you install once.
/// Run a real model turn and hand the answer to a delivery target.
///
/// `--deliver session` is the default and just prints. A channel name means
/// the reply is queued in the durable outbox for the running gateway to pick
/// up, so the user sees it where they asked for it instead of on a terminal
/// nobody is watching.
fn run_delivered_task(
    task_id: &Option<String>,
    say: &Option<String>,
    target: &str,
    verdict_tool: bool,
) {
    let text = match say {
        Some(s) => s.clone(),
        None => {
            eprintln!("run --deliver {target} needs --say \"text\" (or --say - / PANTHEON_SAY_STDIN=1 for stdin)");
            std::process::exit(2);
        }
    };
    if !matches!(target, "telegram" | "discord" | "session") {
        eprintln!("unknown delivery target '{target}'; use session, telegram, or discord");
        std::process::exit(2);
    }
    // Reuse an existing run when the caller named one, so a delivered task
    // continues its conversation instead of starting an orphan.
    let run_id = task_id.clone().unwrap_or_else(pantheon_runtime::new_run_id);

    use crate::config::build_model_policy;
    let file_cfg = crate::config::Config::load_or_report(&data_dir());
    let model_policy = build_model_policy(file_cfg.as_ref(), None, None);
    let allow_memory = file_cfg
        .as_ref()
        .map(|c| c.policy == Some(crate::config_schema::PolicyPreset::CoderMemory))
        .unwrap_or(false);
    let policy = if allow_memory {
        pantheon_api::capability::Policy::coder_with_memory()
    } else {
        pantheon_api::capability::Policy::coder()
    };
    let secrets = crate::config::chat_secrets(file_cfg.as_ref());
    let session =
        match pantheon_runtime::session::Session::new(data_dir(), policy, model_policy, secrets) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("open session: {e}");
                std::process::exit(1);
            }
        };
    if verdict_tool {
        session.set_verdict_tool(true);
    }
    match session.chat_turn(&run_id, "", &text) {
        Ok(outcome) => {
            let answer = outcome_text(&outcome);
            if target == "session" {
                println!("{answer}");
            } else if let Err(e) =
                crate::gateway::enqueue_outbound(&data_dir(), target, &answer, target)
            {
                eprintln!("queue for {target}: {e}");
                std::process::exit(1);
            } else {
                println!("queued for {target} — run {run_id}");
            }
            println!("{run_id}");
        }
        Err(e) if e.code == "RUN_PARKED" => {
            // A parked run is a real outcome, not a failure. The error names
            // the exact grant/deny command with the scope inlined, so it is
            // printed verbatim instead of being restated with a placeholder.
            println!("parked — run {run_id}");
            println!("{e}");
        }
        Err(e) => {
            eprintln!("run {run_id}: {e}");
            std::process::exit(1);
        }
    }
}

/// Best-effort extraction of the assistant text from a loop outcome. The
/// ledger has the authoritative transcript; this only shapes what gets
/// delivered, so a shape we do not recognize falls back to empty rather than
/// inventing text.
pub(crate) fn outcome_text(outcome: &pantheon_agent::LoopOutcome) -> String {
    use pantheon_agent::LoopOutcome;
    match outcome {
        LoopOutcome::Answered { text, .. } => text.clone(),
        _ => String::new(),
    }
}

fn usage() -> String {
    let mut s = String::new();
    s.push_str("pantheon - a durable agent runtime\n\n");
    s.push_str("USAGE\n");
    s.push_str("  pantheon                      open a session (the terminal interface)\n");
    s.push_str("  pantheon --help | --version\n\n");

    s.push_str("TALK TO IT\n");
    s.push_str("  run  --taskID <id> [--say TEXT] [--fail CODE] [--ext]\n");
    s.push_str("        [--deliver session|telegram|discord]\n");
    s.push_str("        run a task by id; delivery defaults to an in-session turn\n\n");

    s.push_str("INSPECT A RUN\n");
    s.push_str("  runs                         list runs and their status\n");
    s.push_str("  runs <run_id>                full event trace for one run\n");
    s.push_str("  logs [agent|errors|gateway]  read the log files (-n, -f, --level)\n");
    s.push_str("  audit <run_id> [OUT.jsonl]   sequence-validated JSONL trajectory\n");
    s.push_str("  stats [--week|--month|--from D|--to D] [--json]\n");
    s.push_str("        usage by model, project, session, day\n\n");

    s.push_str("SET UP\n");
    s.push_str("  setup                         wizard: API key, default model, policy\n");
    s.push_str("  init                          install the always-on gateway service\n");
    s.push_str("                                (chat surfaces + scheduled tasks), idempotently\n");
    s.push_str("  update [--check] [--version TAG]  replace this binary with the latest release\n");
    s.push_str("  model [--list] [--auxiliary KIND]        provider picker, keys -> .env\n");
    s.push_str("  provider <add|list|remove>   custom-endpoint registry\n");
    s.push_str("  config <set|get|edit|path>  read and write config.toml values\n");
    s.push_str("  providers                    list cataloged providers and models\n");
    s.push_str("  fallback <add|list|remove>   ordered provider/model fallback chain\n");
    s.push_str("  doctor [--ping] [<plugin_dir>]  system preflight (or per-plugin)\n");
    s.push_str(
        "  repair [--dry-run]              find and fix anything wrong with this install;\n",
    );
    s.push_str("                                --dry-run reports without changing anything.\n");
    s.push_str("                                (pantheon doctor is diagnose-only)\n");
    s.push_str("  reset [--config|--state|--everything] [--yes]\n");
    s.push_str("  backup [--list] [--restore DIR --yes]\n");
    s.push_str("        atomic SQLite snapshots into backups/<UTC-timestamp>/\n");
    s.push_str("  uninstall [--yes] [--include-secrets]\n");
    s.push_str("        remove config + data (keeps .env unless --include-secrets)\n\n");

    s.push_str("EXTEND\n");
    s.push_str("  skills list|import <name>|doctor        SKILL.md skills\n");
    s.push_str("  plugins list|install|enable|disable     capability plugins\n");
    s.push_str("  extensions                    list loaded extensions\n");
    s.push_str("  hook <name> [--session S] [--platform P]  fire a hook\n");
    s.push_str("  mcp list [--json]             MCP servers a migration declared\n");
    s.push_str("  migrate <detect|show|plan|apply|validate> <hermes|openclaw|omp> [path]\n");
    s.push_str("           [--kind K] [--json] [--yes] [--merge-providers]\n\n");

    s.push_str("MEMORY\n");
    s.push_str("  memory import|export|sync|recall|list|confirm|put|vault|backend\n");
    s.push_str("  reflect [--dry-run] [on|off|status|log|pending] [--approve ID] [--deny ID]\n");
    s.push_str("        ledger-native self-improvement: propose, eval-gate, approve, apply\n");
    s.push_str("  consolidate [--dry-run] [status]\n");
    s.push_str("        stage/weigh/promote repeated facts into long-term memory\n");
    s.push_str(
        "  nightly [--dry-run] [on|off|status|log|pending|escalations|report|replay-tasks]\n",
    );
    s.push_str("        [--approve ID] [--deny ID]  nightly pass: repair, memory, tasks\n\n");

    s.push_str("RUN UNATTENDED\n");
    s.push_str("  schedule <task> --30m | list|pause|resume|cancel|run <id>\n");
    s.push_str("  swarm status [<id>] | list   previously recorded swarms (spawn unsupported)\n");
    s.push_str("  gateway [run|start|stop|restart|status]\n");
    s.push_str("        always-on service: chat surfaces + scheduled tasks\n");
    s.push_str("  serve [--port N] [--host H]   AG-UI SSE + RPC server\n");
    s.push_str("  dashboard [--port 7171] [--host 127.0.0.1] [--open]\n");
    s.push_str("        web control plane: runs, approvals, schedule, config, keys,\n");
    s.push_str("        logs, skills/MCP, gateway. Per-instance token auth; stays on\n");
    s.push_str("        localhost unless you put a reverse proxy with real auth in front.\n\n");

    s.push_str("WORKFLOWS\n");
    s.push_str("  pipeline <sub>                durable 6-stage workflow with human gates\n\n");

    s.push_str("APPROVALS\n");
    s.push_str("  Approvals are answered in the session that raised them. The terminal\n");
    s.push_str("  interface shows a permission card (y/n). A run parked on approval can\n");
    s.push_str("  also be answered out of band:\n");
    s.push_str("    pantheon run --taskID <id> --grant <scope>   allow one exact call\n");
    s.push_str("    pantheon run --taskID <id> --deny  <scope>   refuse it\n");
    s
}

/// OMP RESERVED_TOP_LEVEL_WORDS guard: every top-level dispatch target,
/// kept in sync with the `match args[1]` arms in `main`.
const KNOWN_VERBS: &[&str] = &[
    "audit",
    "backup",
    "consolidate",
    "dashboard",
    "doctor",
    "extensions",
    "fallback",
    "gateway",
    "hook",
    "init",
    "logs",
    "mcp",
    "memory",
    "migrate",
    "model",
    "nightly",
    "pipeline",
    "plugins",
    "provider",
    "providers",
    "reflect",
    "repair",
    "reset",
    "run",
    "runs",
    "schedule",
    "serve",
    "setup",
    "skills",
    "stats",
    "swarm",
    "uninstall",
    "update",
];

/// Classification of argv[1] before it can become a prompt or session input.
#[derive(Debug, PartialEq, Eq)]
enum FirstArg {
    /// No argv[1] (bare `pantheon`): the terminal interface.
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

/// Guard for verbs that take a positional argument (`audit <run_id>`,
/// `runs [<run_id>]`): the hand-rolled dispatch used to treat `--*`
/// tokens as the positional value, so `doctor --ping` ran the plugin
/// doctor on a directory literally named "--ping" and `runs --frobnicate`
/// looked up a run literally named "--frobnicate". `--help`/`-h` prints
/// the verb help and exits 0; any other flag-looking token that is not
/// in `allowed_flags` is a usage error (exit 2).
fn reject_flag_positional(
    verb: &str,
    args: &[String],
    index: usize,
    allowed_flags: &[&str],
    help: fn(),
) {
    match args.get(index).map(String::as_str) {
        Some("--help") | Some("-h") => {
            help();
            std::process::exit(0);
        }
        Some(a) if a.starts_with('-') && !allowed_flags.contains(&a) => {
            eprintln!("{verb}: unexpected flag {a:?} in a positional slot");
            help();
            std::process::exit(2);
        }
        _ => {}
    }
}

fn audit_help() {
    eprintln!("usage: pantheon audit <run_id> [OUT.jsonl]");
    eprintln!("  sequence-validated JSONL trajectory for one run;");
    eprintln!("  writes OUT.jsonl when given, stdout otherwise");
}

fn runs_help() {
    eprintln!("usage: pantheon runs [--metrics] [<run_id>]");
    eprintln!("  no run id: list runs and their status");
    eprintln!("  <run_id>:  full event trace for one run");
    eprintln!("  --metrics: one-line counts folded from the ledger");
}

fn doctor_help() {
    eprintln!("usage: pantheon doctor [--ping] [--json|--human] [<plugin_dir>]");
    eprintln!("  no args:     system preflight (human-readable; --json for JSON)");
    eprintln!("  <plugin_dir>: verify one plugin directory");
    eprintln!("  --ping:      also probe the configured model provider's endpoint");
    eprintln!("  --json:      machine-readable JSON on stdout");
    eprintln!("  --human:     human-readable output (the default)");
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

fn load_mgr() -> ExtensionManager {
    let mut m = ExtensionManager::new(RunnerConfig::default());
    let d = ext_dir();
    if d.exists() {
        // Report a failed load. Swallowing it made a broken plugin directory
        // look identical to an empty one, so `pantheon extensions` cheerfully
        // reported "no extensions loaded" while the user's plugins were
        // sitting right there, broken.
        if let Err(e) = m.load_dir(&d) {
            eprintln!(
                "warning: could not load extensions from {}: {e}",
                d.display()
            );
        }
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
    eprintln!(
        "usage: pantheon memory <import|export|list|recall|put|confirm|sync|backend|vault> ..."
    );
    eprintln!("  import [FILE]       import MEMORY.md into native memory");
    eprintln!("  export [FILE]       export native agent memory to MEMORY.md");
    eprintln!("  sync [FILE]         reconcile MEMORY.md and the native store");
    eprintln!("  list                show every memory in the agent namespace");
    eprintln!("  recall QUERY        search native memory");
    eprintln!("  put KEY VALUE       store an agent memory (explicit write)");
    eprintln!("  backend list        show registered memory backends");
    eprintln!("  backend select NAME [k=v] choose the active backend");
    eprintln!("  backend scaffold NAME [http|stdio]  create a custom plugin manifest");
    eprintln!("  vault search QUERY  search notes/archives in Obsidian vault");
    eprintln!("  vault read PATH     read document from Obsidian vault");
    eprintln!("  vault list [CAT]    list files in Obsidian vault");
}

/// Resolve the `run --say` message (A-10): turn text must not travel via
/// argv, where it is visible in process listings (`ps`). Two stdin paths:
///
/// - `--say -`: the message is read from stdin instead of argv.
/// - `PANTHEON_SAY_STDIN=1` with no `--say` at all: same, for callers that
///   cannot pass the flag (e.g. the dashboard spawner).
///
/// A `--say` value other than `-` is returned unchanged. Empty stdin is an
/// error: silently running with no message would seed a confusing turn.
fn resolve_say_stdin(say: Option<String>) -> Option<String> {
    let from_stdin = match &say {
        Some(s) if s == "-" => true,
        None => matches!(
            std::env::var("PANTHEON_SAY_STDIN").as_deref(),
            Ok("1") | Ok("true")
        ),
        _ => false,
    };
    if !from_stdin {
        return say;
    }
    use std::io::Read;
    let mut buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
        eprintln!("run: could not read the message from stdin: {e}");
        std::process::exit(1);
    }
    let text = buf.trim_end_matches(['\r', '\n']).to_string();
    if text.is_empty() {
        eprintln!("run: stdin was empty; nothing to say");
        std::process::exit(2);
    }
    Some(text)
}

pub fn run() {
    let args: Vec<String> = std::env::args().collect();
    // The pantheon folder's own key store: `<data_dir>/.env` fills in any
    // process env var that is not already set (exports always win).
    // Custom endpoints from config are registered before any verb runs.
    crate::config::init_env_and_catalog(&data_dir());
    // Point the logger at the data dir before any verb runs, so a failure in
    // setup is already recorded by the time anyone goes looking. Level comes
    // from PANTHEON_LOG_LEVEL and defaults to INFO, because a DEBUG default
    // would fill the disk with records nobody reads.
    let level = std::env::var("PANTHEON_LOG_LEVEL")
        .ok()
        .and_then(|v| pantheon_api::logging::Level::parse(&v))
        .unwrap_or(pantheon_api::logging::Level::Info);
    pantheon_api::logging::init(&data_dir(), level);
    if args.len() < 2 {
        // The TUI is the terminal interface. There is no second interactive
        // surface and no line-based fallback: if this cannot open, it says so
        // and exits, rather than silently becoming a different program.
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            eprintln!("pantheon: the terminal interface needs a terminal on stdin and stdout.");
            eprintln!("pantheon: for a non-interactive turn use:");
            eprintln!("pantheon:   pantheon run --taskID <id> --say \"text\"");
            eprintln!("pantheon:   pantheon run --taskID <id> --say \"text\" --deliver telegram");
            std::process::exit(1);
        }
        crate::entry::run();
        return;
    }
    // `pantheon --resume [id]` (or `pantheon --resume` with no id) jumps
    // straight into a session on a specific run.
    if args.len() >= 2 && args[1] == "--resume" {
        let resume_id: Option<String> = args.get(2).cloned();
        crate::entry::run_with_resume(resume_id);
        return;
    }
    // `pantheon --profile <name>` (also `-p`, `--agent <name>`): open the
    // TUI as the named agent profile instead of the configured default.
    if args.len() >= 3 && matches!(args[1].as_str(), "--profile" | "-p" | "--agent") {
        crate::entry::run_with_profile(&args[2]);
        return;
    }
    match args[1].as_str() {
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
                    let policy = pantheon_api::capability::Policy::coder_with_memory();
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
                    // Recalls the same namespace `put` writes to, so the
                    // roundtrip is symmetric: `memory put k v` then
                    // `memory recall k` finds it. An earlier version of
                    // this verb read args[2] as the namespace, which broke
                    // that contract -- the query is args[2].
                    //
                    // `--ns <name>` (or `--ns '*'`) is the explicit way to
                    // read another agent's memory. Nothing defaults to it.
                    let (namespaces, query): (Vec<&str>, String) =
                        if args.get(2).map(String::as_str) == Some("--ns") {
                            let ns = args.get(3).map(String::as_str).unwrap_or("*");
                            (
                                vec![ns],
                                args.get(4..).map(|a| a.join(" ")).unwrap_or_default(),
                            )
                        } else {
                            (vec![namespace.as_str()], args[2..].join(" "))
                        };
                    let hits = pantheon_memory::recall_via(
                        backend.as_ref(),
                        &Policy::coder(),
                        &namespaces,
                        &[
                            LayerKind::TaskSession,
                            LayerKind::Project,
                            LayerKind::Agent,
                            LayerKind::Global,
                        ],
                        &query,
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
                // `recall` needs a query, so there was no way to see what
                // was stored. This lists the agent namespace directly.
                "list" => {
                    let backend = pantheon_memory::open_selected(&data_dir()).unwrap_or_else(|e| {
                        eprintln!("memory list: backend: {e}");
                        std::process::exit(1);
                    });
                    let rows = backend.list_agent(&namespace).unwrap_or_else(|e| {
                        eprintln!("memory list: {e}");
                        std::process::exit(1);
                    });
                    if rows.is_empty() {
                        println!("no memories in namespace {namespace}");
                    } else {
                        for (key, value) in &rows {
                            println!("{key} = {value}");
                        }
                        println!("({} in namespace {namespace})", rows.len());
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
                    // Confirm targets the Agent layer: that is where `memory
                    // put` writes, so the roundtrip is symmetric. The
                    // default policy marks memory.confirm as approval-gated,
                    // and typing this command IS the user vouching.
                    let record = pantheon_memory::confirm_via(
                        backend.as_ref(),
                        &Policy::coder_with_memory(),
                        pantheon_memory::LayerKind::Agent,
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
                                trust: pantheon_api::provenance::TrustTier::User,
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
                    let mut reg = pantheon_tools::tools::ToolRegistry::new();
                    pantheon_tools::vault_tools::register_vault_tools(
                        &mut reg,
                        pantheon_tools::vault_tools::VaultToolOptions { vault_dir },
                    );
                    // Same resolution order the agent loop uses, so
                    // `pantheon memory vault …` obeys the policy the user
                    // configured rather than running ungated.
                    let vault_cfg = crate::config::Config::load_or_report(&data_dir());
                    let policy = crate::config_schema::policy_for_config(&vault_cfg);
                    match args.get(3).map(|s| s.as_str()) {
                        Some("search") => {
                            if args.len() < 5 {
                                eprintln!("usage: pantheon memory vault search <QUERY>");
                                std::process::exit(2);
                            }
                            let query = args[4..].join(" ");
                            let json_arg = serde_json::json!({ "query": query }).to_string();
                            // Gated: the vault tools read files, and an interactive `pantheon
                            // memory vault …` must obey the same policy the agent loop enforces.
                            match reg.execute_gated(&policy, "vault_search", &json_arg) {
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
                            // Gated: the vault tools read files, and an interactive `pantheon
                            // memory vault …` must obey the same policy the agent loop enforces.
                            match reg.execute_gated(&policy, "vault_read", &json_arg) {
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
                            // Gated: the vault tools read files, and an interactive `pantheon
                            // memory vault …` must obey the same policy the agent loop enforces.
                            match reg.execute_gated(&policy, "vault_list", &json_arg) {
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
            crate::plugins_verb::cmd_plugins(&args);
        }
        "run" => {
            let mut id: Option<String> = None;
            let mut say: Option<String> = None;
            let mut tool: Option<String> = None;
            let mut fail: Option<String> = None;
            let mut with_ext = false;
            let mut platform = String::from("cli");
            // Reviewer-only tool for staged teams: the dashboard's swarm
            // worker passes this to reviewer child runs (`--deliver
            // session`), registering the `verdict` tool on the turn so the
            // orchestrator can read a structured verdict.
            let mut verdict_tool = false;
            // Approvals are normally answered inside the session that raised
            // them. These two flags are the out-of-band path: a run parked
            // from a script, a gateway message, or a second terminal. They
            // live on `run` rather than as top-level verbs so the verb list
            // stays short and the approval surface stays one place.
            let mut grant_scope: Option<String> = None;
            let mut deny_scope: Option<String> = None;
            // Where the run's output goes. `session` is the default and means
            // an ordinary in-process turn; the channel names hand the turn to
            // the gateway so the reply lands where the user is.
            let mut deliver: Option<String> = None;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    // `--taskID` is the documented name; `--id` stays as an
                    // alias because eval/run.py and muscle memory use it.
                    "--taskID" | "--id" => {
                        i += 1;
                        if i < args.len() {
                            id = Some(args[i].clone());
                        }
                    }
                    "--deliver" => {
                        i += 1;
                        if i < args.len() {
                            deliver = Some(args[i].clone());
                        }
                    }
                    "--grant" => {
                        i += 1;
                        if i < args.len() {
                            grant_scope = Some(args[i].clone());
                        }
                    }
                    "--deny" => {
                        i += 1;
                        if i < args.len() {
                            deny_scope = Some(args[i].clone());
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
                    "--verdict-tool" => {
                        verdict_tool = true;
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
            // A-10: resolve the message before anything consumes it, so
            // `--say -` / PANTHEON_SAY_STDIN keep turn text out of argv.
            let say = resolve_say_stdin(say);
            // Out-of-band approval: settle a parked scope, then continue the
            // run unless the caller opted out. The supervisor is scoped so
            // its SQLite write handle closes before the Session opens — two
            // live writers on one file make the Session's `BEGIN IMMEDIATE`
            // block on the busy timeout, which presents as a silent hang.
            if grant_scope.is_some() || deny_scope.is_some() {
                let run_id = id.clone().unwrap_or_else(|| {
                    eprintln!("run --grant/--deny needs --id <run_id>");
                    std::process::exit(2);
                });
                let is_grant = grant_scope.is_some();
                let scope = grant_scope.or(deny_scope).unwrap_or_default();
                {
                    let sup = Supervisor::open(data_dir()).unwrap_or_else(|e| {
                        eprintln!("open runtime: {e}");
                        std::process::exit(1);
                    });
                    let res = if is_grant {
                        sup.grant(&run_id, &scope)
                    } else {
                        sup.deny(&run_id, &scope)
                    };
                    if let Err(e) = res {
                        eprintln!("approval: {e}");
                        std::process::exit(1);
                    }
                }
                let verb = if is_grant { "granted" } else { "denied" };
                println!("{verb} {scope} for {run_id}");
                if !args.iter().any(|a| a == "--no-resume") {
                    crate::agui::resume_after_grant(&run_id);
                }
                return;
            }
            // A named delivery target means a real turn, not a synthetic
            // ledger write: the whole point is that the answer arrives where
            // the user is. Without one, `run` stays the synthetic writer the
            // eval harness depends on.
            if let Some(target) = deliver {
                if !matches!(target.as_str(), "session" | "telegram" | "discord") {
                    eprintln!(
                        "unknown --deliver target '{target}'; use session, telegram, or discord"
                    );
                    std::process::exit(2);
                }
                // `session` is the default target, and it is a real model
                // turn printed to this terminal — not the synthetic ledger
                // writer below. Gating it on `target != "session"` meant the
                // default path produced an event trace with no model call.
                return run_delivered_task(&id, &say, &target, verdict_tool);
            }
            // `run` writes ledger events directly. It does not call a model
            // and does not execute the named tool, so it is a synthetic-run
            // writer for ledger and recovery testing. It is not an
            // alternative to `chat`, and saying so up front is cheaper than
            // letting someone discover it from an empty transcript.
            if say.is_none() && tool.is_none() && fail.is_none() && !with_ext {
                eprintln!("usage: pantheon run [--id ID] [--say TEXT|--say -] [--tool NAME] [--fail CODE] [--ext] [--platform P]");
                eprintln!("       --say - reads the message from stdin (never via argv, so it");
                eprintln!("       never appears in process listings); PANTHEON_SAY_STDIN=1 does");
                eprintln!("       the same when --say is absent.");
                eprintln!();
                eprintln!("run writes synthetic ledger events only: it never calls a model and");
                eprintln!(
                    "never executes the tool named by --tool. Use `pantheon run --say TEXT` for a real"
                );
                eprintln!("turn, or `pantheon run --say TEXT` to seed a run for recovery testing.");
                std::process::exit(2);
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
                // Started and completed, so the run does not end with a
                // dangling call id. An unfinished call makes the next resume
                // treat the run as interrupted and re-drive it.
                let prov = pantheon_api::provenance::Provenance::system("cli");
                sup.emit(Event::ToolStarted {
                    run_id: run_id.clone(),
                    call_id: "cli".into(),
                    tool: t.clone(),
                    args: String::new(),
                    provenance: prov.clone(),
                })
                .unwrap_or_else(|e| die(&format!("ledger: {e}")));
                sup.emit(Event::ToolCompleted {
                    run_id: run_id.clone(),
                    call_id: "cli".into(),
                    tool: t,
                    provenance: prov,
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
            crate::schedule::cmd_schedule(&args, &data_dir());
        }
        "reflect" => {
            crate::reflect_cli::cmd_reflect(&args, &data_dir());
        }
        "consolidate" => {
            crate::consolidate_cli::cmd_consolidate(&args, &data_dir());
        }
        "nightly" => {
            crate::nightly_cli::cmd_nightly(&args, &data_dir());
        }
        "swarm" => {
            crate::swarm::cmd_swarm(&args, &data_dir());
        }
        "runs" => {
            // No run id: the operator wants to know what exists and what
            // state each run is in. The run id is positional: `--metrics`
            // is the one real flag; any other `--*` in its slot is a
            // usage error, not a run id.
            reject_flag_positional("runs", &args, 2, &["--metrics", "-m"], runs_help);
            if args.len() < 3 {
                let sup = Supervisor::open(data_dir()).unwrap_or_else(|e| {
                    eprintln!("open runtime: {e}");
                    std::process::exit(1);
                });
                match sup.ledger_list_runs(50) {
                    Ok(rows) if rows.is_empty() => println!("no runs yet"),
                    Ok(rows) => {
                        println!("{:<34} {:<18} TITLE", "RUN", "STATUS");
                        for (run_id, status, _ts, title, _project) in rows {
                            // Same status vocabulary as the TUI `/runs` view,
                            // so the two never disagree about a run.
                            println!("{run_id:<34} {status:<18} {}", title.unwrap_or_default());
                        }
                    }
                    Err(e) => {
                        eprintln!("logs: {e}");
                        std::process::exit(1);
                    }
                }
                return;
            }
            let sup = Supervisor::open(data_dir()).unwrap_or_else(|e| {
                eprintln!("open runtime: {e}");
                std::process::exit(1);
            });
            // `--metrics` answers "what actually happened in this run" in one
            // line: counts folded from the ledger, so it cannot disagree with
            // the trace above. Without it, spotting a run that has been
            // quietly dropping context means scrolling a hundred events.
            if args.iter().any(|a| a == "--metrics" || a == "-m") {
                match sup.run_metrics(&args[2]) {
                    Ok(m) => println!("{m}"),
                    Err(e) => {
                        eprintln!("runs: {e}");
                        std::process::exit(1);
                    }
                }
                return;
            }
            match sup.render_run_log(&args[2]) {
                Ok(t) => println!("{t}"),
                Err(e) => {
                    eprintln!("logs: {e}");
                    std::process::exit(1);
                }
            }
        }
        "logs" => {
            crate::logs::cmd_logs(&args);
        }

        "audit" => {
            // Export a run's ledger as a sequence-validated JSONL trajectory.
            // Usage: pantheon audit <run_id> [OUT]  (default: <run_id>.jsonl)
            // The run id is positional: a `--*` token there is a misplaced
            // flag, not a run id.
            reject_flag_positional("audit", &args, 2, &[], audit_help);
            if args.len() < 3 {
                audit_help();
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
            // Flags: --ping (reachability probe), --json / --human
            // (output format; human is the default). The plugin dir is the
            // first non-flag positional; an unknown `--*` is a usage
            // error, not a plugin directory named "--ping".
            let mut ping = false;
            let mut json = false;
            let mut plugin_dir: Option<&str> = None;
            for a in args.iter().skip(2) {
                match a.as_str() {
                    "--help" | "-h" => {
                        doctor_help();
                        std::process::exit(0);
                    }
                    "--ping" => ping = true,
                    "--json" => json = true,
                    "--human" => json = false,
                    s if s.starts_with('-') => {
                        eprintln!("doctor: unexpected flag {s:?}");
                        doctor_help();
                        std::process::exit(2);
                    }
                    s => {
                        if plugin_dir.is_none() {
                            plugin_dir = Some(s);
                        }
                    }
                }
            }
            match plugin_dir {
                Some(dir) => {
                    // Plugin-dir form: keep the original extension doctor.
                    let rep = doctor(std::path::Path::new(dir));
                    if json {
                        print_json("doctor", &rep);
                    } else {
                        print!("{}", crate::doctor::render_plugin_doctor_human(&rep));
                    }
                    if !rep.ok {
                        std::process::exit(1);
                    }
                }
                None => {
                    // System doctor: config, model, ledger, memory, plugins.
                    let rep = crate::doctor::run_system_doctor_opts(&data_dir(), ping);
                    if json {
                        print_json("doctor", &rep);
                    } else {
                        print!("{}", crate::doctor::render_human(&rep));
                    }
                    if !rep.ok {
                        std::process::exit(1);
                    }
                }
            }
        }
        "serve" => {
            crate::agui::cmd_serve(&args);
        }
        "dashboard" => {
            cmd_dashboard(&args);
        }
        "gateway" => {
            crate::gateway::cmd_gateway(&args);
        }
        "init" => {
            crate::init::cmd_init(&args);
        }
        "setup" => {
            crate::setup::cmd_setup(&args);
        }
        "update" => {
            crate::update::cmd_update(&args);
        }
        "backup" => {
            crate::backup::cmd_backup(&args);
        }
        "uninstall" => {
            crate::uninstall::cmd_uninstall(&args);
        }
        "model" => {
            crate::model::cmd_model(&args);
        }
        "provider" => {
            crate::provider::cmd_provider(&args);
        }
        "config" => {
            crate::config_verb::cmd_config(&args);
        }
        "reset" => {
            crate::reset::cmd_reset(&args);
        }
        "pipeline" => {
            crate::pipeline::cmd_pipeline(&args);
        }
        "providers" => {
            // List the providers a user can choose, plus the custom-provider
            // passthrough (any URL used with --provider URL). Development
            // endpoints are filtered out: this is the list someone reads to
            // decide what to configure, and a loopback address is not an
            // answer to that.
            println!("cataloged providers:");
            for p in pantheon_providers::catalog::selectable_providers() {
                let models: Vec<&str> = p.models.iter().map(|m| m.model.as_str()).collect();
                let mode = match p.api_mode {
                    pantheon_providers::catalog::ApiMode::OpenAi => "OpenAI",
                    pantheon_providers::catalog::ApiMode::Anthropic => "Anthropic",
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
            if pantheon_providers::catalog::all_providers().len()
                != pantheon_providers::catalog::selectable_providers().len()
            {
                println!(
                    "note: development-only endpoints are hidden here and remain usable by id"
                );
            }
            println!("example: pantheon run --taskID t1 --say \"hi\" --deliver session");
            println!("setup:   pantheon model  (picker → keys land in <data_dir>/.env)");
        }
        "skills" => {
            if args.len() < 3 {
                eprintln!("usage: pantheon skills <list|import|doctor> ...");
                std::process::exit(2);
            }
            match args[2].as_str() {
                "list" => crate::skills::cmd_skills_list(&args),
                "import" => crate::skills::cmd_skills_import(&args[2..]),
                "doctor" => crate::skills::cmd_skills_doctor(&args),
                _ => {
                    eprintln!("usage: pantheon skills <list|import|doctor> ...");
                    std::process::exit(2);
                }
            }
        }
        "fallback" => {
            crate::fallback::cmd_fallback(&args);
        }
        "stats" => {
            crate::stats::cmd_stats(&args, &data_dir());
        }
        "repair" => {
            crate::repair::cmd_repair(&args);
        }
        "migrate" => {
            crate::migrate::cmd_migrate(&args[2..]);
        }
        "mcp" => {
            crate::mcp::cmd_mcp(&args[2..]);
        }
        _ => {
            match classify_first_arg(&args) {
                // `--help` and `--version` are questions with real answers.
                // They were treated as bare flags, so both printed the whole
                // usage block to stderr and exited 2. `pantheon --version`
                // must print a version, and `--help` is the one place a user
                // looks first, so it goes to stdout with exit 0.
                FirstArg::Flag if args[1] == "--version" || args[1] == "-V" => {
                    println!("pantheon {}", env!("CARGO_PKG_VERSION"));
                }
                FirstArg::Flag if args[1] == "--help" || args[1] == "-h" => {
                    print!("{}", usage());
                }
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

// ── Startup splash ──────────────────────────────────────────────────────
// The Pantheon logo on the TUI welcome screen. The ASCII art is embedded at
// compile time from assets/ (relative to this file: src/ → ../../../assets).
// `session.rs` renders these lines on the welcome screen and after /new when
// the transcript is empty; nothing else in this module touches them.

// 60-column art, used on terminals >= 64 columns wide.
const SPLASH_LOGO_WIDE: &str = include_str!("../../../assets/logo-ascii-60.txt");
// 44-column art, used on narrower terminals.
const SPLASH_LOGO_NARROW: &str = include_str!("../../../assets/logo-ascii-44.txt");

/// Width threshold: terminals this wide (or wider) get the 60-column logo.
pub const SPLASH_WIDE_MIN_WIDTH: u16 = 64;

/// Pick the logo art for a terminal width. The `Option` inputs make the
/// graceful path testable: when an asset is missing (`None`), this returns
/// `None` and the caller skips the logo but keeps the wordmark.
pub fn choose_splash_logo<'a>(
    wide: Option<&'a str>,
    narrow: Option<&'a str>,
    width: u16,
) -> Option<&'a str> {
    if width >= SPLASH_WIDE_MIN_WIDTH {
        wide.or(narrow)
    } else {
        narrow.or(wide)
    }
}

/// The logo art for `width` columns, falling back across assets and to an
/// empty string when nothing is embedded. A missing asset never breaks
/// startup; the wordmark below still renders.
pub fn splash_logo(width: u16) -> &'static str {
    choose_splash_logo(Some(SPLASH_LOGO_WIDE), Some(SPLASH_LOGO_NARROW), width).unwrap_or("")
}

/// Center `line` in `width` columns (left-pad only; overlong lines pass
/// through and ratatui clips them).
fn splash_center(line: &str, width: usize) -> String {
    let len = line.chars().count();
    if len >= width {
        return line.to_string();
    }
    let pad = (width - len) / 2;
    format!("{}{line}", " ".repeat(pad))
}

/// Splash lines for the welcome screen: the logo art, then the wordmark
/// "PANTHEON" as styled normal text (bold, accent color) — deliberately not
/// part of the ASCII art. Missing/empty art is skipped; the wordmark always
/// renders.
pub fn splash_lines(
    width: u16,
    theme: &crate::session::theme::Theme,
) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    let width_usize = width.max(1) as usize;
    let mut lines: Vec<Line> = Vec::new();
    let art = splash_logo(width);
    if !art.trim().is_empty() {
        for raw in art.lines() {
            // Trailing spaces in the art are padding, not content; strip
            // them so a narrow terminal does not wrap on invisible width.
            lines.push(Line::from(splash_center(raw.trim_end(), width_usize)));
        }
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        splash_center("PANTHEON", width_usize),
        Style::default()
            .fg(theme.primary)
            .add_modifier(Modifier::BOLD),
    )));
    lines
}
