//! Schedule CRUD over the reworked scheduler core.
//!
//! Jobs live in `<data_dir>/schedule.json` - the same file the
//! `pantheon schedule` CLI reads and writes. The dashboard does not keep
//! its own store mirror: the shared [`ScheduledJob`] shape is read and
//! written with the core's [`load_jobs`]/[`save_jobs`], so legacy rows
//! migrate on load and the file stays the single source of truth.
//! Templates go through [`TemplateStore::open`] (write-through to
//! `templates.json`).
//!
//! Triggering a run does not reimplement the runner: it spawns the real
//! `pantheon schedule run <id>` CLI path in the background.

use crate::util::{now_ms, DEFAULT_AGENT_NAME};
use crate::{bad_json, body_json, err_json, json_ok, spawn_pantheon, App};
use pantheon_gateway::http::{Request, Response};
use pantheon_scheduler::{
    apply_defaults, cron, expand_template, is_reserved_var, load_jobs, load_jobs_with_warnings,
    parse_duration, render_prompt, update_jobs, Job, OverlapPolicy, ScheduleKind, ScheduleTemplate,
    ScheduledJob, TemplateSchedule, TemplateStore, TemplateVar,
};
use std::collections::HashMap;

/// Only the three live kinds can be created. Rows in the old file format
/// Render a [`ScheduleKind`] for the dashboard. Webhook jobs show their
/// path; they fire on inbound calls, never on the tick.
fn kind_json(kind: &ScheduleKind) -> serde_json::Value {
    match kind {
        ScheduleKind::Cron { expr } => serde_json::json!({"type": "cron", "expr": expr}),
        ScheduleKind::Interval { every_ms } => {
            serde_json::json!({"type": "every", "every_ms": every_ms})
        }
        ScheduleKind::OneShot { at_ms } => serde_json::json!({"type": "oneshot", "at_ms": at_ms}),
        ScheduleKind::Webhook { path } => serde_json::json!({"type": "webhook", "path": path}),
    }
}

/// The shared [`ScheduledJob`] shape: `{"job": {...}, "last_run": ...}`,
/// with the kind expanded for the UI. The `job` object carries `task`,
/// `template`, `template_vars`, `agent`, and `catch_up` straight from the
/// core's serde.
fn job_json(j: &ScheduledJob) -> serde_json::Value {
    let mut job = serde_json::to_value(&j.job).unwrap_or(serde_json::Value::Null);
    if let Some(obj) = job.as_object_mut() {
        obj.insert("kind".to_string(), kind_json(&j.job.kind));
    }
    serde_json::json!({
        "job": job,
        "last_run": j.last_run,
        "next_fire_ms": j.job.next_fire_ms(now_ms(), j.last_run),
    })
}

/// Success envelope for single-job responses: the shared
/// `{"job": {...}, "last_run": ..., "next_fire_ms": ...}` shape plus `ok`.
fn ok_job(j: &ScheduledJob) -> Response {
    let mut v = job_json(j);
    if let Some(obj) = v.as_object_mut() {
        obj.insert("ok".to_string(), serde_json::Value::Bool(true));
    }
    json_ok(v)
}

/// `GET /api/schedule/jobs`
pub fn list_jobs(app: &App) -> Response {
    // Warnings name legacy rows skipped because their trigger kind was
    // removed (webhook/conditional/manual), so the dashboard can show
    // them instead of letting jobs vanish silently.
    let (result, warnings) = load_jobs_with_warnings(&app.data_dir);
    match result {
        Ok(jobs) => json_ok(serde_json::json!({
            "jobs": jobs.iter().map(job_json).collect::<Vec<_>>(),
            "warnings": warnings,
        })),
        Err(e) => err_json(500, "SCHEDULE", &e),
    }
}

/// `GET /api/schedule/templates`: built-ins + user templates with their
/// variables, so the create form can render without hand-written fields.
pub fn list_templates(app: &App) -> Response {
    let store = TemplateStore::open(&app.data_dir);
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
                "builtin": store.is_builtin(&t.name),
            })
        })
        .collect();
    json_ok(serde_json::json!({"templates": templates}))
}

