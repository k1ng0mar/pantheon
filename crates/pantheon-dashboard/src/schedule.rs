//! Schedule CRUD over the real scheduler store.
//!
//! Jobs live in `<data_dir>/schedule.json` — the same file the
//! `pantheon schedule` CLI reads and writes. The JSON shape below mirrors
//! `pantheon-tui`'s `StoredJob` (its documented "Shape B" contract); all
//! validation reuses `pantheon-scheduler` itself: [`Job::validate`],
//! [`Job::pin_model`], [`cron::validate`], and the template machinery
//! ([`TemplateStore`], [`apply_defaults`], [`render_prompt`]).
//!
//! Triggering a run does not reimplement the runner: it spawns the real
//! `pantheon schedule run <id>` CLI path in the background.

use crate::server::{Request, Response};
use crate::{bad_json, body_json, err_json, json_ok, spawn_pantheon, App};
use pantheon_scheduler::{
    apply_defaults, cron, is_reserved_var, render_prompt, Job, MissedPolicy, OverlapPolicy,
    ScheduleKind, TemplateSchedule, TemplateStore,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// File-format mirror of `pantheon-tui`'s `StoredJob`. The JSON on disk is
/// the contract; this struct must stay field-compatible with it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub overlap: OverlapPolicy,
    #[serde(default)]
    pub deliver: Option<String>,
}

impl From<&StoredJob> for Job {
    fn from(s: &StoredJob) -> Self {
        Self {
            id: s.id.clone(),
            kind: s.kind.clone(),
            idempotency_key: format!("job:{}:", s.id),
            missed: s.missed.clone(),
            paused: s.paused,
            target_agent: s.agent.clone().unwrap_or_else(|| "nyx".into()),
            model: s.model.clone(),
            provider: s.provider.clone(),
            timeout_secs: s.timeout_secs,
            overlap: s.overlap.clone(),
            deliver: s.deliver.clone(),
        }
    }
}

fn store_path(data_dir: &Path) -> PathBuf {
    data_dir.join("schedule.json")
}

