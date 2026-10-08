//! `pantheon schedule` - durable cron/interval/one-shot task scheduling.
//!
//! Jobs live in the data dir (`schedule.json`) and are read/written through
//! the scheduler core's [`load_jobs`]/[`save_jobs`]. Occurrence idempotency
//! is handled by the scheduler's `DurableClaimLedger` over the `ClaimStore`.
//!
//! Jobs created with `--template <name>` re-render their prompt from the
//! template at fire time ([`Job::resolve_task`]), so editing the template
//! updates every job built from it. The rendered snapshot in `task` stays
//! as the fallback when the template is deleted or fails to render.

use crate::schedule_self_heal::SelfHealer;
use pantheon_gateway::schedule_delivery::{self, Deliver};
use pantheon_gateway::scheduler::{ExecuteFn, FireOutcome, SchedulerLoop, TaskOutcome, TickReport};
use pantheon_scheduler::{
    after_manual_run, expand_template, job_store_path, load_jobs, load_jobs_with_warnings,
    parse_duration, update_jobs, DurableClaimLedger, Job, OverlapPolicy, RunOutcome, ScheduleKind,
    ScheduleTemplate, ScheduledJob, TemplateSchedule, TemplateStore, TemplateVar,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Parse `--at <time>` into epoch millis:
/// - epoch millis (`1790845200000`),
/// - an RFC3339 datetime (`2026-10-01T09:00:00+01:00`),
/// - a relative offset from now (`+30m`, `+2h`, `+1d`; same units as
///   `--every`).
fn parse_at(s: &str) -> Result<i64, String> {
    let s = s.trim();
    if let Ok(ms) = s.parse::<i64>() {
        return Ok(ms);
    }
    if let Some(rest) = s.strip_prefix('+') {
        let delta = parse_duration(rest)?;
        return Ok(now_ms().saturating_add(delta as i64));
    }
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.timestamp_millis())
        .map_err(|_| {
            format!(
                "expected epoch millis, an RFC3339 datetime, or +<duration> (e.g. +30m), got {s:?}"
            )
        })
}

/// Public wrapper for the TUI's /schedule command. Returns an empty vec
/// when no schedule file exists; reports corruption as an error string.
pub fn load_jobs_public(data_dir: &Path) -> Vec<ScheduledJob> {
    load_jobs(data_dir).unwrap_or_default()
}

/// Load jobs or exit. A corrupt schedule file is a stop, not an empty list.
fn load_or_exit(data_dir: &Path) -> Vec<ScheduledJob> {
    let (result, warnings) = load_jobs_with_warnings(data_dir);
    for w in warnings {
        eprintln!("schedule: warning: {w}");
    }
    match result {
        Ok(j) => j,
        Err(e) => {
            eprintln!("schedule: {e}");
            eprintln!(
                "fix: delete or repair {}",
                job_store_path(data_dir).display()
            );
            std::process::exit(1);
        }
    }
}