fn template_json(t: &ScheduleTemplate, builtin: bool) -> serde_json::Value {
    let schedule = match &t.schedule {
        TemplateSchedule::Every(d) => serde_json::json!({"type": "every", "duration": d}),
        TemplateSchedule::Cron(e) => serde_json::json!({"type": "cron", "expr": e}),
    };
    serde_json::json!({
        "name": t.name,
        "description": t.description,
        "schedule": schedule,
        "prompt": t.prompt,
        "vars": t.vars,
        "builtin": builtin,
    })
}

/// `POST /api/schedule/templates`: body has `name`, `description`,
/// `schedule` (`{"cron": "..."}` or `{"every": "30m"}`), `prompt`, and
/// `vars` (`[{name, question, default?}]`). The core validates loudly
/// (bad cron, empty prompt, ...) and writes through to `templates.json`.
pub fn create_template(app: &App, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let get = |k: &str| body.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let schedule = match body.get("schedule") {
        Some(s) if s.get("cron").and_then(|x| x.as_str()).is_some() => {
            TemplateSchedule::Cron(s["cron"].as_str().unwrap_or_default().to_string())
        }
        Some(s) if s.get("every").and_then(|x| x.as_str()).is_some() => {
            TemplateSchedule::Every(s["every"].as_str().unwrap_or_default().to_string())
        }
        _ => return bad_json("schedule must be {\"cron\": \"...\"} or {\"every\": \"30m\"}"),
    };
    let mut vars = Vec::new();
    if let Some(list) = body.get("vars").and_then(|x| x.as_array()) {
        for v in list {
            let Some(name) = v.get("name").and_then(|x| x.as_str()) else {
                return bad_json("each var needs a name");
            };
            vars.push(TemplateVar {
                name: name.to_string(),
                question: v
                    .get("question")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string(),
                default: v
                    .get("default")
                    .and_then(|x| x.as_str())
                    .map(str::to_string),
            });
        }
    }
    let template = ScheduleTemplate {
        name: get("name").unwrap_or_default(),
        description: get("description").unwrap_or_default(),
        schedule,
        prompt: get("prompt").unwrap_or_default(),
        vars,
    };
    let mut store = TemplateStore::open(&app.data_dir);
    if let Err(e) = store.save(template.clone()) {
        return bad_json(&e);
    }
    json_ok(serde_json::json!({"ok": true, "template": template_json(&template, false)}))
}

/// `DELETE /api/schedule/templates/:name`. Removes a user template; when
/// the name overrides a built-in, the override is removed and the built-in
/// is revealed again. Built-in names with no override, and unknown names,
/// are an error.
pub fn delete_template(app: &App, name: &str) -> Response {
    let mut store = TemplateStore::open(&app.data_dir);
    if let Err(e) = store.delete(name) {
        return bad_json(&e);
    }
    json_ok(serde_json::json!({"ok": true}))
}

struct CreateInput {
    task: String,
    every: Option<String>,
    cron: Option<String>,
    at_ms: Option<i64>,
    template: Option<String>,
    vars: HashMap<String, String>,
    catch_up: bool,
    agent: Option<String>,
    deliver: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    timeout: Option<String>,
    overlap: Option<String>,
}

