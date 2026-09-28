//! `pantheon schedule` — durable interval/cron/manual task scheduling.
//!
//! Jobs are stored as JSON in the data dir. Occurrence idempotency
//! (§21) is handled by the scheduler's DurableClaimLedger over the
//! ClaimStore.

use pantheon_gateway::schedule_delivery::{self, Deliver};
use pantheon_gateway::scheduler::{
    ExecuteFn, FireOutcome, SchedulableJob, SchedulerLoop, TickReport,
};
use pantheon_scheduler::{
    apply_defaults, render_prompt, DurableClaimLedger, Job, MissedPolicy, OverlapPolicy,
    ScheduleKind, TemplateSchedule, TemplateStore,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
    /// Where the job's result goes after the run (`log` | `telegram` |
    /// `discord` | `notify` | `file:<path>`). `None` = `log`.
    #[serde(default)]
    pub deliver: Option<String>,
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
            deliver: s.deliver,
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
        eprintln!("usage: pantheon schedule <task> (--every 30m | --cron '0 9 * * *') [--agent nyx] [--timeout 10m] [--overlap skip|replace|queue] [--deliver telegram|discord|notify|file:<path>|log] [--model M] [--provider P]");
        eprintln!("       pantheon schedule create --template <name> [--var k=v ...] [--deliver ...] [--every ...|--cron ...]");
        eprintln!("       pantheon schedule template list  — built-in + user templates (<data_dir>/templates/*.toml)");
        eprintln!("       pantheon schedule reflect (--every 30m | --cron '0 2 * * *')  — scheduled reflection pass");
        eprintln!("       pantheon schedule consolidate [--every 30m | --cron '0 3 * * *']  — nightly consolidation pass (default: [consolidation] cron; needs [consolidation] enabled = true)");
        eprintln!("       pantheon schedule list|pause|resume|cancel|run|tick <id>");
        eprintln!("       pantheon schedule webhook sign|verify --body <text> [--secret <s>]");
        eprintln!();
        eprintln!("model rule: --model/--provider pin (or the template's `model` var) > the [scheduled] auxiliary model > never the interactive default.");
        eprintln!("deliver targets: log (default) | telegram (needs PANTHEON_TELEGRAM_BOT_TOKEN + PANTHEON_DELIVER_TELEGRAM_TO) | discord (needs PANTHEON_DISCORD_TOKEN + PANTHEON_DELIVER_DISCORD_TO) | notify (notify-send) | file:<path>");
        std::process::exit(2);
    }

    let subcommand = args[2].as_str();
    if subcommand == "reflect" {
        cmd_schedule_reflect(&args[3..], data_dir);
        return;
    }
    if subcommand == "consolidate" {
        cmd_schedule_consolidate(&args[3..], data_dir);
        return;
    }
    if subcommand == "template" {
        cmd_schedule_template(&args[3..], data_dir);
        return;
    }
    if subcommand == "create" {
        // `schedule create` is the explicit form of `schedule <task>`; both
        // accept the same flags (including --template/--deliver).
        match build_create_job(&args[3..], data_dir) {
            Err(e) => {
                eprintln!("error: {e}");
                eprintln!("not scheduled: fix the arguments and retry");
                std::process::exit(2);
            }
            Ok(stored) => persist_new_job(stored, data_dir),
        }
        return;
    }
    if matches!(
        subcommand,
        "list" | "pause" | "resume" | "cancel" | "run" | "tick" | "webhook" | "prune"
    ) {
        handle_subcommand(&args[2..], data_dir);
        return;
    }

    // Create: pantheon schedule <task> [--every|N<unit>] [--agent NAME] [--cron EXPR]
    //          [--model M] [--provider P] [--timeout 10m] [--overlap skip|replace|queue]
    //          [--deliver T] [--template NAME] [--var k=v]
    match build_create_job(&args[2..], data_dir) {
        Err(e) => {
            eprintln!("error: {e}");
            eprintln!("not scheduled: fix the arguments and retry");
            std::process::exit(2);
        }
        Ok(stored) => persist_new_job(stored, data_dir),
    }
}

