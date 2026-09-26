//! `pantheon schedule` — durable interval/cron/manual task scheduling.
//!
//! Jobs are stored as JSON in the data dir. Occurrence idempotency
//! (§21) is handled by the scheduler's DurableClaimLedger over the
//! ClaimStore.

use pantheon_scheduler::{Job, MissedPolicy, ScheduleKind};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A stored scheduled job with its run state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredJob {
    pub id: String,
    pub task: String,
    pub kind: ScheduleKind,
    pub agent: Option<String>,
    pub missed: MissedPolicy,
    #[serde(default)]
    pub paused: bool,
    #[serde(default)]
    pub last_run: Option<i64>,
    /// Per-job model/provider pin (Hermes parity: its jobs carry a model +
    /// provider snapshot). None = inherit the runtime default at fire time.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
}

impl From<StoredJob> for Job {
    fn from(s: StoredJob) -> Self {
        Self {
            id: s.id.clone(),
            kind: s.kind,
            idempotency_key: format!("job:{}:", s.id),
            missed: s.missed,
            paused: s.paused,
            target_agent: s.agent.unwrap_or_else(|| "nyx".into()),
            model: s.model,
            provider: s.provider,
        }
    }
}

/// Parse a duration string like "30m", "6h", "1d" into milliseconds.
fn parse_duration(s: &str) -> Result<u64, String> {
    let s = s.trim();
    // Longest suffix first: "ms" must win over "s", and slicing a char
    // boundary that ends_with already matched is safe.
    let (num, unit) = if let Some(n) = s.strip_suffix("ms") {
        (n, 1)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1000)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60_000)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3_600_000)
    } else if let Some(n) = s.strip_suffix('d') {
        (n, 86_400_000)
    } else {
        return Err(format!("unknown unit in {s} (use ms, s, m, h, d)"));
    };
    let n: u64 = num.parse().map_err(|_| format!("not a number: {num}"))?;
    Ok(n * unit)
}

fn store_path(data_dir: &PathBuf) -> PathBuf {
    data_dir.join("schedule.json")
}

fn load_jobs(data_dir: &PathBuf) -> Vec<StoredJob> {
    let path = store_path(data_dir);
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_jobs(data_dir: &PathBuf, jobs: &[StoredJob]) -> Result<(), String> {
    let path = store_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(jobs).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| e.to_string())?;
    Ok(())
}

/// Schedule a task to run repeatedly.
pub fn cmd_schedule(args: &[String], data_dir: &PathBuf) {
    if args.len() < 3 {
        eprintln!("usage: pantheon schedule <task> --30m [--agent nyx]");
        eprintln!("       pantheon schedule list|pause|resume|cancel|run <id>");
        std::process::exit(2);
    }

    let subcommand = args[2].as_str();
    if matches!(subcommand, "list" | "pause" | "resume" | "cancel" | "run") {
        handle_subcommand(&args[2..], data_dir);
        return;
    }

    // Create: pantheon schedule <task> [--every|N<unit>] [--agent NAME] [--cron EXPR] [--model M] [--provider P]
    let (task, every, agent, cron, model, provider) = parse_create_args(&args[2..]);

    let kind = if let Some(expr) = &cron {
        ScheduleKind::Cron { expr: expr.clone() }
    } else if let Some(dur) = &every {
        match parse_duration(dur) {
            Ok(ms) => ScheduleKind::Interval { every_ms: ms },
            Err(e) => {
                eprintln!("bad duration: {e}");
                std::process::exit(2);
            }
        }
    } else {
        eprintln!("error: need --every <duration> or --cron <expr>");
        std::process::exit(2);
    };

    let job_id = format!("job_{}", pantheon_runtime::new_run_id());
    let mut probe = Job::new(&job_id, kind.clone(), "nyx");
    if let Some(m) = model.as_deref() {
        if let Err(e) = probe.pin_model(m, provider.as_deref()) {
            eprintln!("bad pin: {e}");
            std::process::exit(2);
        }
    } else if provider.is_some() {
        eprintln!("note: --provider without --model pins nothing; add --model to pin");
        std::process::exit(2);
    }
    let stored = StoredJob {
        id: job_id.clone(),
        task,
        kind: kind.clone(),
        agent,
        missed: MissedPolicy::RunOnce,
        paused: false,
        last_run: None,
        model: probe.model,
        provider: probe.provider,
    };

    let mut jobs = load_jobs(data_dir);
    jobs.push(stored);
    if let Err(e) = save_jobs(data_dir, &jobs) {
        eprintln!("save failed: {e}");
        std::process::exit(1);
    }

    println!(
        "scheduled {} [{}] — runs | cancel: pantheon schedule cancel {}",
        job_id,
        format_kind(&kind),
        job_id
    );
}

