//! Interactive REPL: bare `pantheon` opens a session where every input is
//! a turn on the current run and `/commands` manage the session.
//!
//! Design:
//! - One run id stays in scope. Each input calls `Session::chat`, which
//!   now reopens terminal runs, so a conversation is one ledger run with
//!   full history (and the approval/recovery machinery applies as-is).
//! - `/new` starts a fresh run. `/runs` + `/resume` switch runs. `/exit`
//!   leaves; the run stays resumable next time because the ledger keeps
//!   everything.
//! - Slash commands are session-local, never sent to the model.
use crate::{config_doc, config_schema};
use pantheon_runtime::session::Session;
use std::io::{BufRead, Write};

const HELP: &str = "commands:
  /help              this text
  /new               start a fresh conversation (new run id)
  /runs              list recent conversations
  /resume [ID|n]     switch to a conversation by id or /runs number
  /history           interactive searchable history list
  /status            show the current run id and status
  /name [TITLE]      show this conversation's title, or rename it
  /memory <query>    search memory
  /remember <key> <text>
                     store a memory record (user-authored, user trust)
  /policy            show the active policy preset
  /model [P M]       show or switch the default model (next /new)
  /exit              leave (the conversation stays resumable)
anything else is sent to the model as a message";

/// REPL state. Kept in one struct so /commands read naturally.
struct Repl {
    session: Session,
    run_id: String,
    model: Option<(String, String)>, // (provider, model) override for /new
    namespace: String,
    /// Name of the configured preset, e.g. "coder". `Policy` is a resolved
    /// capability set with no back-pointer, so /policy could only ever dump
    /// capabilities. Kept beside it so the command can answer the question
    /// the user actually asked.
    policy_preset: String,
}

impl Repl {
    fn print_runs(&self, limit: usize) {
        match self.session.supervisor.ledger_list_runs(limit) {
            Ok(runs) => {
                // The current run has no ledger rows until its first message,
                // so a fresh session reported "(no runs yet)" while the user
                // was plainly sitting in one. Show it as unsaved.
                let listed: std::collections::HashSet<&str> =
                    runs.iter().map(|(id, ..)| id.as_str()).collect();
                let current_missing =
                    !self.run_id.is_empty() && !listed.contains(self.run_id.as_str());
                if runs.is_empty() && !current_missing {
                    println!("(no runs yet)");
                    return;
                }
                if current_missing {
                    println!("{:*>3}  {:<9}  {:<18} {}", "", "unsaved", "-", self.run_id);
                }
                for (i, (id, status, created_ms, title)) in runs.iter().enumerate() {
                    let label = title.as_deref().unwrap_or("");
                    println!(
                        "{:>3}  {}  {:<18} {}  {}",
                        i,
                        status,
                        id,
                        fmt_created(*created_ms),
                        label
                    );
                }
            }
            Err(e) => eprintln!("list runs: {e}"),
        }
    }
}

