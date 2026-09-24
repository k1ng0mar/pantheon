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
  /status            show the current run id and status
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
}

impl Repl {
    fn print_runs(&self, limit: usize) {
        match self.session.supervisor.ledger_list_runs(limit) {
            Ok(runs) => {
                if runs.is_empty() {
                    println!("(no runs yet)");
                    return;
                }
                for (i, (id, status, created_ms)) in runs.iter().enumerate() {
                    println!(
                        "{:>3}  {}  {:<18} {}",
                        i,
                        status,
                        id,
                        fmt_created(*created_ms)
                    );
                }
            }
            Err(e) => eprintln!("list runs: {e}"),
        }
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
            match target {
                Some(id) => {
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
                None => {}
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
            println!("run {} ({})", repl.run_id, status);
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

/// Model policy for the REPL: flags/env/config precedence, same as chat.
fn build_model_policy(
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
        default,
        fallbacks: chain,
        auxiliaries: vec![],
    }
}

/// Entry: `pantheon` with no args, or `pantheon session`.
pub fn run_session() {
    let file_cfg = config_doc::Config::load(&crate::data_dir()).ok();
    let model_policy = build_model_policy(&file_cfg, None, None);
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
    let api_key = std::env::var("PANTHEON_API_KEY").unwrap_or_default();
    let session = match Session::new(crate::data_dir(), policy, model_policy, api_key) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("open session: {e}");
            std::process::exit(1);
        }
    };
    let namespace = std::env::var("PANTHEON_MEMORY_NAMESPACE").unwrap_or_else(|_| "nyx".into());

    // Auto-resume: continue the most recent run if one exists.
    let run_id = match session.supervisor.ledger_list_runs(1) {
        Ok(runs) if !runs.is_empty() => {
            let (id, status, _) = runs[0].clone();
            println!("resuming {id} ({status}); /new for a fresh conversation, /help for commands");
            id
        }
        _ => {
            let id = pantheon_runtime::new_run_id();
            println!("new run {id}; type a message or /help");
            id
        }
    };

    let mut repl = Repl {
        session,
        run_id,
        model: None,
        namespace,
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
            let file_cfg = config_doc::Config::load(&crate::data_dir()).ok();
            let mp = build_model_policy(&file_cfg, Some(p), Some(m));
            let pol = repl.session.policy.clone();
            let key = repl.session.api_key.clone();
            if let Ok(s) = Session::new(crate::data_dir(), pol, mp, key) {
                repl.session = s;
            }
            repl.model = None;
        }
        match repl.session.chat(&repl.run_id, line) {
            Ok(_) => {}
            Err(e) => {
                eprintln!("turn failed: {e}");
                if e.code == "RUN_PARKED" {
                    println!("grant with: pantheon grant {} <scope>", repl.run_id);
                }
            }
        }
    }
    println!(
        "session closed; conversation stays resumable (run {})",
        repl.run_id
    );
}