/// Read the job store. Missing file = no jobs. A corrupt file is an
/// error (the CLI refuses to run on one; the dashboard must not silently
/// present it as empty).
pub fn read_jobs(data_dir: &Path) -> Result<Vec<StoredJob>, String> {
    let path = store_path(data_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    serde_json::from_str(&text).map_err(|e| format!("{} is corrupt: {e}", path.display()))
}

fn write_jobs(data_dir: &Path, jobs: &[StoredJob]) -> Result<(), String> {
    let path = store_path(data_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_string_pretty(jobs).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(())
}

/// `<n><unit>` durations (`30s`, `15m`, `2h`, `7d`), like the CLI's
/// `--every`. CLI arg parsing lives in the TUI; this is the same shape.
fn parse_duration(s: &str) -> Result<u64, String> {
    let t = s.trim().to_lowercase();
    let (num, unit) = t.split_at(t.len().saturating_sub(1));
    let n: u64 = num
        .trim()
        .parse()
        .map_err(|_| format!("'{s}' is not a duration like 30m, 2h, 1d"))?;
    let ms = match unit {
        "s" => n * 1000,
        "m" => n * 60_000,
        "h" => n * 3_600_000,
        "d" => n * 86_400_000,
        _ => {
            return Err(format!(
                "unknown duration unit '{unit}' in '{s}' (use s, m, h, d)"
            ))
        }
    };
    if ms == 0 {
        return Err(format!("duration '{s}' must be positive"));
    }
    Ok(ms)
}

fn kind_json(kind: &ScheduleKind) -> serde_json::Value {
    match kind {
        ScheduleKind::Cron { expr } => serde_json::json!({"type": "cron", "expr": expr}),
        ScheduleKind::Interval { every_ms } => {
            serde_json::json!({"type": "every", "every_ms": every_ms})
        }
        ScheduleKind::OneShot { at_ms } => serde_json::json!({"type": "oneshot", "at_ms": at_ms}),
        ScheduleKind::Webhook { path } => serde_json::json!({"type": "webhook", "path": path}),
        ScheduleKind::Conditional { expr } => {
            serde_json::json!({"type": "conditional", "expr": expr})
        }
        ScheduleKind::Manual => serde_json::json!({"type": "manual"}),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn job_json(j: &StoredJob) -> serde_json::Value {
    let job = Job::from(j);
    serde_json::json!({
        "id": j.id,
        "task": j.task,
        "kind": kind_json(&j.kind),
        "agent": j.agent,
        "paused": j.paused,
        "last_run_ms": j.last_run,
        "next_fire_ms": job.next_fire_ms(now_ms(), j.last_run),
        "model": j.model,
        "provider": j.provider,
        "timeout_secs": j.timeout_secs,
        "overlap": format!("{:?}", j.overlap).to_lowercase(),
        "deliver": j.deliver,
    })
}

/// `GET /api/schedule/jobs`
pub fn list_jobs(app: &App) -> Response {
    match read_jobs(&app.data_dir) {
        Ok(jobs) => {
            json_ok(serde_json::json!({"jobs": jobs.iter().map(job_json).collect::<Vec<_>>()}))
        }
        Err(e) => err_json(500, "SCHEDULE", &e),
    }
}

/// `GET /api/schedule/templates`: built-ins + user templates with their
/// variables, so the create form can render without hand-written fields.
pub fn list_templates(app: &App) -> Response {
    let store = TemplateStore::load(&app.data_dir);
    let templates: Vec<serde_json::Value> = store
        .list()
        .iter()
        .map(|t| {
            let schedule = match &t.schedule {
                TemplateSchedule::Every(d) => serde_json::json!({"type": "every", "duration": d}),
                TemplateSchedule::Cron(e) => serde_json::json!({"type": "cron", "expr": e}),
            };
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "schedule": schedule,
                "vars": t.vars.iter().map(|v| serde_json::json!({
                    "name": v.name,
                    "question": v.question,
                    "default": v.default,
                    "reserved": is_reserved_var(&v.name),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json_ok(serde_json::json!({"templates": templates}))
}

struct CreateInput {
    task: String,
    every: Option<String>,
    cron: Option<String>,
    template: Option<String>,
    vars: HashMap<String, String>,
    deliver: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    timeout: Option<String>,
    overlap: Option<String>,
    agent: Option<String>,
}

fn parse_create(v: &serde_json::Value) -> Result<CreateInput, String> {
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let mut vars = HashMap::new();
    if let Some(obj) = v.get("vars").and_then(|x| x.as_object()) {
        for (k, val) in obj {
            vars.insert(
                k.clone(),
                val.as_str()
                    .ok_or_else(|| format!("var '{k}' must be a string"))?
                    .to_string(),
            );
        }
    }
    Ok(CreateInput {
        task: get("task").unwrap_or_default(),
        every: get("every"),
        cron: get("cron"),
        template: get("template"),
        vars,
        deliver: get("deliver"),
        model: get("model"),
        provider: get("provider"),
        timeout: get("timeout"),
        overlap: get("overlap"),
        agent: get("agent"),
    })
}

/// `POST /api/schedule/jobs`: create, with the same validation the CLI's
/// `schedule create` applies (template rendering, cron check, model pin,
/// delivery target). No TTY prompting: a template var without a default
/// must be supplied or creation fails loudly.
pub fn create_job(app: &App, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let mut input = match parse_create(&body) {
        Ok(i) => i,
        Err(e) => return bad_json(&e),
    };
    // Template expansion: the real `TemplateStore` machinery.
    if let Some(tname) = input.template.clone() {
        let store = TemplateStore::load(&app.data_dir);
        let t = match store.get(&tname) {
            Some(t) => t,
            None => {
                return bad_json(&format!(
                    "unknown template '{tname}'; see /api/schedule/templates"
                ))
            }
        };
        // Reserved vars become the model pin, not prompt text. An explicit
        // model/provider wins over the var.
        for reserved in ["model", "provider"] {
            if let Some(v) = input.vars.remove(reserved) {
                if v.trim().is_empty() {
                    continue;
                }
                if reserved == "model" && input.model.is_none() {
                    input.model = Some(v);
                } else if reserved == "provider" && input.provider.is_none() {
                    input.provider = Some(v);
                }
            }
        }
        apply_defaults(t, &mut input.vars);
        let mut missing = Vec::new();
        for tv in &t.vars {
            let current = input.vars.get(&tv.name).map(String::as_str).unwrap_or("");
            if current.is_empty() && tv.default.is_none() {
                missing.push(tv.name.clone());
            }
        }
        if !missing.is_empty() {
            return bad_json(&format!(
                "template '{tname}' needs vars: {}",
                missing.join(", ")
            ));
        }
        match render_prompt(t, &input.vars) {
            Ok(rendered) => {
                if input.task.is_empty() {
                    input.task = rendered;
                }
            }
            Err(e) => return bad_json(&format!("template render: {e}")),
        }
        if input.every.is_none() && input.cron.is_none() {
            match &t.schedule {
                TemplateSchedule::Every(d) => input.every = Some(d.clone()),
                TemplateSchedule::Cron(e) => input.cron = Some(e.clone()),
            }
        }
    }
    if input.task.trim().is_empty() {
        return bad_json("need a task (or --template with all vars)");
    }
    if let Some(d) = &input.deliver {
        if let Err(e) = pantheon_gateway::schedule_delivery::Deliver::parse(d) {
            return bad_json(&e);
        }
    }
    let kind = if let Some(expr) = &input.cron {
        if let Err(e) = cron::CronSchedule::validate(expr) {
            return bad_json(&format!("invalid cron expression: {e}"));
        }
        ScheduleKind::Cron { expr: expr.clone() }
    } else if let Some(dur) = &input.every {
        match parse_duration(dur) {
            Ok(ms) => ScheduleKind::Interval { every_ms: ms },
            Err(e) => return bad_json(&e),
        }
    } else {
        return bad_json("need every (duration) or cron (expression)");
    };
    let job_id = format!("job_{}", pantheon_runtime::new_run_id());
    let mut probe = Job::new(&job_id, kind.clone(), "nyx");
    if let Err(e) = probe.validate() {
        return bad_json(&format!("invalid schedule: {e}"));
    }
    if let Some(m) = input.model.as_deref() {
        if let Err(e) = probe.pin_model(m, input.provider.as_deref()) {
            return bad_json(&format!("bad model pin: {e}"));
        }
    } else if input.provider.is_some() {
        return bad_json("provider without model pins nothing; add a model");
    }
    let timeout_secs = match input.timeout.as_deref() {
        None => None,
        Some(d) => match parse_duration(d) {
            Ok(ms) => {
                let secs = ms / 1000;
                if secs == 0 {
                    return bad_json("timeout must be at least 1s");
                }
                Some(secs)
            }
            Err(e) => return bad_json(&format!("bad timeout: {e}")),
        },
    };
    let overlap = match input.overlap.as_deref() {
        None => OverlapPolicy::default(),
        Some(o) => match o.parse::<OverlapPolicy>() {
            Ok(p) => p,
            Err(e) => return bad_json(&format!("bad overlap: {e}")),
        },
    };
    let stored = StoredJob {
        id: job_id.clone(),
        task: input.task,
        kind,
        agent: input.agent,
        missed: MissedPolicy::RunOnce,
        paused: false,
        last_run: None,
        model: probe.model,
        provider: probe.provider,
        timeout_secs,
        overlap,
        deliver: input.deliver,
    };
    let mut jobs = match read_jobs(&app.data_dir) {
        Ok(j) => j,
        Err(e) => return err_json(500, "SCHEDULE", &e),
    };
    jobs.push(stored.clone());
    if let Err(e) = write_jobs(&app.data_dir, &jobs) {
        return err_json(500, "SCHEDULE", &e);
    }
    json_ok(serde_json::json!({"ok": true, "job": job_json(&stored)}))
}

/// `PUT /api/schedule/jobs/:id`: edit fields / pause / resume.
pub fn update_job(app: &App, req: &Request, id: &str) -> Response {
    let body = match body_json(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let mut jobs = match read_jobs(&app.data_dir) {
        Ok(j) => j,
        Err(e) => return err_json(500, "SCHEDULE", &e),
    };
    let pos = match jobs.iter().position(|j| j.id == id) {
        Some(p) => p,
        None => return err_json(404, "NOT_FOUND", "unknown job"),
    };
    let get = |k: &str| body.get(k).and_then(|x| x.as_str()).map(str::to_string);
    if let Some(paused) = body.get("paused").and_then(|x| x.as_bool()) {
        jobs[pos].paused = paused;
    }
    if let Some(task) = get("task") {
        if task.trim().is_empty() {
            return bad_json("task cannot be empty");
        }
        jobs[pos].task = task;
    }
    if let Some(d) = get("deliver") {
        if let Err(e) = pantheon_gateway::schedule_delivery::Deliver::parse(&d) {
            return bad_json(&e);
        }
        jobs[pos].deliver = Some(d);
    }
    if body.get("deliver").is_some() && get("deliver").is_none() {
        jobs[pos].deliver = None; // explicit null clears
    }
    if let Some(m) = get("model") {
        let mut probe = Job::from(&jobs[pos]);
        if let Err(e) = probe.pin_model(
            &m,
            get("provider").as_deref().or(jobs[pos].provider.as_deref()),
        ) {
            return bad_json(&format!("bad model pin: {e}"));
        }
        jobs[pos].model = probe.model;
        jobs[pos].provider = probe.provider;
    }
    // Rescheduling: rebuild the kind and re-validate like creation does.
    if get("cron").is_some() || get("every").is_some() {
        let kind = if let Some(expr) = get("cron") {
            if let Err(e) = cron::CronSchedule::validate(&expr) {
                return bad_json(&format!("invalid cron expression: {e}"));
            }
            ScheduleKind::Cron { expr }
        } else {
            match parse_duration(&get("every").unwrap_or_default()) {
                Ok(ms) => ScheduleKind::Interval { every_ms: ms },
                Err(e) => return bad_json(&e),
            }
        };
        let mut probe = Job::from(&jobs[pos]);
        probe.kind = kind.clone();
        if let Err(e) = probe.validate() {
            return bad_json(&format!("invalid schedule: {e}"));
        }
        jobs[pos].kind = kind;
    }
    if let Some(o) = get("overlap") {
        match o.parse::<OverlapPolicy>() {
            Ok(p) => jobs[pos].overlap = p,
            Err(e) => return bad_json(&format!("bad overlap: {e}")),
        }
    }
    if let Err(e) = write_jobs(&app.data_dir, &jobs) {
        return err_json(500, "SCHEDULE", &e);
    }
    json_ok(serde_json::json!({"ok": true, "job": job_json(&jobs[pos])}))
}

/// `DELETE /api/schedule/jobs/:id`
pub fn delete_job(app: &App, id: &str) -> Response {
    let mut jobs = match read_jobs(&app.data_dir) {
        Ok(j) => j,
        Err(e) => return err_json(500, "SCHEDULE", &e),
    };
    let before = jobs.len();
    jobs.retain(|j| j.id != id);
    if jobs.len() == before {
        return err_json(404, "NOT_FOUND", "unknown job");
    }
    if let Err(e) = write_jobs(&app.data_dir, &jobs) {
        return err_json(500, "SCHEDULE", &e);
    }
    json_ok(serde_json::json!({"ok": true}))
}

/// `POST /api/schedule/jobs/:id/trigger`: run now via the real CLI path,
/// in the background. 202 immediately; the run appears in the ledger.
pub fn trigger_job(app: &App, _req: &Request, id: &str) -> Response {
    let jobs = match read_jobs(&app.data_dir) {
        Ok(j) => j,
        Err(e) => return err_json(500, "SCHEDULE", &e),
    };
    if !jobs.iter().any(|j| j.id == id) {
        return err_json(404, "NOT_FOUND", "unknown job");
    }
    let id_owned = id.to_string();
    std::thread::spawn(move || {
        spawn_pantheon(&["schedule", "run", &id_owned]);
    });
    json_ok(serde_json::json!({"ok": true, "triggered": id}))
}