/// Interactive searchable run picker for /history. Typing filters,
/// <Enter> on a number selects, /q cancels. Mirrors `pick_model`'s
/// filter-then-select pattern. `runs`: (id, status, created_ms).
pub fn pick_run(runs: &[(String, String, i64, Option<String>)]) -> Option<String> {
    use std::io::{self, BufRead, Write};
    let stdin = io::stdin();
    let mut filter = String::new();

    loop {
        let all: Vec<&(String, String, i64, Option<String>)> = runs
            .iter()
            .filter(|(id, status, _, title)| {
                filter.is_empty()
                    || id.to_lowercase().contains(&filter.to_lowercase())
                    || status.to_lowercase().contains(&filter.to_lowercase())
                    || title
                        .as_deref()
                        .map(|t| t.to_lowercase().contains(&filter.to_lowercase()))
                        .unwrap_or(false)
            })
            .collect();

        print!("\x1b[2J\x1b[H");
        println!("Pantheon history — type to filter, <Enter> on a number to resume, /clear to reset, /q to cancel\n");
        if !filter.is_empty() {
            println!("filter: {}\n", filter);
        }
        if all.is_empty() {
            println!("(no matches)");
        } else {
            println!("{:>3}  {:<18} {:<10} AGE", "#", "RUN ID", "STATUS");
            for (i, (id, status, created_ms, title)) in all.iter().enumerate() {
                let label = title.as_deref().unwrap_or("");
                println!(
                    "  {:>3}  {:<18} {:<10} {}  {}",
                    i,
                    id,
                    status,
                    fmt_created(*created_ms),
                    label
                );
            }
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

        if let Ok(n) = line.parse::<usize>() {
            if n < all.len() {
                return Some(all[n].0.clone());
            }
            eprintln!("out of range");
            continue;
        }

        filter = line.to_string();
    }
}

fn fmt_created(ms: i64) -> String {
    // Local-relative age; no external time crate in the CLI.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let s = (now - ms).max(0) / 1000;
    if s < 60 {
        format!("{s}s ago")
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 86400 {
        format!("{}h ago", s / 3600)
    } else {
        format!("{}d ago", s / 86400)
    }
}

/// Handle one `/command`. Returns false when the REPL should exit.
fn command(repl: &mut Repl, line: &str) -> bool {
    let mut parts = line.splitn(2, ' ');
    let cmd = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim();
    match cmd {
        "/help" => println!("{HELP}"),
        "/new" => {
            let new_id = pantheon_runtime::new_run_id();
            println!("run {}", new_id);
            repl.run_id = new_id;
            if let Some((p, m)) = &repl.model {
                println!("(model {} {} applies to the next turn)", p, m);
            }
        }
        "/runs" => repl.print_runs(20),
        "/history" => {
            // Interactive searchable history: /runs list + type-to-filter +
            // pick-by-number, then switch runs like /resume does.
            match repl.session.supervisor.ledger_list_runs(50) {
                Ok(runs) => {
                    let target = pick_run(&runs);
                    if let Some(id) = target {
                        if id != repl.run_id {
                            match repl.session.supervisor.ledger_status(&id) {
                                Ok(Some(_)) => {
                                    println!("resumed {}", id);
                                    repl.run_id = id;
                                }
                                Ok(None) => println!("no run {}", id),
                                Err(e) => eprintln!("status: {e}"),
                            }
                        }
                    }
                }
                Err(e) => eprintln!("list runs: {e}"),
            }
        }
        "/resume" => {
            if rest.is_empty() {
                repl.print_runs(20);
                println!("resume which? give an id or a number");
                return true;
            }
            // Number form indexes into the same list /runs printed.
            let target = if let Ok(n) = rest.parse::<usize>() {
                match repl.session.supervisor.ledger_list_runs(20) {
                    Ok(runs) if n < runs.len() => Some(runs[n].0.clone()),
                    Ok(_) => {
                        println!("out of range");
                        return true;
                    }
                    Err(e) => {
                        eprintln!("list runs: {e}");
                        return true;
                    }
                }
            } else {
                Some(rest.to_string())
            };
            if let Some(id) = target {
                // Prove the run exists before switching.
                match repl.session.supervisor.ledger_status(&id) {
                    Ok(Some(_)) => {
                        println!("resumed {}", id);
                        repl.run_id = id;
                    }
                    Ok(None) => println!("no run {}", id),
                    Err(e) => eprintln!("status: {e}"),
                }
            }
        }
        "/status" => {
            let status = repl
                .session
                .supervisor
                .ledger_status(&repl.run_id)
                .ok()
                .flatten()
                .unwrap_or_else(|| "new".into());
            match repl
                .session
                .supervisor
                .ledger_title(&repl.run_id)
                .ok()
                .flatten()
                .filter(|t| !t.is_empty())
            {
                Some(title) => println!("run {} ({}) — {}", repl.run_id, status, title),
                None => println!("run {} ({})", repl.run_id, status),
            }
        }
        "/memory" => {
            if rest.is_empty() {
                println!("usage: /memory <query>");
                return true;
            }
            let store =
                match pantheon_memory::MemoryStore::open(&crate::data_dir().join("memory.db")) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("open memory: {e}");
                        return true;
                    }
                };
            let policy = pantheon_core::capability::Policy::coder();
            match pantheon_memory::recall(&store, &policy, &default_layers(), rest, 8) {
                Ok(hits) => {
                    if hits.is_empty() {
                        println!("(no matches)");
                    }
                    for h in hits {
                        let trust = h.record.provenance.trust.as_str();
                        println!(
                            "[{:?}] {} = {} [trust:{}]",
                            h.record.layer, h.record.key, h.record.value, trust
                        );
                    }
                }
                Err(e) => eprintln!("recall: {e}"),
            }
        }
        "/remember" => {
            let mut it = rest.splitn(2, ' ');
            let key = it.next().unwrap_or("").trim();
            let value = it.next().unwrap_or("").trim();
            if key.is_empty() || value.is_empty() {
                println!("usage: /remember <key> <text>");
                return true;
            }
            let store =
                match pantheon_memory::MemoryStore::open(&crate::data_dir().join("memory.db")) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("open memory: {e}");
                        return true;
                    }
                };
            let proposal = pantheon_memory::Proposal {
                layer: pantheon_memory::LayerKind::Agent,
                namespace: repl.namespace.clone(),
                key: key.to_string(),
                value: value.to_string(),
                provenance: pantheon_memory::Provenance {
                    source: "cli".into(),
                    origin: "user".into(),
                    trust: pantheon_core::provenance::TrustTier::User,
                    recorded_at_ms: 0,
                },
            };
            match pantheon_memory::propose_write(
                &store,
                &pantheon_core::capability::Policy::coder_with_memory(),
                proposal,
                4096,
            ) {
                Ok(_) => println!("stored {key}"),
                Err(e) => eprintln!("memory put: {e}"),
            }
        }
        "/policy" => {
            let granted: Vec<String> = repl
                .session
                .policy
                .granted()
                .iter()
                .map(|c| format!("{c:?}"))
                .collect();
            let approval: Vec<String> = repl
                .session
                .policy
                .approval_caps()
                .iter()
                .map(|c| format!("{c:?}"))
                .collect();
            println!("preset: {}", repl.policy_preset);
            println!("granted: {}", granted.join(", "));
            println!("needs approval: {}", approval.join(", "));
        }
        "/model" => {
            let mut it = rest.split_whitespace();
            let p = it.next().map(|s| s.to_string());
            let m = it.next().map(|s| s.to_string());
            match (p, m) {
                (None, _) => match &repl.model {
                    Some((p, m)) => println!("model override: {} {}", p, m),
                    None => println!("no override; using the session default"),
                },
                (Some(p), Some(m)) => {
                    println!("model override set: {} {} (applies next turn)", p, m);
                    repl.model = Some((p, m));
                }
                (Some(p), None) => {
                    println!("usage: /model <provider> <model>  (got only provider {p})")
                }
            }
        }
        "/name" => {
            let current = repl
                .session
                .supervisor
                .ledger_title(&repl.run_id)
                .ok()
                .flatten()
                .filter(|t| !t.is_empty());
            if rest.is_empty() {
                match current {
                    Some(t) => println!("{t}"),
                    None => println!("(untitled)"),
                }
                println!("rename with: /name <new title>");
                return true;
            }
            // Normalize exactly like a model reply: one line, bounded —
            // the manual path shares the title contract with the aux.
            let title =
                pantheon_core::model::bound_title(rest, pantheon_core::model::TITLE_MAX_CHARS);
            if title.is_empty() {
                eprintln!("/name: nothing to title with");
                return true;
            }
            // A not-yet-started run has no row to store the title in;
            // create it so the rename survives (and shows in /history).
            if repl
                .session
                .supervisor
                .ledger_status(&repl.run_id)
                .ok()
                .flatten()
                .is_none()
            {
                if let Err(e) = repl.session.supervisor.start_run(&repl.run_id) {
                    eprintln!("/name: {e}");
                    return true;
                }
            }
            // Manual rename: a SessionTitled event like any other, so
            // last-write-wins and `pantheon logs` shows who named it. A later
            // auto-title is suppressed (the run is already titled).
            let ev = pantheon_core::events::Event::SessionTitled {
                run_id: repl.run_id.clone(),
                title: title.clone(),
                model: "user".into(),
                source: "manual".into(),
            };
            match repl.session.supervisor.emit(ev) {
                Ok(()) => println!("\u{2192} {title}"),
                Err(e) => eprintln!("/name: {e}"),
            }
        }
        "/exit" | "/quit" => return false,
        _ => {
            println!("unknown command {cmd}; /help lists commands");
        }
    }
    true
}