fn parse_vars(
    obj: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Result<HashMap<String, String>, String> {
    let mut vars = HashMap::new();
    if let Some(map) = obj {
        for (k, val) in map {
            vars.insert(
                k.clone(),
                val.as_str()
                    .ok_or_else(|| format!("var '{k}' must be a string"))?
                    .to_string(),
            );
        }
    }
    Ok(vars)
}

fn parse_create(v: &serde_json::Value) -> Result<CreateInput, String> {
    let get = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    // `template_vars` is the documented key; `vars` is accepted as an alias.
    let mut vars = parse_vars(v.get("template_vars").and_then(|x| x.as_object()))?;
    if vars.is_empty() {
        vars = parse_vars(v.get("vars").and_then(|x| x.as_object()))?;
    }
    let at_ms = match v.get("at_ms") {
        None => None,
        Some(x) => Some(
            x.as_i64()
                .ok_or_else(|| "at_ms must be an integer epoch-millis timestamp".to_string())?,
        ),
    };
    Ok(CreateInput {
        task: get("task").unwrap_or_default(),
        every: get("every"),
        cron: get("cron"),
        at_ms,
        template: get("template"),
        vars,
        catch_up: v.get("catch_up").and_then(|x| x.as_bool()).unwrap_or(true),
        agent: get("agent"),
        deliver: get("deliver"),
        model: get("model"),
        provider: get("provider"),
        timeout: get("timeout"),
        overlap: get("overlap"),
    })
}

/// Kind precedence: cron > every > at_ms, like the CLI.
fn build_kind(input: &CreateInput) -> Result<ScheduleKind, String> {
    if let Some(expr) = &input.cron {
        if let Err(e) = cron::CronSchedule::validate(expr) {
            return Err(format!("invalid cron expression: {e}"));
        }
        Ok(ScheduleKind::Cron { expr: expr.clone() })
    } else if let Some(dur) = &input.every {
        match parse_duration(dur) {
            Ok(ms) => Ok(ScheduleKind::Interval { every_ms: ms }),
            Err(e) => Err(e),
        }
    } else if let Some(at_ms) = input.at_ms {
        Ok(ScheduleKind::OneShot { at_ms })
    } else {
        Err("need every (duration), cron (expression), or at_ms (epoch millis)".to_string())
    }
}

/// `POST /api/schedule/jobs`: create, with the same validation the CLI's
/// `schedule create` applies (template rendering, cron check, model pin,
/// delivery target). No TTY prompting: a template var without a default
/// must be supplied or creation fails loudly. The rendered prompt becomes
/// the `task` snapshot; the template name and vars are stored on the job
/// for re-render at fire time.
pub fn create_job(app: &App, req: &Request) -> Response {
    let body = match body_json(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let mut input = match parse_create(&body) {
        Ok(i) => i,
        Err(e) => return bad_json(&e),
    };
    // Template expansion: the shared `expand_template` machinery. The
    // rendered prompt is the task snapshot; the name + vars stay on the
    // job so the fire path can re-render with fresh defaults. No TTY
    // prompting: a template var without a default must be supplied, or
    // creation fails loudly naming every missing var at once.
    if let Some(tname) = input.template.clone() {
        let store = TemplateStore::open(&app.data_dir);
        let t = match store.get(&tname) {
            Some(t) => t,
            None => {
                return bad_json(&format!(
                    "unknown template '{tname}'; see /api/schedule/templates"
                ))
            }
        };
        let has_schedule = input.every.is_some() || input.cron.is_some() || input.at_ms.is_some();
        let mut missing = Vec::new();
        let mut collect = |name: &str, _question: &str| -> Result<String, String> {
            missing.push(name.to_string());
            // The error value is discarded: the caller reports `missing`.
            Err(format!("template '{tname}' needs --var {name}=<value>"))
        };
        if let Err(e) = expand_template(
            t,
            &mut input.vars,
            &mut input.model,
            &mut input.provider,
            &mut input.task,
            &mut input.every,
            &mut input.cron,
            has_schedule,
            &mut collect,
        ) {
            if !missing.is_empty() {
                return bad_json(&format!(
                    "template '{tname}' needs vars: {}",
                    missing.join(", ")
                ));
            }
            return bad_json(&format!("template render: {e}"));
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
    let kind = match build_kind(&input) {
        Ok(k) => k,
        Err(e) => return bad_json(&e),
    };
    let agent = input
        .agent
        .unwrap_or_else(|| DEFAULT_AGENT_NAME.to_string());
    let job_id = format!("job_{}", pantheon_runtime::new_run_id());
    let mut job = Job::new(&job_id, kind, &agent);
    if let Err(e) = job.validate() {
        return bad_json(&format!("invalid schedule: {e}"));
    }
    if let Some(m) = input.model.as_deref() {
        if let Err(e) = job.pin_model(m, input.provider.as_deref()) {
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
    job.task = input.task;
    job.template = input.template;
    job.template_vars = input.vars;
    job.catch_up = input.catch_up;
    job.timeout_secs = timeout_secs;
    job.overlap = overlap;
    job.deliver = input.deliver;
    let scheduled = ScheduledJob {
        job,
        last_run: None,
    };
    // Single locked read-modify-write: two dashboard threads creating jobs
    // at once can't clobber each other's rows.
    match update_jobs(&app.data_dir, |jobs| {
        jobs.push(scheduled.clone());
        Ok::<_, String>(scheduled.clone())
    }) {
        Ok(created) => ok_job(&created),
        Err(e) => err_json(500, "SCHEDULE", &e),
    }
}

/// `PUT /api/schedule/jobs/:id`: edit fields / pause / resume. `template`
/// reassigns the blueprint (null clears it); `template_vars`/`vars`
/// merge into the job's vars, validated against the template like creation.
pub fn update_job(app: &App, req: &Request, id: &str) -> Response {
    let body = match body_json(req) {
        Ok(v) => v,
        Err(r) => return r,
    };
    // The edit, computed under the store's exclusive lock so a
    // concurrent dashboard request cannot slip an edit between our
    // load and save. Validation failures are data, not store errors:
    // the lock is released and the file rewrite is a harmless no-op.
    enum Edit {
        Done(ScheduledJob),
        Bad(String),
        Missing,
    }
    let outcome = match update_jobs(&app.data_dir, |jobs| {
        let pos = match jobs.iter().position(|j| j.job.id == id) {
            Some(p) => p,
            None => return Ok(Edit::Missing),
        };
        let get = |k: &str| body.get(k).and_then(|x| x.as_str()).map(str::to_string);
        if let Some(paused) = body.get("paused").and_then(|x| x.as_bool()) {
            jobs[pos].job.paused = paused;
        }
        if let Some(catch_up) = body.get("catch_up").and_then(|x| x.as_bool()) {
            jobs[pos].job.catch_up = catch_up;
        }
        // Template reassignment: `template` swaps the blueprint (explicit
        // null clears it); `template_vars` (alias `vars`) merges into the
        // job's vars. Validation mirrors creation: the template must exist,
        // reserved vars (`model`, `provider`) become the model pin when the
        // body sets none explicitly, defaults fill gaps, and the prompt must
        // render. A reassigned template refreshes the task snapshot; an
        // explicit `task` in the same body still wins (handled below).
        if body.get("template").is_some()
            || body.get("template_vars").is_some()
            || body.get("vars").is_some()
        {
            let store = TemplateStore::open(&app.data_dir);
            // Present-but-not-an-object is a 400, not a silent ignore.
            let strict_vars = |key: &str| -> Result<HashMap<String, String>, String> {
                match body.get(key) {
                    None | Some(serde_json::Value::Null) => Ok(HashMap::new()),
                    Some(serde_json::Value::Object(map)) => parse_vars(Some(map)),
                    Some(_) => Err(format!("`{key}` must be an object of string values")),
                }
            };
            let mut new_vars = match strict_vars("vars") {
                Ok(v) => v,
                Err(e) => return Ok(Edit::Bad(e)),
            };
            let template_vars = match strict_vars("template_vars") {
                Ok(v) => v,
                Err(e) => return Ok(Edit::Bad(e)),
            };
            for (k, v) in template_vars {
                new_vars.insert(k, v);
            }
            // Reserved vars become the model pin, not prompt text - like
            // creation. An explicit model/provider in the body wins. Applied
            // once, before the template/vars branches below.
            let mut var_model: Option<String> = None;
            let mut var_provider: Option<String> = None;
            for reserved in ["model", "provider"] {
                if let Some(v) = new_vars.remove(reserved) {
                    if v.trim().is_empty() {
                        continue;
                    }
                    if reserved == "model" {
                        var_model = Some(v);
                    } else {
                        var_provider = Some(v);
                    }
                }
            }
            if let Some(m) = var_model {
                if body.get("model").is_none() {
                    let mut probe = jobs[pos].job.clone();
                    let pin_provider = var_provider.clone().or_else(|| probe.provider.clone());
                    if let Err(e) = probe.pin_model(&m, pin_provider.as_deref()) {
                        return Ok(Edit::Bad(format!("bad model pin: {e}")));
                    }
                    jobs[pos].job.model = probe.model;
                    jobs[pos].job.provider = probe.provider;
                }
            } else if var_provider.is_some() {
                return Ok(Edit::Bad(
                    "provider without model pins nothing; add a model".to_string(),
                ));
            }
            if body.get("template").is_some() {
                match body.get("template") {
                    Some(serde_json::Value::String(tname)) => {
                        let t = match store.get(tname) {
                            Some(t) => t,
                            None => {
                                return Ok(Edit::Bad(format!(
                                    "unknown template '{tname}'; see /api/schedule/templates"
                                )))
                            }
                        };
                        // Keep old vars the new template still declares,
                        // overlaid with the newly provided ones.
                        let declared: std::collections::HashSet<&str> =
                            t.vars.iter().map(|v| v.name.as_str()).collect();
                        let mut merged: HashMap<String, String> = jobs[pos]
                            .job
                            .template_vars
                            .iter()
                            .filter(|(k, _)| declared.contains(k.as_str()))
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        for (k, v) in new_vars {
                            merged.insert(k, v);
                        }
                        apply_defaults(t, &mut merged);
                        let missing: Vec<String> = t
                            .vars
                            .iter()
                            .filter(|tv| {
                                merged
                                    .get(&tv.name)
                                    .map(String::as_str)
                                    .unwrap_or("")
                                    .is_empty()
                                    && tv.default.is_none()
                            })
                            .map(|tv| tv.name.clone())
                            .collect();
                        if !missing.is_empty() {
                            return Ok(Edit::Bad(format!(
                                "template '{tname}' needs vars: {}",
                                missing.join(", ")
                            )));
                        }
                        let rendered = match render_prompt(t, &merged) {
                            Ok(r) => r,
                            Err(e) => return Ok(Edit::Bad(format!("template render: {e}"))),
                        };
                        jobs[pos].job.template = Some(tname.to_string());
                        jobs[pos].job.template_vars = merged;
                        jobs[pos].job.task = rendered;
                    }
                    Some(serde_json::Value::Null) => {
                        // Explicit null: drop the blueprint. Provided vars are
                        // kept (inert without a template, like creation).
                        jobs[pos].job.template = None;
                        jobs[pos].job.template_vars = new_vars;
                    }
                    _ => {
                        return Ok(Edit::Bad(
                            "`template` must be a template name or null".to_string(),
                        ))
                    }
                }
            } else {
                // Vars only: merge into the existing vars. Reserved vars were
                // already intercepted and pinned above.
                for (k, v) in new_vars {
                    jobs[pos].job.template_vars.insert(k, v);
                }
                if let Some(tname) = jobs[pos].job.template.clone() {
                    let t = match store.get(&tname) {
                        Some(t) => t,
                        None => {
                            return Ok(Edit::Bad(format!(
                            "job's template '{tname}' no longer exists; reassign `template` first"
                        )))
                        }
                    };
                    if let Err(e) = render_prompt(t, &jobs[pos].job.template_vars) {
                        return Ok(Edit::Bad(format!("template render: {e}")));
                    }
                }
            }
        }
        if let Some(task) = get("task") {
            if task.trim().is_empty() {
                return Ok(Edit::Bad("task cannot be empty".to_string()));
            }
            jobs[pos].job.task = task;
        }
        if let Some(d) = get("deliver") {
            if let Err(e) = pantheon_gateway::schedule_delivery::Deliver::parse(&d) {
                return Ok(Edit::Bad(e));
            }
            jobs[pos].job.deliver = Some(d);
        }
        if body.get("deliver").is_some() && get("deliver").is_none() {
            jobs[pos].job.deliver = None; // explicit null clears
        }
        if let Some(m) = get("model") {
            let mut probe = jobs[pos].job.clone();
            if let Err(e) = probe.pin_model(
                &m,
                get("provider")
                    .as_deref()
                    .or(jobs[pos].job.provider.as_deref()),
            ) {
                return Ok(Edit::Bad(format!("bad model pin: {e}")));
            }
            jobs[pos].job.model = probe.model;
            jobs[pos].job.provider = probe.provider;
        }
        if let Some(agent) = get("agent") {
            if agent.trim().is_empty() {
                return Ok(Edit::Bad("agent cannot be empty".to_string()));
            }
            jobs[pos].job.agent = agent;
        }
        // Rescheduling: rebuild the kind and re-validate like creation does
        // (cron > every > at_ms).
        if get("cron").is_some() || get("every").is_some() || body.get("at_ms").is_some() {
            let expr = get("cron");
            let dur = get("every");
            let at_ms = body.get("at_ms").and_then(|x| x.as_i64());
            let kind = if let Some(e) = expr {
                if let Err(err) = cron::CronSchedule::validate(&e) {
                    return Ok(Edit::Bad(format!("invalid cron expression: {err}")));
                }
                ScheduleKind::Cron { expr: e }
            } else if let Some(d) = dur {
                match parse_duration(&d) {
                    Ok(ms) => ScheduleKind::Interval { every_ms: ms },
                    Err(e) => return Ok(Edit::Bad(e)),
                }
            } else if let Some(ms) = at_ms {
                ScheduleKind::OneShot { at_ms: ms }
            } else {
                return Ok(Edit::Bad(
                    "at_ms must be an integer epoch-millis timestamp".to_string(),
                ));
            };
            let mut probe = jobs[pos].job.clone();
            probe.kind = kind.clone();
            if let Err(e) = probe.validate() {
                return Ok(Edit::Bad(format!("invalid schedule: {e}")));
            }
            jobs[pos].job.kind = kind;
        }
        if let Some(o) = get("overlap") {
            match o.parse::<OverlapPolicy>() {
                Ok(p) => jobs[pos].job.overlap = p,
                Err(e) => return Ok(Edit::Bad(format!("bad overlap: {e}"))),
            }
        }
        Ok(Edit::Done(jobs[pos].clone()))
    }) {
        Ok(o) => o,
        Err(e) => return err_json(500, "SCHEDULE", &e),
    };
    match outcome {
        Edit::Done(job) => ok_job(&job),
        Edit::Bad(msg) => bad_json(&msg),
        Edit::Missing => err_json(404, "NOT_FOUND", "unknown job"),
    }
}

/// `DELETE /api/schedule/jobs/:id`
pub fn delete_job(app: &App, id: &str) -> Response {
    // Locked read-modify-write: the existence check and the removal are
    // one atomic step, so a concurrent edit can't resurrect or duplicate.
    match update_jobs(&app.data_dir, |jobs| {
        let before = jobs.len();
        jobs.retain(|j| j.job.id != id);
        Ok::<_, String>(jobs.len() != before)
    }) {
        Ok(true) => json_ok(serde_json::json!({"ok": true})),
        Ok(false) => err_json(404, "NOT_FOUND", "unknown job"),
        Err(e) => err_json(500, "SCHEDULE", &e),
    }
}

/// `POST /api/schedule/jobs/:id/trigger`: run now via the real CLI path,
/// in the background. 200 immediately; the run appears in the ledger.
pub fn trigger_job(app: &App, _req: &Request, id: &str) -> Response {
    let jobs = match load_jobs(&app.data_dir) {
        Ok(j) => j,
        Err(e) => return err_json(500, "SCHEDULE", &e),
    };
    if !jobs.iter().any(|j| j.job.id == id) {
        return err_json(404, "NOT_FOUND", "unknown job");
    }
    let id_owned = id.to_string();
    std::thread::spawn(move || {
        let _ = spawn_pantheon(&["schedule", "run", &id_owned]);
    });
    json_ok(serde_json::json!({"ok": true, "triggered": id}))
}
