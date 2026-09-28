//! `pantheon schedule` — durable interval/cron/manual task scheduling.
//!
//! Jobs are stored as JSON in the data dir. Occurrence idempotency
//! (§21) is handled by the scheduler's DurableClaimLedger over the
//! ClaimStore.

use pantheon_scheduler::{
    DurableClaimLedger, Job, MissedPolicy, OverlapPolicy, ScheduleKind, TickDecision, TickDriver,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    /// Per-job run ceiling in seconds. None = the scheduler default
    /// (10 minutes); a run past it is abandoned.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// What a tick does when the job is still running. Default: skip.
    #[serde(default)]
    pub overlap: OverlapPolicy,
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
            timeout_secs: s.timeout_secs,
            overlap: s.overlap,
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

fn store_path(data_dir: &Path) -> PathBuf {
    data_dir.join("schedule.json")
}

/// Load jobs, treating a corrupt file as an error rather than as "no jobs".
///
/// A silent `unwrap_or_default` meant a malformed schedule.json read as an
/// empty list and the next create overwrote the user's jobs.
pub fn load_jobs(data_dir: &Path) -> Result<Vec<StoredJob>, String> {
    let path = store_path(data_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    serde_json::from_str(&text).map_err(|e| format!("{} is corrupt: {e}", path.display()))
}

fn save_jobs(data_dir: &Path, jobs: &[StoredJob]) -> Result<(), String> {
    let path = store_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(jobs).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| e.to_string())?;
    Ok(())
}

/// Public wrapper for the TUI's /schedule command. Returns an empty vec
/// when no schedule file exists; reports corruption as an error string.
pub fn load_jobs_public(data_dir: &Path) -> Vec<StoredJob> {
    load_jobs(data_dir).unwrap_or_default()
}

/// Load jobs or exit. A corrupt schedule file is a stop, not an empty list.
fn load_or_exit(data_dir: &Path) -> Vec<StoredJob> {
    match load_jobs(data_dir) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("schedule: {e}");
            eprintln!("fix: delete or repair {}", store_path(data_dir).display());
            std::process::exit(1);
        }
    }
}