/// Schedule a task to run repeatedly.
pub fn cmd_schedule(args: &[String], data_dir: &Path) {
    if args.len() < 3 {
        eprintln!("usage: pantheon schedule <task> (--every 30m | --cron '0 9 * * *' | --at <time>) [--agent nyx] [--timeout 10m] [--overlap skip|replace|queue] [--deliver telegram|discord|notify|file:<path>|log] [--model M] [--provider P]");
        eprintln!("       pantheon schedule create --template <name> [--var k=v ...] [--deliver ...] [--every ...|--cron ...|--at ...]");
        eprintln!("       pantheon schedule template list|get <name>|save|delete  - prompt templates (<data_dir>/templates.json)");
        eprintln!("       pantheon schedule nightly (--every 30m | --cron '0 3 * * *' | --at <time>)  - scheduled nightly self-improvement pass (default: [nightly] cron)");
        eprintln!("       pantheon schedule reflect ... | pantheon schedule consolidate ...  - legacy aliases for `schedule nightly`");
        eprintln!("       pantheon schedule list|pause|resume|cancel|run|tick|prune <id>");
        eprintln!();
        eprintln!("--at <time>: epoch millis, RFC3339 ('2026-10-01T09:00:00+01:00'), or relative +30m/+2h/+1d - fires once, then the job is removed");
        eprintln!("model rule: --model/--provider pin (or the template's `model` var) > the [scheduled] auxiliary model > never the interactive default.");
        eprintln!("deliver targets: log (default) | telegram (needs PANTHEON_TELEGRAM_BOT_TOKEN + PANTHEON_DELIVER_TELEGRAM_TO) | discord (needs PANTHEON_DISCORD_TOKEN + PANTHEON_DELIVER_DISCORD_TO) | notify (notify-send) | file:<path>");
        std::process::exit(2);
    }

    let subcommand = args[2].as_str();
    if subcommand == "nightly" || subcommand == "reflect" || subcommand == "consolidate" {
        cmd_schedule_nightly(&args[3..], data_dir);
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
        "list" | "pause" | "resume" | "cancel" | "run" | "tick" | "prune"
    ) {
        handle_subcommand(&args[2..], data_dir);
        return;
    }

    // Create: pantheon schedule <task> [--every|N<unit>] [--agent NAME] [--cron EXPR] [--at TIME]
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
fn persist_new_job(stored: ScheduledJob, data_dir: &Path) {
    let job_id = stored.job.id.clone();
    let kind = stored.job.kind.clone();
    let deliver = stored.job.deliver.clone();
    // Single locked rewrite (load/mutate/save under the store lock) so a
    // concurrent dashboard edit can't be clobbered by this CLI process.
    if let Err(e) = update_jobs(data_dir, |jobs| {
        jobs.push(stored);
        Ok(())
    }) {
        eprintln!("save failed: {e}");
        std::process::exit(1);
    }

    let deliver_note = deliver
        .as_deref()
        .map(|d| format!(" → {d}"))
        .unwrap_or_default();
    println!(
        "scheduled {} [{}]{deliver_note} - runs | cancel: pantheon schedule cancel {}",
        job_id,
        format_kind(&kind),
        job_id
    );
}

/// `pantheon schedule template <list|get|save|delete>`: built-ins plus the
/// user's `<data_dir>/templates.json` overlay.
fn cmd_schedule_template(args: &[String], data_dir: &Path) {
    match args.first().map(String::as_str) {
        Some("list") => {
            let store = TemplateStore::open(data_dir);
            let mut any = false;
            for t in store.list() {
                any = true;
                let sched = format_template_schedule(&t.schedule);
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
            if !any {
                println!("no templates");
            }
        }
        Some("get") => {
            let name = args.get(1).unwrap_or_else(|| {
                eprintln!("usage: pantheon schedule template get <name>");
                std::process::exit(2);
            });
            let store = TemplateStore::open(data_dir);
            let t = store.get(name).unwrap_or_else(|| {
                eprintln!("unknown template '{name}'; see `pantheon schedule template list`");
                std::process::exit(1);
            });
            let builtin = if store.is_builtin(name) {
                " (built-in)"
            } else {
                ""
            };
            println!(
                "{name}{builtin}\n  {}\n  schedule: {}",
                t.description,
                format_template_schedule(&t.schedule)
            );
            println!("  prompt:");
            for line in t.prompt.lines() {
                println!("    {line}");
            }
            if t.vars.is_empty() {
                println!("  vars: none");
            } else {
                for v in &t.vars {
                    let def = v
                        .default
                        .as_deref()
                        .map(|d| format!(" [default: {d}]"))
                        .unwrap_or_default();
                    println!("  var {}: {}{def}", v.name, v.question);
                }
            }
        }
        Some("save") => cmd_schedule_template_save(&args[1..], data_dir),
        Some("delete") => {
            let name = args.get(1).unwrap_or_else(|| {
                eprintln!("usage: pantheon schedule template delete <name>");
                std::process::exit(2);
            });
            let mut store = TemplateStore::open(data_dir);
            match store.delete(name) {
                Ok(()) => println!("deleted template '{name}'"),
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            eprintln!("usage: pantheon schedule template list");
            eprintln!("       pantheon schedule template get <name>");
            eprintln!("       pantheon schedule template save --name <name> [--desc <text>] (--every 30m | --cron '<expr>') --prompt '<text>' [--var name:question[:default] ...]");
            eprintln!("       pantheon schedule template delete <name>");
            std::process::exit(2);
        }
    }
}

fn format_template_schedule(schedule: &TemplateSchedule) -> String {
    match schedule {
        TemplateSchedule::Every(d) => format!("every {d}"),
        TemplateSchedule::Cron(e) => format!("cron '{e}'"),
    }
}

/// `pantheon schedule template save --name <n> [--desc <d>]
/// (--every 30m | --cron '<expr>') --prompt '<text>'
/// [--var name:question[:default] ...]`
///
/// A bad cron, a bad duration, or a missing required flag is a loud error
/// here - the template must be usable the moment it is saved.
fn cmd_schedule_template_save(args: &[String], data_dir: &Path) {
    let usage = "usage: pantheon schedule template save --name <name> [--desc <text>] (--every 30m | --cron '<expr>') --prompt '<text>' [--var name:question[:default] ...]";
    let mut name: Option<String> = None;
    let mut desc = String::new();
    let mut every: Option<String> = None;
    let mut cron: Option<String> = None;
    let mut prompt: Option<String> = None;
    let mut var_specs: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--name" => {
                i += 1;
                if i < args.len() {
                    name = Some(args[i].clone());
                }
            }
            "--desc" => {
                i += 1;
                if i < args.len() {
                    desc = args[i].clone();
                }
            }
            "--every" => {
                i += 1;
                if i < args.len() {
                    every = Some(args[i].clone());
                }
            }
            "--cron" => {
                i += 1;
                if i < args.len() {
                    cron = Some(args[i].clone());
                }
            }
            "--prompt" => {
                i += 1;
                if i < args.len() {
                    prompt = Some(args[i].clone());
                }
            }
            "--var" => {
                i += 1;
                if i < args.len() {
                    var_specs.push(args[i].clone());
                }
            }
            other => {
                eprintln!("unknown flag: {other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    let name = name.unwrap_or_else(|| {
        eprintln!("{usage}\nerror: --name is required");
        std::process::exit(2);
    });
    let prompt = prompt.unwrap_or_else(|| {
        eprintln!("{usage}\nerror: --prompt is required");
        std::process::exit(2);
    });
    let schedule = match (cron, every) {
        (Some(e), _) => TemplateSchedule::Cron(e),
        (None, Some(d)) => {
            if let Err(e) = parse_duration(&d) {
                eprintln!("error: bad --every: {e}");
                std::process::exit(2);
            }
            TemplateSchedule::Every(d)
        }
        (None, None) => {
            eprintln!("{usage}\nerror: need --every <duration> or --cron <expr>");
            std::process::exit(2);
        }
    };
    let mut vars = Vec::new();
    for spec in &var_specs {
        // name:question[:default]; extra colons stay in the default.
        let mut parts = spec.splitn(3, ':');
        let vname = parts.next().unwrap_or("").trim();
        let question = parts.next().unwrap_or("").trim();
        let default = parts.next().map(str::trim).filter(|d| !d.is_empty());
        if vname.is_empty() || question.is_empty() {
            eprintln!("error: bad --var {spec:?}: want name:question[:default]");
            std::process::exit(2);
        }
        vars.push(TemplateVar {
            name: vname.to_string(),
            question: question.to_string(),
            default: default.map(str::to_string),
        });
    }
    let mut store = TemplateStore::open(data_dir);
    match store.save(ScheduleTemplate {
        name: name.clone(),
        description: desc,
        schedule,
        prompt,
        vars,
    }) {
        Ok(()) => println!("saved template '{name}'"),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

/// `pantheon schedule nightly [--every 30m | --cron '0 3 * * *' | --at <time>]`:
/// a scheduled unified nightly pass. Registers a normal stored job whose
/// task is the nightly marker; `run_job_now` runs the pass directly
/// instead of a chat turn. `schedule reflect` / `schedule consolidate`
/// are legacy aliases for this command.
fn cmd_schedule_nightly(args: &[String], data_dir: &Path) {
    match build_nightly_job(data_dir, args) {
        Err(e) => {
            eprintln!("error: {e}");
            eprintln!("not scheduled: fix the arguments and retry");
            std::process::exit(2);
        }
        Ok(stored) => {
            let job_id = stored.job.id.clone();
            // Single locked rewrite (load/mutate/save under the store lock)
            // so a concurrent dashboard edit can't be clobbered.
            if let Err(e) = update_jobs(data_dir, |jobs| {
                jobs.push(stored);
                Ok(())
            }) {
                eprintln!("save failed: {e}");
                std::process::exit(1);
            }
            println!(
                "scheduled nightly {job_id} - runs | cancel: pantheon schedule cancel {job_id}"
            );
        }
    }
}

/// Testable core of `pantheon schedule nightly`: the default-cron
/// resolution, without the persistence.
pub fn build_nightly_job(data_dir: &Path, args: &[String]) -> Result<ScheduledJob, String> {
    let file_cfg = crate::config::Config::load_or_report(data_dir);
    let mut parsed = parse_create_args(args);
    if !parsed.task.is_empty() {
        return Err("`schedule nightly` takes no task text: it always runs a nightly pass".into());
    }
    // No schedule given = the `[nightly] cron` value (default
    // "0 3 * * *").
    if parsed.every.is_none() && parsed.cron.is_none() && parsed.at.is_none() {
        parsed.cron = Some(crate::config::nightly_cron(file_cfg.as_ref()));
    }
    parsed.task = NIGHTLY_TASK_MARKER.to_string();
    build_job_from_create(&parsed)
}

/// Marker task for nightly jobs. `run_job_now` intercepts it and runs
/// the unified nightly pass instead of a chat turn - a scheduled
/// nightly job never spends an agent turn. Re-exported from
/// [`pantheon_scheduler`] so every client shares one marker string.
pub use pantheon_scheduler::NIGHTLY_TASK_MARKER;

/// Marker task for reflection jobs (legacy). `run_job_now` intercepts it
/// and runs the unified nightly pass instead of a chat turn.
pub const REFLECT_TASK_MARKER: &str = "__pantheon_reflect__";

/// Marker task for consolidation jobs (legacy). `run_job_now`
/// intercepts it and runs the unified nightly pass instead of a chat
/// turn.
pub const CONSOLIDATE_TASK_MARKER: &str = "__pantheon_consolidate__";

/// Build the stored job for `schedule <task>` / `schedule create`, resolving
/// `--template` first: the template's rendered prompt becomes the task and
/// its default schedule fills in when no `--every`/`--cron`/`--at` is given.
/// A positional task always overrides the template prompt.
fn build_create_job(args: &[String], data_dir: &Path) -> Result<ScheduledJob, String> {
    let mut parsed = parse_create_args(args);
    if let Some(tname) = parsed.template.clone() {
        let store = TemplateStore::open(data_dir);
        let t = store.get(&tname).ok_or_else(|| {
            format!("unknown template '{tname}'; see `pantheon schedule template list`")
        })?;
        let mut vars: HashMap<String, String> = parsed.vars.iter().cloned().collect();
        let has_schedule = parsed.every.is_some() || parsed.cron.is_some() || parsed.at.is_some();
        // Interactive missing-var resolution: prompt on a TTY, fail loud
        // otherwise. An empty answer is an error, like before.
        let mut prompt = |name: &str, question: &str| -> Result<String, String> {
            let answer = prompt_for_var(question, None)
                .map_err(|_| format!("template '{tname}' needs --var {name}=<value>"))?;
            if answer.is_empty() {
                return Err(format!("template '{tname}' needs --var {name}=<value>"));
            }
            Ok(answer)
        };
        expand_template(
            t,
            pantheon_scheduler::templates::TemplateParams {
                vars: &mut vars,
                model: &mut parsed.model,
                provider: &mut parsed.provider,
                task: &mut parsed.task,
                every: &mut parsed.every,
                cron: &mut parsed.cron,
            },
            has_schedule,
            &mut prompt,
        )?;
        // Remember the template on the job: at fire time the prompt is
        // re-rendered from it, so editing the template updates the job.
        // `task` stays as the snapshot for when the template is later
        // deleted or fails to render.
        parsed.template = Some(tname);
        parsed.template_vars = vars;
    }
    if parsed.task.is_empty() {
        return Err(
            "need a task: pantheon schedule <task> --every 30m|--cron '0 9 * * *'|--at <time> (or --template <name>)"
                .into(),
        );
    }
    build_job_from_create(&parsed)
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

/// Build the stored job from `schedule create` arguments, validating
/// everything before anything is persisted.
///
/// A bad cron expression used to be stored without a murmur and then
/// silently never fire. Now it is rejected here, with the field and the
/// reason, so a broken schedule is a loud error at registration instead
/// of a quiet no-show at 3am.
fn build_job_from_create(parsed: &CreateArgs) -> Result<ScheduledJob, String> {
    // Kind precedence: --cron > --every > --at. One of them is required:
    // a job with no schedule could never fire and must not be stored.
    let kind = if let Some(expr) = &parsed.cron {
        ScheduleKind::Cron { expr: expr.clone() }
    } else if let Some(dur) = &parsed.every {
        match parse_duration(dur) {
            Ok(ms) => ScheduleKind::Interval { every_ms: ms },
            Err(e) => return Err(format!("bad --every: {e}")),
        }
    } else if let Some(at) = &parsed.at {
        match parse_at(at) {
            Ok(at_ms) => ScheduleKind::OneShot { at_ms },
            Err(e) => return Err(format!("bad --at: {e}")),
        }
    } else {
        return Err("need --every <duration>, --cron <expr>, or --at <time>".into());
    };

    // Validate the delivery target now: a typo must fail here, not vanish
    // into a job that never delivers.
    if let Some(d) = &parsed.deliver {
        Deliver::parse(d).map_err(|e| format!("bad --deliver: {e}"))?;
    }

    let job_id = format!("job_{}", pantheon_runtime::new_run_id());
    let mut job = Job::new(&job_id, kind, &parsed.agent);
    job.task = parsed.task.clone();
    job.template = parsed.template.clone();
    job.template_vars = parsed.template_vars.clone();
    // Registration-time validation: reject the broken expression now.
    job.validate()
        .map_err(|e| format!("invalid --cron expression: {e}"))?;
    if let Some(m) = parsed.model.as_deref() {
        job.pin_model(m, parsed.provider.as_deref())
            .map_err(|e| format!("bad pin: {e}"))?;
    } else if parsed.provider.is_some() {
        return Err("--provider without --model pins nothing; add --model to pin".into());
    }
    job.timeout_secs = match parsed.timeout.as_deref() {
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
    job.overlap = match parsed.overlap.as_deref() {
        None => OverlapPolicy::default(),
        Some(o) => o
            .parse::<OverlapPolicy>()
            .map_err(|e| format!("bad --overlap: {e}"))?,
    };
    job.deliver = parsed.deliver.clone();
    Ok(ScheduledJob {
        job,
        last_run: None,
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
                let job = &j.job;
                let status = if job.paused { "paused" } else { "active" };
                let pin = match (&job.model, &job.provider) {
                    (Some(m), Some(p)) => format!("  model: {m} via {p}"),
                    (Some(m), None) => format!("  model: {m}"),
                    _ => String::new(),
                };
                let deliver = job
                    .deliver
                    .as_deref()
                    .map(|d| format!("  deliver: {d}"))
                    .unwrap_or_default();
                let template = job
                    .template
                    .as_deref()
                    .map(|t| format!("  template: {t}"))
                    .unwrap_or_default();
                println!(
                    "{}  {}  {}  [{}]{}{}{}  last: {}  | cancel: pantheon schedule cancel {}",
                    // Full id, not a byte-prefix: ids are `job_run_<ts>_<n>`,
                    // so the first 8 bytes are the shared `job_run_` and
                    // the truncated form was identical for every job.
                    job.id.as_str(),
                    status,
                    format_kind(&job.kind),
                    job.task,
                    pin,
                    deliver,
                    template,
                    format_last(j.last_run),
                    job.id
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
            // Single locked rewrite (load/mutate/save under the store lock)
            // so a concurrent dashboard edit can't be clobbered.
            let outcome = update_jobs(data_dir, |jobs| -> Result<bool, String> {
                let Some(pos) = jobs.iter().position(|j| j.job.id == *id) else {
                    return Ok(false);
                };
                match parts[0].as_str() {
                    "pause" => jobs[pos].job.paused = true,
                    "resume" => jobs[pos].job.paused = false,
                    // "cancel" removes; the outer guard allows only
                    // pause|resume|cancel here.
                    _ => {
                        jobs.remove(pos);
                    }
                }
                Ok(true)
            });
            match outcome {
                Ok(true) => println!("{} {}", parts[0], id),
                Ok(false) => {
                    eprintln!("not found: {id}");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("schedule: {e}");
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
            match jobs.iter().find(|j| j.job.id == *id) {
                Some(j) => {
                    // The prompt is re-rendered from the template at fire
                    // time, so edits to the template apply to already
                    // scheduled jobs.
                    let is_oneshot = matches!(j.job.kind, ScheduleKind::OneShot { .. });
                    let task = j.job.resolve_task(&TemplateStore::open(data_dir));
                    // A manual `run` keeps the old hard-fail behavior: the
                    // tick loop instead logs and continues with other jobs.
                    let report = run_job_now(&task, &j.job, data_dir);
                    if let Some(e) = report.error {
                        eprintln!("{e}");
                        std::process::exit(1);
                    }
                    // stays for retry. Single locked rewrite so a concurrent
                    // dashboard edit can't be clobbered.
                    if let Err(e) = update_jobs(data_dir, |jobs| {
                        after_manual_run(jobs, id, now_ms());
                        Ok(())
                    }) {
                        eprintln!("warning: could not update job store after run: {e}");
                    } else if is_oneshot {
                        println!("removed one-shot job {id}");
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
            // same due job - two threads, two processes, or a restart
            // replaying a minute - agree on exactly one winner instead of
            // double-executing. A claim that cannot be persisted fails
            // closed: the run does not start.
            let watch = parts.iter().any(|a| a == "--watch");
            let mut sched = match SchedulerLoop::open(data_dir, 30) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("tick: {e}");
                    eprintln!("tick: refusing to fire without durable claims");
                    std::process::exit(1);
                }
            };
            // Record every fired run's outcome and self-heal the failed
            // ones: investigate, retry once when fixed, alert always.
            let healer = SelfHealer::new(data_dir);
            sched.set_outcome_sink(healer.outcome_sink());
            install_scheduler_hooks(&mut sched, data_dir);
            let dd = data_dir.to_path_buf();
            let execute: ExecuteFn = Arc::new(move |j: &ScheduledJob| {
                let task = j.job.resolve_task(&TemplateStore::open(&dd));
                let report = run_job_now(&task, &j.job, &dd);
                if let Some(e) = &report.error {
                    eprintln!("{e}");
                }
                TaskOutcome {
                    run_id: report.run_id,
                    error: report.error,
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
                        eprintln!(
                            "fix: delete or repair {}",
                            job_store_path(data_dir).display()
                        );
                        std::process::exit(1);
                    }
                };
                let now = now_ms();
                let (reports, watchers) = sched.tick_once(now, &jobs, execute.clone());
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
                    // One-shot tick waits for the fired runs' watchers:
                    // the watcher threads run the outcome sink (run
                    // history + self-heal) and the process must not exit
                    // before they finish. Bounded by the jobs' timeouts.
                    for w in watchers {
                        let _ = w.join();
                    }
                    return;
                }
                std::thread::sleep(std::time::Duration::from_secs(30));
            }
        }
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
            std::process::exit(2);
        }
    }
}

/// How often the automatic retention pass may run. The tick loop calls
/// `maybe_run_retention` every iteration (every 30s in `--watch` mode);
/// the gate below keeps a long-running daemon to one pass per day.
const RETENTION_INTERVAL_MS: i64 = 24 * 60 * 60 * 1000;

/// What one retention pass removed. Returned so the caller can log it
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
/// terminal are never pruned, however old their events - the active run's
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
/// disabled (`keep_days = 0`) or the last pass is fresh. Logs what
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

/// What `schedule <task> ...` parsed out of the command line. Named because
/// a six-element tuple gave no clue what any position meant at the call
/// site.
struct CreateArgs {
    task: String,
    every: Option<String>,
    agent: String,
    cron: Option<String>,
    at: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    timeout: Option<String>,
    overlap: Option<String>,
    deliver: Option<String>,
    template: Option<String>,
    template_vars: HashMap<String, String>,
    vars: Vec<(String, String)>,
}

fn parse_create_args(args: &[String]) -> CreateArgs {
    let mut task = String::new();
    let mut every = None;
    let mut agent = "nyx".to_string();
    let mut cron = None;
    let mut at = None;
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
                    agent = args[i].clone();
                }
            }
            "--cron" => {
                i += 1;
                if i < args.len() {
                    cron = Some(args[i].clone());
                }
            }
            "--at" => {
                i += 1;
                if i < args.len() {
                    at = Some(args[i].clone());
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
        at,
        model,
        provider,
        timeout,
        overlap,
        deliver,
        template,
        template_vars: HashMap::new(),
        vars,
    }
}

fn format_kind(kind: &ScheduleKind) -> String {
    match kind {
        ScheduleKind::Interval { every_ms } => format!("every {every_ms}ms"),
        ScheduleKind::Cron { expr } => format!("cron {expr}"),
        ScheduleKind::OneShot { at_ms } => match chrono::DateTime::from_timestamp_millis(*at_ms) {
            Some(t) => format!("at {}", t.to_rfc3339()),
            None => format!("at {at_ms}ms"),
        },
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
/// What one scheduled job's execution produced: the task-level truth.
/// `error` is `None` when the task ran cleanly; `Some` when the task
/// itself failed (or never started, in which case `run_id` is empty).
/// The outcome sink maps `Some` to [`RunOutcome::Failed`] for run
/// history, which feeds both the nightly repair loop and self-heal.
pub(crate) struct JobRunReport {
    pub run_id: String,
    pub error: Option<String>,
}

/// the error themselves.
/// Execute one scheduled job: a real agent turn, or the bounded nightly
/// pass for nightly jobs. Shared by `pantheon schedule tick` and the
/// gateway service's scheduler loop - one execution core, never duplicated.
pub(crate) fn run_job_now(task: &str, sched: &Job, data_dir: &Path) -> JobRunReport {
    // Nightly jobs (the new `__pantheon_nightly__` marker and both old
    // `__pantheon_reflect__` / `__pantheon_consolidate__` markers) run the
    // unified nightly pass directly - no agent turn, no chat model. The
    // pass is off by default; a disabled pass refuses the run loudly via
    // the `run_one_pass` master gate, so a stale schedule entry can never
    // silently do nothing - or silently spend model calls.
    if task == NIGHTLY_TASK_MARKER || task == CONSOLIDATE_TASK_MARKER || task == REFLECT_TASK_MARKER
    {
        return match crate::nightly_cli::run_one_pass(data_dir, false) {
            Ok(out) => {
                println!("{}", crate::nightly_cli::summarize_pass(&out));
                if out.pending > 0 {
                    println!(
                        "note: {} proposal(s) await approval - run `pantheon nightly pending`",
                        out.pending
                    );
                }
                JobRunReport {
                    run_id: String::new(),
                    error: None,
                }
            }
            Err(e) => JobRunReport {
                run_id: String::new(),
                error: Some(format!("scheduled nightly pass failed: {e}")),
            },
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
        Err(e) => {
            return JobRunReport {
                run_id: String::new(),
                error: Some(format!("open session: {e}")),
            };
        }
    };
    config::apply_tool_enablement(&session, file_cfg.as_ref());

    let run_id = pantheon_runtime::new_run_id();
    match session.chat(&run_id, task) {
        Ok(outcome) => {
            println!("ran {task} - run {run_id}");
            deliver_job_result(task, sched, data_dir, &run_id, &outcome);
            JobRunReport {
                run_id,
                error: None,
            }
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
                JobRunReport {
                    run_id,
                    error: None,
                }
            } else {
                JobRunReport {
                    run_id,
                    error: Some(format!("scheduled task failed: {e}")),
                }
            }
        }
    }
}

/// Route a completed job's summary to its delivery target.
///
/// The summary is the run's final assistant message, redacted and
/// truncated. Delivery failure never fails the job: it is a log line, not
/// an error - the run already completed and sits in the ledger.
fn deliver_job_result(
    task: &str,
    sched: &Job,
    data_dir: &Path,
    run_id: &str,
    outcome: &pantheon_agent::LoopOutcome,
) {
    let raw = sched.deliver.as_deref();
    // The home session is the default delivery target: no explicit target
    // and `--deliver mobile` both land in the pinned session the user
    // actually opens. `Deliver::parse` itself is untouched.
    if schedule_delivery::routes_via_home_session(raw) {
        deliver_job_result_home(task, sched, data_dir, run_id, outcome);
        return;
    }
    let target = match raw {
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

/// Route a completed job's summary into the home session: the default
/// delivery target (and the `--deliver mobile` target). The summary is the
/// run's final assistant message, redacted and truncated, exactly like the
/// channel path above. Delivery failure never fails the job.
fn deliver_job_result_home(
    task: &str,
    sched: &Job,
    data_dir: &Path,
    run_id: &str,
    outcome: &pantheon_agent::LoopOutcome,
) {
    let summary = match outcome {
        pantheon_agent::LoopOutcome::Answered { text, .. } => Some(text.clone()),
        _ => last_assistant_text(data_dir, run_id),
    };
    let Some(text) = summary.filter(|t| !t.trim().is_empty()) else {
        return;
    };
    let body = schedule_delivery::build_summary(&text);
    if let Some(err) = schedule_delivery::deliver_to_home_session(
        data_dir,
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

/// Load jobs as the scheduler loop sees them. This is the core's shared
/// shape already, so it is a straight pass-through. Corruption is an error
/// for the caller to handle: the CLI tick exits, the service loop logs and
/// keeps going (a bad schedule.json must not take the gateway down).
pub(crate) fn load_schedulable(data_dir: &Path) -> Result<Vec<ScheduledJob>, String> {
    load_jobs(data_dir)
}

/// Persist fire times for jobs that fired, so an interval job does not
/// come due again on the next tick. The durable claim already won is what
/// makes this crash-safe; last_run just drives the due check.
///
/// Only [`FireOutcome::Fired`] stamps `last_run`: a fire means the claim
/// was won and the run started. [`FireOutcome::Queued`] is a deferred
/// duplicate of an already-fired occurrence - stamping `last_run` there
/// would reset the interval clock before anything ran.
///
/// One-shot rows are NOT removed here. They are removed by the one-shot
/// completion observer (installed via
/// [`SchedulerLoop::set_oneshot_sink`]) only when the run actually
/// completes: deleting on fire would lose panicked/timed-out runs
/// silently. A one-shot with `last_run` set is inert (`Job::due` is
/// false), so keeping the row can never cause a refire.
///
/// The load/mutate/save runs under the store's exclusive lock
/// ([`update_jobs`]) so a concurrent dashboard/CLI edit can't be
/// clobbered by this pass's stale read.
pub(crate) fn record_fires(data_dir: &Path, reports: &[TickReport], now_ms: i64) {
    let fired: Vec<&str> = reports
        .iter()
        .filter(|r| matches!(r.outcome, FireOutcome::Fired))
        .map(|r| r.id.as_str())
        .collect();
    if fired.is_empty() {
        return;
    }
    if let Err(e) = update_jobs(data_dir, |jobs| {
        for j in jobs.iter_mut() {
            if fired.contains(&j.job.id.as_str()) {
                j.last_run = Some(now_ms);
            }
        }
        Ok(())
    }) {
        eprintln!("warning: could not record last_run: {e}");
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

/// Install the tick driver's safety hooks on a freshly opened loop:
/// - a paused-state predicate, so a queued drain re-checks paused fresh
///   from the store instead of trusting the snapshot taken at fire time;
/// - a one-shot completion observer, so one-shot rows are removed only
///   when the run actually completed - never on panic/timeout.
fn install_scheduler_hooks(sched: &mut SchedulerLoop, data_dir: &Path) {
    let dd = data_dir.to_path_buf();
    sched.set_paused_check(Arc::new(move |id: &str| {
        match load_jobs(&dd) {
            Ok(jobs) => jobs.iter().any(|j| j.job.id == id && j.job.paused),
            // Fail closed: if the store can't be read, don't fire blind.
            Err(_) => true,
        }
    }));
    let dd = data_dir.to_path_buf();
    sched.set_oneshot_sink(Arc::new(
        move |id: &str, outcome: RunOutcome, _task: Option<TaskOutcome>| {
            if outcome != RunOutcome::Completed {
                eprintln!(
                "scheduler: one-shot job {id} ended as {outcome:?}; keeping the row for inspection"
            );
                return;
            }
            let id_owned = id.to_string();
            if let Err(e) = update_jobs(&dd, |jobs| {
                jobs.retain(|j| {
                    !(j.job.id == id_owned && matches!(j.job.kind, ScheduleKind::OneShot { .. }))
                });
                Ok(())
            }) {
                eprintln!("scheduler: could not remove completed one-shot job {id}: {e}");
            }
        },
    ));
}

/// The gateway service's scheduler loop: ticks due jobs forever on the
/// same claim ledger and job store `pantheon schedule tick` uses.
/// Never returns; a ledger that cannot open is logged and the thread ends
/// (firing without durable claims is refused).
pub(crate) fn run_scheduler_loop(data_dir: &Path) {
    let tick_secs = scheduler_tick_secs();
    let mut sched = match SchedulerLoop::open(data_dir, tick_secs) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("scheduler: {e}; scheduled jobs will not fire");
            return;
        }
    };
    // Record every fired run's outcome and self-heal the failed ones:
    // investigate, retry once when fixed, alert always.
    let healer = SelfHealer::new(data_dir);
    sched.set_outcome_sink(healer.outcome_sink());
    install_scheduler_hooks(&mut sched, data_dir);
    eprintln!("scheduler: ticking every {tick_secs}s");
    let dd = data_dir.to_path_buf();
    let execute: ExecuteFn = Arc::new(move |j: &ScheduledJob| {
        // Re-render the template at fire time: edits to the template
        // apply to already-scheduled jobs.
        let task = j.job.resolve_task(&TemplateStore::open(&dd));
        let report = run_job_now(&task, &j.job, &dd);
        if let Some(e) = &report.error {
            eprintln!("scheduler: {e}");
        }
        TaskOutcome {
            run_id: report.run_id,
            error: report.error,
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