fn handle_subcommand(parts: &[String], data_dir: &PathBuf) {
    match parts[0].as_str() {
        "list" => {
            let jobs = load_jobs(data_dir);
            if jobs.is_empty() {
                println!("no scheduled jobs");
                return;
            }
            for j in &jobs {
                let status = if j.paused { "paused" } else { "active" };
                let pin = match (&j.model, &j.provider) {
                    (Some(m), Some(p)) => format!("  model: {m} via {p}"),
                    (Some(m), None) => format!("  model: {m}"),
                    _ => String::new(),
                };
                println!(
                    "{}  {}  {}  [{}]{}  last: {}  | cancel: pantheon schedule cancel {}",
                    // Full id, not a byte-prefix: ids are `job_run_<ts>_<n>`,
                    // so the first 8 bytes are the shared `job_run_` and
                    // the truncated form was identical for every job.
                    &j.id,
                    status,
                    format_kind(&j.kind),
                    j.task,
                    pin,
                    format_last(j.last_run),
                    j.id
                );
            }
        }
        "pause" | "resume" | "cancel" => {
            let id = if parts.len() > 1 {
                &parts[1]
            } else {
                eprintln!("usage: pantheon schedule {} <id>", parts[0]);
                std::process::exit(2);
            };
            let mut jobs = load_jobs(data_dir);
            let found = jobs.iter_mut().find(|j| j.id == *id);
            match found {
                Some(j) => {
                    match parts[0].as_str() {
                        "pause" => j.paused = true,
                        "resume" => j.paused = false,
                        // "cancel" removes below; outer guard allows only
                        // pause|resume|cancel here.
                        _ => {
                            jobs.retain(|x| x.id != *id);
                        }
                    }
                    let _ = save_jobs(data_dir, &jobs);
                    println!("{} {}", parts[0], id);
                }
                None => {
                    eprintln!("not found: {id}");
                    std::process::exit(1);
                }
            }
        }
        "run" => {
            let id = if parts.len() > 1 {
                &parts[1]
            } else {
                eprintln!("usage: pantheon schedule run <id>");
                std::process::exit(2);
            };
            let jobs = load_jobs(data_dir);
            match jobs.iter().find(|j| j.id == *id) {
                Some(j) => {
                    run_job_now(j, data_dir);
                    // Record the fire time, otherwise `schedule list` keeps
                    // reporting "last: never" after a successful run and the
                    // user cannot tell a working job from a dead one.
                    let mut jobs = jobs;
                    if let Some(slot) = jobs.iter_mut().find(|j| j.id == *id) {
                        slot.last_run = Some(
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis() as i64)
                                .unwrap_or(0),
                        );
                    }
                    if let Err(e) = save_jobs(data_dir, &jobs) {
                        eprintln!("warning: could not record last_run: {e}");
                    }
                }
                None => {
                    eprintln!("not found: {id}");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            // Guarded by cmd_schedule's subcommand allow-list; fail loud
            // (not unreachable) so a new subcommand can't silently no-op.
            eprintln!("usage: pantheon schedule list|pause|resume|cancel|run <id>");
            std::process::exit(2);
        }
    }
}

fn parse_create_args(
    args: &[String],
) -> (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    let mut task = String::new();
    let mut every = None;
    let mut agent = None;
    let mut cron = None;
    let mut model = None;
    let mut provider = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--every" | "-e" => {
                i += 1;
                if i < args.len() {
                    every = Some(args[i].clone());
                }
            }
            "--agent" => {
                i += 1;
                if i < args.len() {
                    agent = Some(args[i].clone());
                }
            }
            "--cron" => {
                i += 1;
                if i < args.len() {
                    cron = Some(args[i].clone());
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
            s if s.starts_with("--") => {
                // Support --30m style shorthand
                let dur = s.trim_start_matches("--");
                every = Some(dur.to_string());
            }
            _ => {
                if task.is_empty() {
                    task = args[i].clone();
                }
            }
        }
        i += 1;
    }
    (task, every, agent, cron, model, provider)
}

fn format_kind(kind: &ScheduleKind) -> String {
    match kind {
        ScheduleKind::Interval { every_ms } => {
            format!("every {}ms", every_ms)
        }
        ScheduleKind::Cron { expr } => format!("cron {}", expr),
        _ => format!("{:?}", kind),
    }
}

fn format_last(last: Option<i64>) -> String {
    match last {
        Some(ms) => {
            let secs = ms / 1000;
            format!("{secs}s ago")
        }
        None => "never".into(),
    }
}

/// Fire a job now: a real agent turn against the configured model, with the
/// job's own model/provider pin applied when it has one.
///
/// This used to open a run, log a progress line, and mark it complete
/// without ever calling a model. The user saw "ran <task>" and an idle
/// ledger. A scheduled task that never executes the task is worse than one
/// that fails loudly.
fn run_job_now(job: &StoredJob, data_dir: &Path) {
    use crate::config_doc;
    use crate::session_cli::build_model_policy;
    use pantheon_runtime::session::Session;

    let file_cfg = config_doc::Config::load(data_dir).ok();
    let model_policy = build_model_policy(&file_cfg, job.provider.clone(), job.model.clone());
    let allow_memory = file_cfg
        .as_ref()
        .map(|c| c.policy == Some(crate::config_schema::PolicyPreset::CoderMemory))
        .unwrap_or(false);
    let policy = if allow_memory {
        pantheon_core::capability::Policy::coder_with_memory()
    } else {
        pantheon_core::capability::Policy::coder()
    };
    let secrets = config_doc::chat_secrets(file_cfg.as_ref());
    let session = match Session::new(data_dir.to_path_buf(), policy, model_policy, secrets) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("open session: {e}");
            std::process::exit(1);
        }
    };

    let run_id = pantheon_runtime::new_run_id();
    match session.chat(&run_id, &job.task) {
        Ok(_) => println!("ran {} — run {run_id}", job.task),
        Err(e) => {
            // A parked run is a real outcome, not a failure: it needs a
            // grant before it can finish.
            if e.code == "RUN_PARKED" {
                println!(
                    "parked {} — run {run_id} (grant: pantheon grant {run_id} <scope>)",
                    job.task
                );
            } else {
                eprintln!("scheduled task failed: {e}");
                std::process::exit(1);
            }
        }
    }
}