/// Schedule a task to run repeatedly.
pub fn cmd_schedule(args: &[String], data_dir: &Path) {
    if args.len() < 3 {
        eprintln!("usage: pantheon schedule <task> (--every 30m | --cron '0 9 * * *') [--agent nyx] [--timeout 10m] [--overlap skip|replace|queue]");
        eprintln!("       pantheon schedule list|pause|resume|cancel|run|tick <id>");
        eprintln!("       pantheon schedule webhook sign|verify --body <text> [--secret <s>]");
        std::process::exit(2);
    }

    let subcommand = args[2].as_str();
    if matches!(
        subcommand,
        "list" | "pause" | "resume" | "cancel" | "run" | "tick" | "webhook" | "prune"
    ) {
        handle_subcommand(&args[2..], data_dir);
        return;
    }

    // Create: pantheon schedule <task> [--every|N<unit>] [--agent NAME] [--cron EXPR]
    //          [--model M] [--provider P] [--timeout 10m] [--overlap skip|replace|queue]
    match build_scheduled_job(&args[2..]) {
        Err(e) => {
            eprintln!("error: {e}");
            eprintln!("not scheduled: fix the arguments and retry");
            std::process::exit(2);
        }
        Ok(stored) => {
            let job_id = stored.id.clone();
            let kind = stored.kind.clone();
            let mut jobs = load_or_exit(data_dir);
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
    }
}

/// Build the stored job from `schedule create` arguments, validating
/// everything before anything is persisted.
///
/// A bad cron expression used to be stored without a murmur and then
/// silently never fire. Now it is rejected here, with the field and the
/// reason, so a broken schedule is a loud error at registration instead
/// of a quiet no-show at 3am.
fn build_scheduled_job(args: &[String]) -> Result<StoredJob, String> {
    // Create: pantheon schedule <task> [--every|N<unit>] [--agent NAME] [--cron EXPR] [--model M] [--provider P]
    let CreateArgs {
        task,
        every,
        agent,
        cron,
        model,
        provider,
        timeout,
        overlap,
    } = parse_create_args(args);
    if task.is_empty() {
        return Err("need a task: pantheon schedule <task> --every 30m|--cron '0 9 * * *'".into());
    }

    let kind = if let Some(expr) = &cron {
        ScheduleKind::Cron { expr: expr.clone() }
    } else if let Some(dur) = &every {
        match parse_duration(dur) {
            Ok(ms) => ScheduleKind::Interval { every_ms: ms },
            Err(e) => return Err(format!("bad duration: {e}")),
        }
    } else {
        return Err("need --every <duration> or --cron <expr>".into());
    };

    let job_id = format!("job_{}", pantheon_runtime::new_run_id());
    let mut probe = Job::new(&job_id, kind.clone(), "nyx");
    // Registration-time validation: reject the broken expression now.
    probe
        .validate()
        .map_err(|e| format!("invalid --cron expression: {e}"))?;
    if let Some(m) = model.as_deref() {
        probe
            .pin_model(m, provider.as_deref())
            .map_err(|e| format!("bad pin: {e}"))?;
    } else if provider.is_some() {
        return Err("--provider without --model pins nothing; add --model to pin".into());
    }
    let timeout_secs = match timeout.as_deref() {
        None => None,
        Some(d) => {
            let ms = parse_duration(d).map_err(|e| format!("bad --timeout: {e}"))?;
            let secs = ms / 1000;
            if secs == 0 {
                return Err("bad --timeout: must be at least 1s".into());
            }
            Some(secs)
        }
    };
    let overlap = match overlap.as_deref() {
        None => OverlapPolicy::default(),
        Some(o) => o
            .parse::<OverlapPolicy>()
            .map_err(|e| format!("bad --overlap: {e}"))?,
    };
    Ok(StoredJob {
        id: job_id,
        task,
        kind,
        agent,
        missed: MissedPolicy::RunOnce,
        paused: false,
        last_run: None,
        model: probe.model,
        provider: probe.provider,
        timeout_secs,
        overlap,
    })
}

fn handle_subcommand(parts: &[String], data_dir: &Path) {
    match parts[0].as_str() {
        "list" => {
            let jobs = load_or_exit(data_dir);
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
                    j.id.as_str(),
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
            let mut jobs = load_or_exit(data_dir);
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
            let jobs = load_or_exit(data_dir);
            match jobs.iter().find(|j| j.id == *id) {
                Some(j) => {
                    // A manual `run` keeps the old hard-fail behavior: the
                    // tick loop instead logs and continues with other jobs.
                    if let Err(e) = run_job_now(j, data_dir) {
                        eprintln!("{e}");
                        std::process::exit(1);
                    }
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
        "tick" => {
            // Fire every job that is due. Jobs used to be written to
            // schedule.json and nothing ever read it against a clock, so no
            // job could ever fire on its own. `tick` is the primitive a
            // daemon, cron entry, or CI step calls; `--watch` keeps it
            // running in the foreground.
            //
            // Every fire goes through the durable claim ledger first: the
            // claim is an atomic first-wins INSERT, so two ticks racing the
            // same due job — two threads, two processes, or a restart
            // replaying a minute — agree on exactly one winner instead of
            // double-executing. A claim that cannot be persisted fails
            // closed: the run does not start.
            let watch = parts.iter().any(|a| a == "--watch");
            let ledger = match DurableClaimLedger::open(&data_dir.join("claims.db")) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("tick: cannot open claim ledger: {e}");
                    eprintln!("tick: refusing to fire without durable claims");
                    std::process::exit(1);
                }
            };
            let driver = Arc::new(TickDriver::new(ledger));
            loop {
                // Automatic retention: at most one prune pass per day,
                // so --watch daemons keep the ledger bounded on their own.
                maybe_run_retention(data_dir);
                let jobs = load_or_exit(data_dir);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                let mut fired = Vec::new();
                for j in jobs.iter().filter(|j| !j.paused) {
                    let scheduled = Job::from(j.clone());
                    let stored = j.clone();
                    let dd = data_dir.to_path_buf();
                    let execute = Arc::new(move || {
                        if let Err(e) = run_job_now(&stored, &dd) {
                            eprintln!("{e}");
                        }
                    });
                    match driver.tick_job(&scheduled, now, j.last_run, execute) {
                        TickDecision::Fired { .. } => {
                            println!("tick {now}: fired {}", j.id);
                            fired.push(j.id.clone());
                        }
                        TickDecision::Queued => {
                            println!("tick {now}: {} still running, queued (overlap=queue)", j.id);
                            fired.push(j.id.clone());
                        }
                        TickDecision::NotDue => {}
                        TickDecision::SkippedClaimLost => {
                            println!("tick {now}: {} already claimed, skipping (replay)", j.id);
                        }
                        TickDecision::SkippedOverlap => {
                            println!(
                                "tick {now}: {} still running, skipping (overlap=skip)",
                                j.id
                            );
                        }
                        TickDecision::ClaimFailed(e) => {
                            eprintln!("tick {now}: claim failed for {}: {e} (not run)", j.id);
                        }
                    }
                }
                if fired.is_empty() {
                    println!("tick {now}: nothing due");
                } else {
                    // Persist fire times so an interval job does not
                    // immediately come due again on the next tick. The
                    // durable claim already won is what makes this
                    // crash-safe; last_run just drives the due check.
                    let mut jobs = jobs;
                    for j in jobs.iter_mut() {
                        if fired.contains(&j.id) {
                            j.last_run = Some(now);
                        }
                    }
                    if let Err(e) = save_jobs(data_dir, &jobs) {
                        eprintln!("warning: could not record last_run: {e}");
                    }
                }
                if !watch {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_secs(30));
            }
        }
        "webhook" => handle_webhook_command(&parts[1..]),
        "prune" => {
            // Manual trigger for the retention pass (the tick loop runs it
            // automatically at most once per day). `--days N` overrides the
            // configured `[retention] keep_days` for this run only.
            let mut days: Option<u32> = None;
            let mut it = parts[1..].iter();
            while let Some(a) = it.next() {
                if a == "--days" {
                    let v = it.next().unwrap_or_else(|| {
                        eprintln!("usage: pantheon schedule prune [--days N]");
                        std::process::exit(2);
                    });
                    days = Some(v.parse().unwrap_or_else(|_| {
                        eprintln!("prune: bad --days {v:?}");
                        std::process::exit(2);
                    }));
                } else {
                    eprintln!("unknown flag: {a}");
                    std::process::exit(2);
                }
            }
            let keep_days = days.unwrap_or_else(|| {
                crate::config::Config::load_or_report(data_dir)
                    .map(|c| c.retention_days())
                    .unwrap_or(crate::config::DEFAULT_RETENTION_DAYS)
            });
            if keep_days == 0 {
                println!("retention disabled (keep_days = 0); nothing pruned");
                return;
            }
            match run_retention(data_dir, keep_days) {
                Ok(r) => {
                    record_retention_pass(data_dir);
                    println!(
                        "retention: pruned {} events, {} search chunks, {} claims older than {}d (active runs kept)",
                        r.events_pruned,
                        r.search_chunks_pruned,
                        r.claims_pruned,
                        r.keep_days
                    );
                }
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            // Guarded by cmd_schedule's subcommand allow-list; fail loud
            // (not unreachable) so a new subcommand can't silently no-op.
            eprintln!("usage: pantheon schedule list|pause|resume|cancel|run <id>");
            eprintln!("       pantheon schedule tick [--watch]");
            eprintln!("       pantheon schedule prune [--days N]");
            eprintln!("       pantheon schedule webhook sign|verify --body <text> [--secret <s>]");
            std::process::exit(2);
        }
    }
}

/// How often the automatic retention pass may run. The tick loop calls
/// `maybe_run_retention` every iteration (every 30s in `--watch` mode);
/// the gate below keeps a long-running daemon to one pass per day.
const RETENTION_INTERVAL_MS: i64 = 24 * 60 * 60 * 1000;

/// What one retention pass removed. Returned so the caller can log it —
/// pruning must be visible, never silent.
#[derive(Debug, Default)]
pub struct RetentionReport {
    pub keep_days: u32,
    pub events_pruned: usize,
    pub search_chunks_pruned: usize,
    pub claims_pruned: usize,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Run the retention pass now: prune ledger events, the FTS search sidecar,
/// and idempotency claims older than `keep_days`. Runs whose status is not
/// terminal are never pruned, however old their events — the active run's
/// transcript is what a resume rebuilds from.
pub fn run_retention(data_dir: &Path, keep_days: u32) -> Result<RetentionReport, String> {
    let cutoff = now_ms() - keep_days as i64 * 86_400_000;
    let mut report = RetentionReport {
        keep_days,
        ..Default::default()
    };

    let ledger_path = data_dir.join("ledger.db");
    if ledger_path.exists() {
        let ledger = pantheon_storage::ledger::Ledger::open(&ledger_path)
            .map_err(|e| format!("retention: cannot open ledger: {e}"))?;
        report.events_pruned = ledger
            .prune_events_before_active_safe(cutoff)
            .map_err(|e| format!("retention: prune events: {e}"))?;
        // The FTS sidecar lives in the same database file.
        match pantheon_storage::search::SessionSearch::open(&ledger_path) {
            Ok(search) => {
                report.search_chunks_pruned = search
                    .prune_before(cutoff)
                    .map_err(|e| format!("retention: prune search index: {e}"))?;
            }
            Err(e) => eprintln!("retention: search index unavailable, skipping: {e}"),
        }
    }
    let claims_path = data_dir.join("claims.db");
    if claims_path.exists() {
        let claims = DurableClaimLedger::open(&claims_path)
            .map_err(|e| format!("retention: cannot open claim ledger: {e}"))?;
        report.claims_pruned = claims
            .prune_before(cutoff)
            .map_err(|e| format!("retention: prune claims: {e}"))?;
    }
    Ok(report)
}

fn retention_state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("retention.json")
}

/// True when a retention pass is due: never ran, or the last pass is older
/// than `RETENTION_INTERVAL_MS`.
fn retention_due(data_dir: &Path) -> bool {
    let last: i64 = std::fs::read_to_string(retention_state_path(data_dir))
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.get("last_prune_ms")?.as_i64())
        .unwrap_or(0);
    now_ms() - last >= RETENTION_INTERVAL_MS
}

fn record_retention_pass(data_dir: &Path) {
    let state = serde_json::json!({ "last_prune_ms": now_ms() });
    let _ = std::fs::write(retention_state_path(data_dir), state.to_string());
}

/// The automatic pass, called from the tick loop. No-op when retention is
/// disabled (`keep_days = 0`) or the last pass is still fresh. Logs what
/// was pruned.
fn maybe_run_retention(data_dir: &Path) {
    let keep_days = crate::config::Config::load_or_report(data_dir)
        .map(|c| c.retention_days())
        .unwrap_or(crate::config::DEFAULT_RETENTION_DAYS);
    if keep_days == 0 || !retention_due(data_dir) {
        return;
    }
    match run_retention(data_dir, keep_days) {
        Ok(r) => {
            record_retention_pass(data_dir);
            println!(
                "retention: pruned {} events, {} search chunks, {} claims older than {}d (active runs kept)",
                r.events_pruned, r.search_chunks_pruned, r.claims_pruned, r.keep_days
            );
        }
        Err(e) => eprintln!("{e}"),
    }
}

/// `pantheon schedule webhook sign|verify` — HMAC-SHA256 signing for the
/// webhook trigger contract (§21, `pantheon_scheduler::webhook`).
///
/// The signing primitive lived in the scheduler with no surface: an
/// operator wiring an external sender (GitHub, Stripe, …) into a webhook
/// job had no way to mint a valid `X-Pantheon-Signature` or to check that
/// their sender's signatures verify against the shared secret. `sign`
/// mints the header value for a body; `verify` checks one. Both go through
/// the scheduler's primitives, so the contract cannot drift between here
/// and the trigger path.
///
/// Secret precedence: `--secret` flag, then `PANTHEON_WEBHOOK_SECRET`.
/// Missing or empty fails closed; the secret is never printed or logged.
fn handle_webhook_command(parts: &[String]) {
    let action = parts.first().map(String::as_str).unwrap_or("");
    if !matches!(action, "sign" | "verify") {
        eprintln!("usage: pantheon schedule webhook sign|verify --body <text> [--body-file <path>] [--secret <s>] [--signature <value>]");
        eprintln!("       secret: --secret flag or PANTHEON_WEBHOOK_SECRET (never logged)");
        std::process::exit(2);
    }
    let mut body: Option<String> = None;
    let mut body_file: Option<String> = None;
    let mut secret_flag: Option<String> = None;
    let mut signature: Option<String> = None;
    let mut it = parts[1..].iter().peekable();
    while let Some(a) = it.next() {
        let mut v = || {
            it.next().map(|s| s.to_string()).unwrap_or_else(|| {
                eprintln!("usage: {a} needs a value");
                std::process::exit(2);
            })
        };
        match a.as_str() {
            "--body" => body = Some(v()),
            "--body-file" => body_file = Some(v()),
            "--secret" => secret_flag = Some(v()),
            "--signature" => signature = Some(v()),
            other => {
                eprintln!("unknown flag: {other}");
                std::process::exit(2);
            }
        }
    }
    // --secret wins over the environment; an empty secret authenticates
    // nothing, so it fails closed exactly like WebhookAuth::new.
    let secret = secret_flag
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var(pantheon_scheduler::webhook::SECRET_ENV_VAR)
                .ok()
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| {
            eprintln!(
                "webhook {action}: no secret — pass --secret or set {}",
                pantheon_scheduler::webhook::SECRET_ENV_VAR
            );
            std::process::exit(2);
        });
    if body.is_some() && body_file.is_some() {
        eprintln!("webhook {action}: --body and --body-file are mutually exclusive");
        std::process::exit(2);
    }
    let body_bytes: Vec<u8> = match (body, body_file) {
        (Some(t), _) => t.into_bytes(),
        (None, Some(p)) => std::fs::read(&p).unwrap_or_else(|e| {
            eprintln!("webhook {action}: cannot read {p}: {e}");
            std::process::exit(1);
        }),
        (None, None) => Vec::new(),
    };
    match action {
        "sign" => {
            let header = pantheon_scheduler::webhook::sign(secret.as_bytes(), &body_bytes);
            println!(
                "{}: {header}",
                pantheon_scheduler::webhook::SIGNATURE_HEADER
            );
        }
        _ => {
            let sig = signature.unwrap_or_else(|| {
                eprintln!("usage: pantheon schedule webhook verify --signature <value> --body <text> [--secret <s>]");
                std::process::exit(2);
            });
            match pantheon_scheduler::webhook::verify_signature(
                secret.as_bytes(),
                &body_bytes,
                Some(&sig),
            ) {
                Ok(()) => println!("signature valid"),
                Err(e) => {
                    eprintln!("signature invalid: {e}");
                    std::process::exit(1);
                }
            }
        }
    }
}

/// What `schedule <task> ...` parsed out of the command line. Named because
/// a six-element tuple gave no clue what any position meant at the call
/// site.
struct CreateArgs {
    task: String,
    every: Option<String>,
    agent: Option<String>,
    cron: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    timeout: Option<String>,
    overlap: Option<String>,
}

fn parse_create_args(args: &[String]) -> CreateArgs {
    let mut task = String::new();
    let mut every = None;
    let mut agent = None;
    let mut cron = None;
    let mut model = None;
    let mut provider = None;
    let mut timeout = None;
    let mut overlap = None;
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
            "--timeout" => {
                i += 1;
                if i < args.len() {
                    timeout = Some(args[i].clone());
                }
            }
            "--overlap" => {
                i += 1;
                if i < args.len() {
                    overlap = Some(args[i].clone());
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
    CreateArgs {
        task,
        every,
        agent,
        cron,
        model,
        provider,
        timeout,
        overlap,
    }
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

/// Render `last_run` as elapsed time.
///
/// `last_run` is a wall-clock millisecond timestamp, not a duration. It was
/// printed as if it were elapsed seconds, so a job that fired a moment ago
/// reported "last: 1790433286s ago".
fn format_last(last: Option<i64>) -> String {
    let Some(fired_at) = last else {
        return "never".into();
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let secs = ((now - fired_at) / 1000).max(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// Fire a job now: a real agent turn against the configured model, with the
/// job's own model/provider pin applied when it has one.
///
/// This used to open a run, log a progress line, and mark it complete
/// without ever calling a model. The user saw "ran <task>" and an idle
/// ledger. A scheduled task that never executes the task is worse than one
/// that fails loudly.
///
/// Returns `Err` instead of exiting: the tick loop runs jobs on worker
/// threads, and a failing job must not take the whole daemon down with it.
/// Callers that want the old hard-fail behavior (`schedule run`) exit on
/// the error themselves.
fn run_job_now(job: &StoredJob, data_dir: &Path) -> Result<(), String> {
    use crate::config;
    use crate::config::build_model_policy;
    use pantheon_runtime::session::Session;

    let file_cfg = config::Config::load_or_report(data_dir);
    let model_policy =
        build_model_policy(file_cfg.as_ref(), job.provider.clone(), job.model.clone());
    let allow_memory = file_cfg
        .as_ref()
        .map(|c| c.policy == Some(crate::config_schema::PolicyPreset::CoderMemory))
        .unwrap_or(false);
    let policy = if allow_memory {
        pantheon_api::capability::Policy::coder_with_memory()
    } else {
        pantheon_api::capability::Policy::coder()
    };
    let secrets = config::chat_secrets(file_cfg.as_ref());
    let session = match Session::new(data_dir.to_path_buf(), policy, model_policy, secrets) {
        Ok(s) => s,
        Err(e) => return Err(format!("open session: {e}")),
    };

    let run_id = pantheon_runtime::new_run_id();
    match session.chat(&run_id, &job.task) {
        Ok(_) => {
            println!("ran {} — run {run_id}", job.task);
            Ok(())
        }
        Err(e) => {
            // A parked run is a real outcome, not a failure: it needs a
            // grant before it can finish.
            if e.code == "RUN_PARKED" {
                // The error carries the copy-pasteable grant command with the
                // scope already inlined, so a placeholder here was worse than
                // useless: it looked actionable and was not.
                println!("parked {}", job.task);
                println!("{e}");
                Ok(())
            } else {
                Err(format!("scheduled task failed: {e}"))
            }
        }
    }
}