fn default_layers() -> [pantheon_memory::LayerKind; 3] {
    [
        pantheon_memory::LayerKind::Project,
        pantheon_memory::LayerKind::Agent,
        pantheon_memory::LayerKind::Global,
    ]
}

#[cfg(test)]
#[path = "session_cli_tests.rs"]
mod tests;

/// Model policy for the REPL: flags/env/config precedence, same as chat.
pub fn build_model_policy(
    file_cfg: &Option<config_doc::Config>,
    provider: Option<String>,
    model: Option<String>,
) -> pantheon_core::model::ModelPolicy {
    let cfg_model = file_cfg
        .as_ref()
        .and_then(|c| c.model.clone())
        .map(|m| (Some(m.provider), Some(m.model)));
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
    pantheon_core::model::ModelPolicy {
        default: default.clone(),
        fallbacks: chain,
        auxiliaries: config_doc::auxiliaries(file_cfg.as_ref(), &default),
    }
}

/// Entry for `pantheon --resume [id]`: same as `run_session`, but the
/// run id is pinned to the given (or most recent) run instead of a fresh
/// auto-resume. A missing id exits with the same "no run" wording /resume
/// uses, so `--resume bad-id` fails loudly, not silently fresh.
pub fn run_session_with_resume(resume_id: Option<String>) {
    // Validate the target before opening the session.
    let pinned = match resume_id {
        Some(id) => {
            // The ledger needs the session's supervisor; validate through a
            // lightweight ledger open on the data dir instead of building a
            // full Session first.
            match pantheon_runtime::Supervisor::open(crate::data_dir()) {
                Ok(sup) => match sup.ledger_status(&id) {
                    Ok(Some(_)) => id,
                    Ok(None) => {
                        eprintln!("no run {id}");
                        std::process::exit(1);
                    }
                    Err(e) => {
                        eprintln!("resume: {e}");
                        std::process::exit(1);
                    }
                },
                Err(e) => {
                    eprintln!("open session: {e}");
                    std::process::exit(1);
                }
            }
        }
        None => {
            // No id: most recent run, same as auto-resume.
            match pantheon_runtime::Supervisor::open(crate::data_dir()) {
                Ok(sup) => match sup.ledger_list_runs(1) {
                    Ok(runs) if !runs.is_empty() => runs[0].0.clone(),
                    _ => {
                        eprintln!("no runs to resume; start a conversation first");
                        std::process::exit(1);
                    }
                },
                Err(e) => {
                    eprintln!("open session: {e}");
                    std::process::exit(1);
                }
            }
        }
    };
    run_session_inner(Some(pinned));
}