/// Persist a newly created job and print the confirmation line. Shared by
/// `schedule <task>` and `schedule create`.
fn persist_new_job(stored: StoredJob, data_dir: &Path) {
    let job_id = stored.id.clone();
    let kind = stored.kind.clone();
    let deliver = stored.deliver.clone();
    let mut jobs = load_or_exit(data_dir);
    jobs.push(stored);
    if let Err(e) = save_jobs(data_dir, &jobs) {
        eprintln!("save failed: {e}");
        std::process::exit(1);
    }

    let deliver_note = deliver
        .as_deref()
        .map(|d| format!(" → {d}"))
        .unwrap_or_default();
    println!(
        "scheduled {} [{}]{deliver_note} — runs | cancel: pantheon schedule cancel {}",
        job_id,
        format_kind(&kind),
        job_id
    );
}

/// `pantheon schedule template list`: built-ins plus the user's
/// `<data_dir>/templates/*.toml` overlay.
fn cmd_schedule_template(args: &[String], data_dir: &Path) {
    if args.first().map(String::as_str) != Some("list") {
        eprintln!("usage: pantheon schedule template list");
        std::process::exit(2);
    }
    let store = TemplateStore::load(data_dir);
    for t in store.list() {
        let sched = match &t.schedule {
            TemplateSchedule::Every(d) => format!("every {d}"),
            TemplateSchedule::Cron(e) => format!("cron '{e}'"),
        };
        let vars = if t.vars.is_empty() {
            String::new()
        } else {
            format!(
                "  vars: {}",
                t.vars
                    .iter()
                    .map(|v| v.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        println!("{}\n  {}  ({sched}){vars}", t.name, t.description);
    }
}

/// `pantheon schedule consolidate [--every 30m | --cron '0 3 * * *']`:
/// a nightly consolidation pass. Registers a normal stored job whose
/// task is the consolidate marker; `run_job_now` runs the pass directly
/// instead of a chat turn. Refuses when `[consolidation] enabled = false`
/// — a disabled feature must not have a job lying in wait.
fn cmd_schedule_consolidate(args: &[String], data_dir: &Path) {
    match build_consolidate_job(data_dir, args) {
        Err(e) => {
            eprintln!("error: {e}");
            eprintln!("not scheduled: fix the arguments and retry");
            std::process::exit(2);
        }
        Ok(stored) => {
            let job_id = stored.id.clone();
            let mut jobs = load_or_exit(data_dir);
            jobs.push(stored);
            if let Err(e) = save_jobs(data_dir, &jobs) {
                eprintln!("save failed: {e}");
                std::process::exit(1);
            }
            println!(
                "scheduled consolidation {job_id} — runs | cancel: pantheon schedule cancel {job_id}"
            );
        }
    }
}

/// Testable core of `pantheon schedule consolidate`: the enabled check
/// and the default-cron resolution, without the persistence.
pub fn build_consolidate_job(data_dir: &Path, args: &[String]) -> Result<StoredJob, String> {
    let file_cfg = crate::config::Config::load_or_report(data_dir);
    let cfg = crate::config::consolidation_config(file_cfg.as_ref());
    if !cfg.enabled {
        return Err("consolidation is disabled ([consolidation] enabled = false); enable it before scheduling a pass".into());
    }
    let mut parsed = parse_create_args(args);
    if !parsed.task.is_empty() {
        return Err(
            "`schedule consolidate` takes no task text: it always runs a consolidation pass".into(),
        );
    }
    // No schedule given = the `[consolidation] cron` value (default
    // "0 3 * * *").
    if parsed.every.is_none() && parsed.cron.is_none() {
        parsed.cron = Some(cfg.cron.clone());
    }
    parsed.task = CONSOLIDATE_TASK_MARKER.to_string();
    build_job_from_create(&parsed)
}

/// Build the stored job from `schedule create` arguments, validating
/// everything before anything is persisted.
///
/// A bad cron expression used to be stored without a murmur and then
/// silently never fire. Now it is rejected here, with the field and the
/// reason, so a broken schedule is a loud error at registration instead
/// of a quiet no-show at 3am.
/// Marker task for reflection jobs. `run_job_now` intercepts it and runs a
/// bounded reflection pass instead of a chat turn — a scheduled
/// reflection job never spends an agent turn.
pub const REFLECT_TASK_MARKER: &str = "__pantheon_reflect__";

/// Marker task for consolidation jobs. `run_job_now` intercepts it and
/// runs a bounded consolidation pass instead of a chat turn — a
/// scheduled consolidation job never spends an agent turn.
pub const CONSOLIDATE_TASK_MARKER: &str = "__pantheon_consolidate__";

/// Build the stored job for `schedule <task>` / `schedule create`, resolving
/// `--template` first: the template's rendered prompt becomes the task and
/// its default schedule fills in when no `--every`/`--cron` is given. A
/// positional task always overrides the template prompt.
fn build_create_job(args: &[String], data_dir: &Path) -> Result<StoredJob, String> {
    let mut parsed = parse_create_args(args);
    if let Some(tname) = parsed.template.clone() {
        let store = TemplateStore::load(data_dir);
        let t = store.get(&tname).ok_or_else(|| {
            format!("unknown template '{tname}'; see `pantheon schedule template list`")
        })?;
        let mut vars: HashMap<String, String> = parsed.vars.iter().cloned().collect();
        // Reserved vars become the job's model pin, not prompt text. An
        // explicit --model/--provider flag wins over --var.
        for reserved in ["model", "provider"] {
            if let Some(v) = vars.remove(reserved) {
                if v.trim().is_empty() {
                    continue;
                }
                if reserved == "model" && parsed.model.is_none() {
                    parsed.model = Some(v);
                } else if reserved == "provider" && parsed.provider.is_none() {
                    parsed.provider = Some(v);
                }
            }
        }
        apply_defaults(t, &mut vars);
        for v in &t.vars {
            let current = vars.get(&v.name).map(String::as_str).unwrap_or("");
            // A default (even "") satisfies the var without prompting.
            if !current.is_empty() || v.default.is_some() {
                continue;
            }
            let answer = prompt_for_var(&v.question, None)
                .map_err(|_| format!("template '{tname}' needs --var {}=<value>", v.name))?;
            if answer.is_empty() {
                return Err(format!("template '{tname}' needs --var {}=<value>", v.name));
            }
            vars.insert(v.name.clone(), answer);
        }
        let rendered = render_prompt(t, &vars)?;
        if parsed.task.is_empty() {
            parsed.task = rendered;
        }
        if parsed.every.is_none() && parsed.cron.is_none() {
            match &t.schedule {
                TemplateSchedule::Every(d) => parsed.every = Some(d.clone()),
                TemplateSchedule::Cron(e) => parsed.cron = Some(e.clone()),
            }
        }
    }
    if parsed.task.is_empty() {
        return Err(
            "need a task: pantheon schedule <task> --every 30m|--cron '0 9 * * *' (or --template <name>)"
                .into(),
        );
    }
    // Validate the delivery target now: a typo must fail here, not vanish
    // into a job that never delivers.
    if let Some(d) = &parsed.deliver {
        Deliver::parse(d)?;
    }
    let stored = build_job_from_create(&parsed)?;
    Ok(stored)
}

/// Ask the operator for a template variable. Uses the default on empty
/// input; errors when stdin is not a TTY (scripts must pass --var).
fn prompt_for_var(question: &str, default: Option<&str>) -> Result<String, String> {
    use std::io::{BufRead, IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return Err("no TTY to prompt on".into());
    }
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match default {
        Some(d) if !d.is_empty() => write!(out, "{question} [{d}]: "),
        _ => write!(out, "{question}: "),
    }
    .map_err(|e| format!("prompt: {e}"))?;
    out.flush().map_err(|e| format!("prompt: {e}"))?;
    drop(out);
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| format!("prompt: {e}"))?;
    Ok(line.trim().to_string())
}

/// `pantheon schedule reflect (--every 30m | --cron '0 9 * * *')`: a
/// scheduled reflection pass (e.g. nightly). Registers a normal stored
/// job whose task is the reflect marker; `run_job_now` runs the pass
/// directly instead of a chat turn.
fn cmd_schedule_reflect(args: &[String], data_dir: &Path) {
    let mut parsed = parse_create_args(args);
    if !parsed.task.is_empty() {
        eprintln!("usage: pantheon schedule reflect (--every 30m | --cron '0 9 * * *') [--timeout 10m] [--overlap skip|replace|queue]");
        eprintln!("`schedule reflect` takes no task text: it always runs a reflection pass");
        std::process::exit(2);
    }
    parsed.task = REFLECT_TASK_MARKER.to_string();
    match build_job_from_create(&parsed) {
        Err(e) => {
            eprintln!("error: {e}");
            eprintln!("not scheduled: fix the arguments and retry");
            std::process::exit(2);
        }
        Ok(stored) => {
            let job_id = stored.id.clone();
            let mut jobs = load_or_exit(data_dir);
            jobs.push(stored);
            if let Err(e) = save_jobs(data_dir, &jobs) {
                eprintln!("save failed: {e}");
                std::process::exit(1);
            }
            println!(
                "scheduled reflection {job_id} — runs | cancel: pantheon schedule cancel {job_id}"
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
fn build_job_from_create(parsed: &CreateArgs) -> Result<StoredJob, String> {
    let task = parsed.task.clone();
    let every = parsed.every.clone();
    let agent = parsed.agent.clone();
    let cron = parsed.cron.clone();
    let model = parsed.model.clone();
    let provider = parsed.provider.clone();
    let timeout = parsed.timeout.clone();
    let overlap = parsed.overlap.clone();
    let deliver = parsed.deliver.clone();

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
        deliver,
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
                let deliver = j
                    .deliver
                    .as_deref()
                    .map(|d| format!("  deliver: {d}"))
                    .unwrap_or_default();
                println!(
                    "{}  {}  {}  [{}]{}{}  last: {}  | cancel: pantheon schedule cancel {}",
                    // Full id, not a byte-prefix: ids are `job_run_<ts>_<n>`,
                    // so the first 8 bytes are the shared `job_run_` and
                    // the truncated form was identical for every job.
                    j.id.as_str(),
                    status,
                    format_kind(&j.kind),
                    j.task,
                    pin,
                    deliver,
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
                    if let Err(e) = run_job_now(&j.task, &Job::from(j.clone()), data_dir) {
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
            // This shares the gateway service's scheduler loop
            // (pantheon_gateway::scheduler::SchedulerLoop): same claim
            // ledger, same job store, so the two can never double-fire.
            // Every fire goes through the durable claim ledger first: the
            // claim is an atomic first-wins INSERT, so two ticks racing the
            // same due job — two threads, two processes, or a restart
            // replaying a minute — agree on exactly one winner instead of
            // double-executing. A claim that cannot be persisted fails
            // closed: the run does not start.
            let watch = parts.iter().any(|a| a == "--watch");
            let sched = match SchedulerLoop::open(data_dir, 30) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("tick: {e}");
                    eprintln!("tick: refusing to fire without durable claims");
                    std::process::exit(1);
                }
            };
            let dd = data_dir.to_path_buf();
            let execute: ExecuteFn = Arc::new(move |j: &SchedulableJob| {
                if let Err(e) = run_job_now(&j.task, &j.job, &dd) {
                    eprintln!("{e}");
                }
            });
            loop {
                // Automatic retention: at most one prune pass per day,
                // so --watch daemons keep the ledger bounded on their own.
                maybe_run_retention(data_dir);
                let jobs = match load_schedulable(data_dir) {
                    Ok(j) => j,
                    Err(e) => {
                        eprintln!("schedule: {e}");
                        eprintln!("fix: delete or repair {}", store_path(data_dir).display());
                        std::process::exit(1);
                    }
                };
                let now = now_ms();
                let reports = sched.tick_once(now, &jobs, execute.clone());
                let mut any_fired = false;
                for r in &reports {
                    match &r.outcome {
                        FireOutcome::Fired => {
                            println!("tick {now}: fired {}", r.id);
                            any_fired = true;
                        }
                        FireOutcome::Queued => {
                            println!("tick {now}: {} still running, queued (overlap=queue)", r.id);
                            any_fired = true;
                        }
                        FireOutcome::Skipped(reason) => {
                            println!("tick {now}: {} {reason}", r.id);
                        }
                    }
                }
                if !any_fired {
                    println!("tick {now}: nothing due");
                }
                record_fires(data_dir, &reports, now);
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
    deliver: Option<String>,
    template: Option<String>,
    vars: Vec<(String, String)>,
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
    let mut deliver = None;
    let mut template = None;
    let mut vars: Vec<(String, String)> = Vec::new();
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
            "--deliver" => {
                i += 1;
                if i < args.len() {
                    deliver = Some(args[i].clone());
                }
            }
            "--template" => {
                i += 1;
                if i < args.len() {
                    template = Some(args[i].clone());
                }
            }
            "--var" => {
                i += 1;
                if i < args.len() {
                    let kv = &args[i];
                    match kv.split_once('=') {
                        Some((k, v)) => vars.push((k.to_string(), v.to_string())),
                        None => vars.push((kv.clone(), String::new())),
                    }
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
        deliver,
        template,
        vars,
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
/// Execute one scheduled job: a real agent turn, or the bounded reflection
/// pass for reflection jobs. Shared by `pantheon schedule tick` and the
/// gateway service's scheduler loop — one execution core, never duplicated.
pub(crate) fn run_job_now(task: &str, sched: &Job, data_dir: &Path) -> Result<(), String> {
    // Consolidation jobs run a bounded consolidation pass directly —
    // no agent turn, no chat model. Scheduled passes are deterministic
    // unless `[consolidation] enabled = true`, in which case the pass
    // distills through the Consolidation auxiliary model. A job whose
    // marker outlived its config opt-out skips loudly instead of firing.
    if task == CONSOLIDATE_TASK_MARKER {
        let file_cfg = crate::config::Config::load_or_report(data_dir);
        let cfg = crate::config::consolidation_config(file_cfg.as_ref());
        if !cfg.enabled {
            println!("consolidation: [consolidation] enabled = false — skipping scheduled pass");
            return Ok(());
        }
        return match crate::consolidate_cli::run_one_pass(data_dir, false) {
            Ok(report) => {
                println!("{}", report.summary_line());
                Ok(())
            }
            Err(e) => Err(format!("scheduled consolidation failed: {e}")),
        };
    }
    // Reflection jobs run a bounded reflection pass directly — no agent
    // turn, no chat model. Scheduled passes are deterministic in v1 (no
    // LlmRefiner wired); `[reflect] enabled` only matters once one is.
    if task == REFLECT_TASK_MARKER {
        return match crate::reflect_cli::run_one_pass(data_dir, false) {
            Ok(out) => {
                println!("{}", crate::reflect_cli::summarize_pass(&out));
                if !out.pending.is_empty() {
                    println!(
                        "note: {} proposal(s) await approval — run `pantheon reflect pending`",
                        out.pending.len()
                    );
                }
                Ok(())
            }
            Err(e) => Err(format!("scheduled reflection failed: {e}")),
        };
    }
    use crate::config;
    use crate::config::build_scheduled_model_policy;
    use pantheon_runtime::session::Session;

    let file_cfg = config::Config::load_or_report(data_dir);
    // Unpinned jobs run on the `[scheduled]` auxiliary model, not the
    // interactive default: scheduled work is background work and should
    // burn cheap tokens. Explicit pin > template `model` var > aux.
    let model_policy = build_scheduled_model_policy(
        file_cfg.as_ref(),
        sched.provider.clone(),
        sched.model.clone(),
    );
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
    match session.chat(&run_id, task) {
        Ok(outcome) => {
            println!("ran {task} — run {run_id}");
            deliver_job_result(task, sched, data_dir, &run_id, &outcome);
            Ok(())
        }
        Err(e) => {
            // A parked run is a real outcome, not a failure: it needs a
            // grant before it can finish.
            if e.code == "RUN_PARKED" {
                // The error carries the copy-pasteable grant command with the
                // scope already inlined, so a placeholder here was worse than
                // useless: it looked actionable and was not.
                println!("parked {task}");
                println!("{e}");
                Ok(())
            } else {
                Err(format!("scheduled task failed: {e}"))
            }
        }
    }
}

/// Route a completed job's summary to its delivery target.
///
/// The summary is the run's final assistant message, redacted and
/// truncated. Delivery failure never fails the job: it is a log line, not
/// an error — the run already completed and sits in the ledger.
fn deliver_job_result(
    task: &str,
    sched: &Job,
    data_dir: &Path,
    run_id: &str,
    outcome: &pantheon_agent::LoopOutcome,
) {
    let target = match sched.deliver.as_deref() {
        // Validated at create; an unknown value here fails open to log.
        Some(s) => schedule_delivery::Deliver::parse(s).unwrap_or(schedule_delivery::Deliver::Log),
        None => return,
    };
    if target == schedule_delivery::Deliver::Log {
        return;
    }
    let summary = match outcome {
        pantheon_agent::LoopOutcome::Answered { text, .. } => Some(text.clone()),
        _ => last_assistant_text(data_dir, run_id),
    };
    let Some(text) = summary.filter(|t| !t.trim().is_empty()) else {
        return;
    };
    let body = schedule_delivery::build_summary(&text);
    if let Some(err) = schedule_delivery::deliver_best_effort(
        &schedule_delivery::RestChannelSender,
        &target,
        &short_task_label(task, &sched.id),
        &body,
    ) {
        eprintln!("delivery failed (job result kept): {err}");
    }
}

/// Readable notification header: the task's first line, capped; the job id
/// when the task has no usable first line.
fn short_task_label(task: &str, job_id: &str) -> String {
    let first: String = task
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .take(60)
        .collect();
    if first.is_empty() {
        job_id.to_string()
    } else {
        first
    }
}

/// Fallback summary source: the last non-empty assistant message in the
/// ledger, for runs that did not end in a final answer.
fn last_assistant_text(data_dir: &Path, run_id: &str) -> Option<String> {
    let ledger = pantheon_storage::Ledger::open(&data_dir.join("ledger.db")).ok()?;
    let entries = ledger.replay(run_id).ok()?;
    let messages = pantheon_runtime::session::rebuild_messages(entries);
    messages
        .iter()
        .rev()
        .find(|m| m.role == pantheon_api::message::Role::Assistant && !m.content.trim().is_empty())
        .map(|m| m.content.clone())
}

/// Load jobs as the scheduler loop sees them. Corruption is an error for
/// the caller to handle: the CLI tick exits, the service loop logs and
/// keeps going (a bad schedule.json must not take the gateway down).
pub(crate) fn load_schedulable(data_dir: &Path) -> Result<Vec<SchedulableJob>, String> {
    Ok(load_jobs(data_dir)?
        .into_iter()
        .map(|s| SchedulableJob {
            last_run: s.last_run,
            task: s.task.clone(),
            job: Job::from(s),
        })
        .collect())
}

/// Persist fire times for jobs that fired, so an interval job does not
/// come due again on the next tick. The durable claim already won is what
/// makes this crash-safe; last_run just drives the due check.
pub(crate) fn record_fires(data_dir: &Path, reports: &[TickReport], now_ms: i64) {
    let fired: Vec<&str> = reports
        .iter()
        .filter(|r| matches!(r.outcome, FireOutcome::Fired | FireOutcome::Queued))
        .map(|r| r.id.as_str())
        .collect();
    if fired.is_empty() {
        return;
    }
    match load_jobs(data_dir) {
        Ok(mut jobs) => {
            for j in jobs.iter_mut() {
                if fired.contains(&j.id.as_str()) {
                    j.last_run = Some(now_ms);
                }
            }
            if let Err(e) = save_jobs(data_dir, &jobs) {
                eprintln!("warning: could not record last_run: {e}");
            }
        }
        Err(e) => eprintln!("warning: could not record last_run: {e}"),
    }
}

/// Seconds between scheduler passes. Default 60; override with
/// `PANTHEON_SCHEDULER_TICK_SECS`.
pub(crate) fn scheduler_tick_secs() -> u64 {
    std::env::var("PANTHEON_SCHEDULER_TICK_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(60)
}

/// The gateway service's scheduler loop: ticks due jobs forever on the
/// same claim ledger and job store `pantheon schedule tick` uses.
/// Never returns; a ledger that cannot open is logged and the thread ends
/// (firing without durable claims is refused).
pub(crate) fn run_scheduler_loop(data_dir: &Path) {
    let tick_secs = scheduler_tick_secs();
    let sched = match SchedulerLoop::open(data_dir, tick_secs) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("scheduler: {e}; scheduled jobs will not fire");
            return;
        }
    };
    eprintln!("scheduler: ticking every {tick_secs}s");
    let dd = data_dir.to_path_buf();
    let execute: ExecuteFn = Arc::new(move |j: &SchedulableJob| {
        if let Err(e) = run_job_now(&j.task, &j.job, &dd) {
            eprintln!("scheduler: {e}");
        }
    });
    let dd = data_dir.to_path_buf();
    let dd2 = data_dir.to_path_buf();
    sched.run_forever(
        &move || {
            // Automatic retention, same as the CLI tick loop: at most one
            // prune pass per day, so the long-running service keeps the
            // ledger bounded on its own.
            maybe_run_retention(&dd);
            match load_schedulable(&dd) {
                Ok(jobs) => jobs,
                Err(e) => {
                    eprintln!("scheduler: {e}");
                    Vec::new()
                }
            }
        },
        execute,
        &move |reports: &[TickReport], now: i64| {
            for r in reports {
                match &r.outcome {
                    FireOutcome::Fired => println!("scheduler {now}: fired {}", r.id),
                    FireOutcome::Queued => {
                        println!("scheduler {now}: {} still running, queued", r.id)
                    }
                    FireOutcome::Skipped(_) => {}
                }
            }
            record_fires(&dd2, reports, now);
        },
    );
}
