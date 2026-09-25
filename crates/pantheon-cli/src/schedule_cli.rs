//! `pantheon schedule` — durable interval/cron/manual task scheduling.
//!
//! Jobs are stored as JSON in the data dir. Occurrence idempotency
//! (§21) is handled by the scheduler's DurableClaimLedger over the
//! ClaimStore.

use pantheon_runtime::Supervisor;
use pantheon_scheduler::{Job, MissedPolicy, ScheduleKind};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
        }
    }
}

/// Parse a duration string like "30m", "6h", "1d" into milliseconds.
fn parse_duration(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, unit) = if s.ends_with("ms") {
        (&s[..s.len() - 2], 1)
    } else if s.ends_with('s') {
        (&s[..s.len() - 1], 1000)
    } else if s.ends_with('m') {
        (&s[..s.len() - 1], 60_000)
    } else if s.ends_with('h') {
        (&s[..s.len() - 1], 3_600_000)
    } else if s.ends_with('d') {
        (&s[..s.len() - 1], 86_400_000)
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
    if matches!(
        subcommand,
        "list" | "pause" | "resume" | "cancel" | "run"
    ) {
        handle_subcommand(&args[2..], data_dir);
        return;
    }

    // Create: pantheon schedule <task> [--every|N<unit>] [--agent NAME] [--cron EXPR]
    let (task, every, agent, cron) = parse_create_args(&args[2..]);

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
    let stored = StoredJob {
        id: job_id.clone(),
        task,
        kind: kind.clone(),
        agent,
        missed: MissedPolicy::RunOnce,
        paused: false,
        last_run: None,
    };

    let mut jobs = load_jobs(data_dir);
    jobs.push(stored);
    if let Err(e) = save_jobs(data_dir, &jobs) {
        eprintln!("save failed: {e}");
        std::process::exit(1);
    }

    println!(
        "scheduled {} [{}] — {} | cancel: pantheon schedule cancel {}",
        job_id,
        format_kind(&kind),
        "runs",
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
                println!(
                    "{}  {}  {}  [{}]  last: {}  | cancel: pantheon schedule cancel {}",
                    &j.id[..8],
                    status,
                    format_kind(&j.kind),
                    j.task,
                    format_last(j.last_run),
                    j.id
                );
            }
        }
        "pause" | "resume" | "cancel" => {
            let id = if parts.len() > 1 { &parts[1] } else {
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
                        "cancel" => {
                            jobs.retain(|x| x.id != *id);
                        }
                        _ => unreachable!(),
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
            let id = if parts.len() > 1 { &parts[1] } else {
                eprintln!("usage: pantheon schedule run <id>");
                std::process::exit(2);
            };
            let jobs = load_jobs(data_dir);
            match jobs.iter().find(|j| j.id == *id) {
                Some(j) => {
                    run_job_now(j, data_dir);
                }
                None => {
                    eprintln!("not found: {id}");
                    std::process::exit(1);
                }
            }
        }
        _ => unreachable!(),
    }
}

fn parse_create_args(args: &[String]) -> (String, Option<String>, Option<String>, Option<String>) {
    let mut task = String::new();
    let mut every = None;
    let mut agent = None;
    let mut cron = None;
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
    (task, every, agent, cron)
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

fn run_job_now(job: &StoredJob, data_dir: &PathBuf) {
    let sup = match Supervisor::open(data_dir.clone()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("open runtime: {e}");
            std::process::exit(1);
        }
    };
    let run_id = pantheon_runtime::new_run_id();
    let recovered = sup.start_run(&run_id).unwrap_or_else(|e| {
        eprintln!("start: {e}");
        std::process::exit(1);
    });
    if recovered {
        println!("(recovered run {run_id})");
    }
    sup.emit(pantheon_core::events::Event::RunProgress {
        run_id: run_id.clone(),
        detail: format!("scheduled task: {}", job.task),
    })
    .unwrap();
    let _ = sup.complete(&run_id);
    println!("ran {} — run {}", job.task, run_id);
}