/// Entry: `pantheon` with no args.
pub fn run_session() {
    run_session_inner(None);
}

/// `pinned`: Some(id) from `--resume` — skip auto-resume and use the id.
pub fn run_session_inner(pinned_id: Option<String>) {
    let file_cfg = config_doc::Config::load_or_report(&crate::data_dir());
    let model_policy = build_model_policy(&file_cfg, None, None);
    let policy = config_schema::policy_for_config(&file_cfg);
    let secrets = config_doc::chat_secrets(file_cfg.as_ref());
    let mut session = match Session::new(crate::data_dir(), policy, model_policy, secrets) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("open session: {e}");
            std::process::exit(1);
        }
    };
    let namespace = std::env::var("PANTHEON_MEMORY_NAMESPACE").unwrap_or_else(|_| "nyx".into());

    // Install a streaming callback: print TextDelta events as they arrive,
    // plus tool call names and usage info. This gives the user Claude Code-
    // style live output without a full TUI dependency.
    session.on_event = Some(Box::new(|ev| {
        use pantheon_core::model_event::ModelEvent;
        use std::io::Write;
        match ev {
            ModelEvent::TextDelta { text } => {
                let _ = std::io::stdout().write_all(text.as_bytes());
                let _ = std::io::stdout().flush();
            }
            ModelEvent::ReasoningDelta { text } => {
                let _ = writeln!(std::io::stdout(), "\n◊|{text}\n|◊");
            }
            ModelEvent::ToolCall { name, .. } => {
                let _ = writeln!(std::io::stdout(), "\n[tool: {name}]");
            }
            ModelEvent::Usage { usage } => {
                if let Some(cost) = usage.cost_usd {
                    let _ = writeln!(
                        std::io::stdout(),
                        "\n[usage: {} tokens, ${:.4}]",
                        usage.total_tokens,
                        cost
                    );
                } else {
                    let _ = writeln!(
                        std::io::stdout(),
                        "\n[usage: {} tokens]",
                        usage.total_tokens
                    );
                }
            }
            _ => {}
        }
    }));

    let run_id = if let Some(pinned) = pinned_id {
        println!("resumed {pinned} (--resume)");
        pinned
    } else {
        // Auto-resume: continue the most recent run if one exists.
        match session.supervisor.ledger_list_runs(1) {
            Ok(runs) if !runs.is_empty() => {
                let (id, status, _, _) = runs[0].clone();
                println!(
                    "resuming {id} ({status}); /new for a fresh conversation, /help for commands"
                );
                id
            }
            _ => {
                let id = pantheon_runtime::new_run_id();
                println!("new run {id}; type a message or /help");
                id
            }
        }
    };

    // Name the preset the way the user wrote it, so /policy can echo it.
    let policy_preset = file_cfg
        .as_ref()
        .and_then(|c| c.policy)
        .map(|p| p.as_str().to_string())
        .or_else(|| std::env::var("PANTHEON_POLICY").ok())
        .unwrap_or_else(|| "built-in default".to_string());

    let mut repl = Repl {
        session,
        run_id,
        model: None,
        namespace,
        policy_preset,
    };

    let stdin = std::io::stdin();
    loop {
        print!("> ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break, // EOF
            Ok(_) => {}
            Err(_) => break,
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('/') {
            if !command(&mut repl, line) {
                break;
            }
            continue;
        }
        // Apply a pending /model override by rebuilding the session's
        // model policy in place is not possible (Session is immutable);
        // instead the override is applied by rotating the run: the model
        // policy is read per turn from the session struct. For now the
        // override takes effect by restarting the session object.
        if let Some((p, m)) = repl.model.clone() {
            let file_cfg = config_doc::Config::load_or_report(&crate::data_dir());
            let mp = build_model_policy(&file_cfg, Some(p), Some(m));
            let pol = repl.session.policy.clone();
            let secrets = config_doc::chat_secrets(file_cfg.as_ref());
            if let Ok(s) = Session::new(crate::data_dir(), pol, mp, secrets) {
                repl.session = s;
            }
            repl.model = None;
        }
        // Remember whether the run was already titled, so the REPL can
        // announce the generated session title once, right after the turn
        // that produced it (fire-and-forget lands during that turn).
        let had_title = repl
            .session
            .supervisor
            .ledger_title(&repl.run_id)
            .ok()
            .flatten()
            .is_some();
        match repl.session.chat(&repl.run_id, line) {
            Ok(_) => {
                if !had_title {
                    if let Some(title) = repl
                        .session
                        .supervisor
                        .ledger_title(&repl.run_id)
                        .ok()
                        .flatten()
                        .filter(|t| !t.is_empty())
                    {
                        println!("(session: {title})");
                    }
                }
            }
            Err(e) => {
                // RUN_PARKED already names the exact grant/deny command with
                // the scope inlined, so printing a second line with a literal
                // `<scope>` placeholder just told the user to do the lookup the
                // error had already done for them.
                if e.code == "RUN_PARKED" {
                    println!("{e}");
                } else {
                    eprintln!("turn failed: {e}");
                }
            }
        }
    }
    println!(
        "session closed; conversation stays resumable (run {})",
        repl.run_id
    );
}
