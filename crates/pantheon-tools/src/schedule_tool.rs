//! Agent-facing schedule tool.
//!
//! Pantheon had a durable scheduler and a `/schedule` UI, but the agent could
//! not create, inspect or retire its own jobs — the operator had to be in the
//! loop for every standing task. This exposes the same job store
//! (`schedule.json`) under the capability gate:
//!
//! * reading (`list`) needs `schedule`;
//! * mutating (`create` / `remove` / `pause` / `resume`) additionally needs
//!   `schedule.write`, which the coder preset marks **approval**. A model may
//!   propose a standing job; a human accepts it, the same way a `git push` is
//!   proposed rather than performed.
//!
//! Jobs are ordinary scheduler rows, so `/schedule`, the dashboard and the
//! ticker see them with no extra plumbing.

use crate::tools::{ToolRegistry, tool_err};
use pantheon_api::capability::Capability;
use pantheon_api::error::Layer;
use pantheon_scheduler::{Job, ScheduleKind, load_jobs, normalize_cron_expr, save_jobs};
use serde_json::Value;
use std::path::PathBuf;

pub const NAME: &str = "schedule";

fn perr(code: &str, cause: String) -> pantheon_api::error::PantheonError {
    tool_err(
        code,
        Layer::Execution,
        false,
        cause,
        "check the schedule arguments",
    )
}

/// Capabilities for one call: every action reads, mutating actions write.
fn capabilities(args: &str) -> Vec<Capability> {
    let action = serde_json::from_str::<Value>(args)
        .ok()
        .and_then(|v| {
            v.get("action")
                .and_then(|a| a.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    match action.as_str() {
        "create" | "remove" | "pause" | "resume" => vec![Capability::Other("schedule.write".into())],
        _ => Vec::new(),
    }
}

/// Register the tool. `data_dir` is the install whose `schedule.json` it edits.
pub fn register(reg: &mut ToolRegistry, data_dir: PathBuf) {
    reg.register_with(
        pantheon_api::message::ToolSchema {
            name: NAME.into(),
            description: "Manage durable scheduled jobs. `list` shows id, kind, next task text, \
                          paused state and delivery; `create` adds one (give `cron`, `every_ms` or \
                          `at_ms`, plus `task`); `remove`/`pause`/`resume` take `id`. Runs happen \
                          outside this conversation."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "create", "remove", "pause", "resume"]},
                    "id": {"type": "string", "description": "job id (required for remove/pause/resume)"},
                    "cron": {"type": "string", "description": "5-field cron or @daily/@hourly/@weekly/@monthly/@yearly"},
                    "every_ms": {"type": "integer", "description": "interval in milliseconds"},
                    "at_ms": {"type": "integer", "description": "one-shot epoch milliseconds"},
                    "task": {"type": "string", "description": "prompt the job runs"},
                    "deliver": {"type": "string", "description": "log | telegram | discord | file:<path>"},
                    "agent": {"type": "string", "description": "agent profile to run as"}
                },
                "required": ["action"]
            }),
        },
        Capability::Other("schedule".into()),
        move |args| run(args, &data_dir),
        Some(Box::new(capabilities)),
    );
}

fn run(args: &str, data_dir: &std::path::Path) -> Result<String, pantheon_api::error::PantheonError> {
    let v: Value = serde_json::from_str(args).map_err(|e| perr("SCHEDULE_ARGS", e.to_string()))?;
    let action = v
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let mut jobs = load_jobs(data_dir).map_err(|e| perr("SCHEDULE_LOAD", e))?;
    match action.as_str() {
        "list" => Ok(list_json(&jobs)),
        "create" => {
            let task = v
                .get("task")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .ok_or_else(|| perr("SCHEDULE_TASK", "create needs a non-empty `task`".into()))?
                .to_string();
            let kind = job_kind(&v)?;
            let id = match v.get("id").and_then(Value::as_str).map(str::trim) {
                Some(id) if !id.is_empty() => id.to_string(),
                _ => default_id(&task),
            };
            if jobs.iter().any(|j| j.job.id == id) {
                return Err(perr(
                    "SCHEDULE_DUPLICATE",
                    format!("a job with id {id:?} already exists; pass a different `id` or remove it first"),
                ));
            }
            let agent = v
                .get("agent")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|a| !a.is_empty())
                .unwrap_or("nyx");
            let mut job = Job::new(&id, kind, agent);
            job.task = task;
            job.deliver = v
                .get("deliver")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .map(str::to_string);
            jobs.push(pantheon_scheduler::ScheduledJob {
                job: job.clone(),
                last_run: None,
            });
            save_jobs(data_dir, &jobs).map_err(|e| perr("SCHEDULE_SAVE", e))?;
            Ok(serde_json::json!({
                "created": job.id,
                "kind": describe_kind(&job.kind),
                "deliver": job.deliver,
                "paused": job.paused,
            })
            .to_string())
        }
        "remove" | "pause" | "resume" => {
            let id = required_id(&v)?;
            let before = jobs.len();
            if action == "remove" {
                jobs.retain(|j| j.job.id != id);
                if jobs.len() == before {
                    return Err(perr("SCHEDULE_NO_JOB", format!("no job {id:?}")));
                }
            } else {
                let job = jobs
                    .iter_mut()
                    .find(|j| j.job.id == id)
                    .ok_or_else(|| perr("SCHEDULE_NO_JOB", format!("no job {id:?}")))?;
                job.job.paused = action == "pause";
            }
            save_jobs(data_dir, &jobs).map_err(|e| perr("SCHEDULE_SAVE", e))?;
            Ok(serde_json::json!({"ok": true, "action": action, "id": id}).to_string())
        }
        other => Err(perr(
            "SCHEDULE_ACTION",
            format!("unknown action {other:?} (list|create|remove|pause|resume)"),
        )),
    }
}

fn required_id(v: &Value) -> Result<String, pantheon_api::error::PantheonError> {
    v.get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .ok_or_else(|| perr("SCHEDULE_ID", "this action needs `id`".into()))
}

fn job_kind(v: &Value) -> Result<ScheduleKind, pantheon_api::error::PantheonError> {
    if let Some(cron) = v.get("cron").and_then(Value::as_str) {
        let expr = normalize_cron_expr(cron)
            .ok_or_else(|| perr("SCHEDULE_CRON", format!("invalid cron expression {cron:?}")))?;
        // Validated by the scheduler's own parser: an expression it cannot
        // schedule must not reach the store.
        pantheon_scheduler::CronSchedule::parse(&expr)
            .map_err(|e| perr("SCHEDULE_CRON", format!("{cron:?}: {e}")))?;
        return Ok(ScheduleKind::Cron { expr });
    }
    if let Some(ms) = v.get("every_ms").and_then(Value::as_u64) {
        if ms == 0 {
            return Err(perr("SCHEDULE_INTERVAL", "every_ms must be > 0".into()));
        }
        return Ok(ScheduleKind::Interval { every_ms: ms });
    }
    if let Some(at) = v.get("at_ms").and_then(Value::as_i64) {
        if at <= 0 {
            return Err(perr("SCHEDULE_ONESHOT", "at_ms must be a positive epoch".into()));
        }
        return Ok(ScheduleKind::OneShot { at_ms: at });
    }
    Err(perr(
        "SCHEDULE_KIND",
        "create needs exactly one of `cron`, `every_ms`, or `at_ms`".into(),
    ))
}

fn describe_kind(kind: &ScheduleKind) -> String {
    match kind {
        ScheduleKind::Cron { expr } => format!("cron {expr}"),
        ScheduleKind::Interval { every_ms } => format!("every {every_ms}ms"),
        ScheduleKind::OneShot { at_ms } => format!("once at {at_ms}"),
    }
}

fn default_id(task: &str) -> String {
    let slug: String = task
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let slug = slug.trim_matches('-');
    let short: String = slug.chars().take(32).collect();
    let short = short.trim_end_matches('-').to_string();
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    if short.is_empty() {
        format!("job-{ms}")
    } else {
        format!("{short}-{ms}")
    }
}

fn list_json(jobs: &[pantheon_scheduler::ScheduledJob]) -> String {
    let rows: Vec<Value> = jobs
        .iter()
        .map(|s| &s.job)
        .map(|j| {
            serde_json::json!({
                "id": j.id,
                "kind": describe_kind(&j.kind),
                "task": j.task.lines().next().unwrap_or("").chars().take(120).collect::<String>(),
                "paused": j.paused,
                "deliver": j.deliver,
                "agent": j.agent,
            })
        })
        .collect();
    serde_json::json!({"jobs": rows, "count": jobs.len()}).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn create_list_pause_and_remove_round_trip() {
        let d = dir();
        let created = run(
            r#"{"action":"create","id":"digest","cron":"@daily","task":"write the daily digest","deliver":"log"}"#,
            d.path(),
        )
        .expect("create");
        assert!(created.contains("digest"), "{created}");
        let listed = run(r#"{"action":"list"}"#, d.path()).expect("list");
        assert!(listed.contains("write the daily digest"), "{listed}");
        assert!(listed.contains("cron 0 0 * * *"), "{listed}");

        run(r#"{"action":"pause","id":"digest"}"#, d.path()).expect("pause");
        let listed = run(r#"{"action":"list"}"#, d.path()).expect("list");
        assert!(listed.contains("\"paused\":true"), "{listed}");

        run(r#"{"action":"remove","id":"digest"}"#, d.path()).expect("remove");
        let listed = run(r#"{"action":"list"}"#, d.path()).expect("list");
        assert!(listed.contains("\"count\":0"), "{listed}");
    }

    #[test]
    fn bad_kinds_and_unknown_ids_are_refused_with_a_reason() {
        let d = dir();
        let err = run(r#"{"action":"create","task":"x"}"#, d.path()).unwrap_err();
        assert_eq!(err.code, "SCHEDULE_KIND");
        let err = run(
            r#"{"action":"create","cron":"not a cron","task":"x"}"#,
            d.path(),
        )
        .unwrap_err();
        assert_eq!(err.code, "SCHEDULE_CRON");
        let err = run(r#"{"action":"remove","id":"nope"}"#, d.path()).unwrap_err();
        assert_eq!(err.code, "SCHEDULE_NO_JOB");
        let err = run(r#"{"action":"create","id":"a","cron":"@daily","task":"x"}"#, d.path()).unwrap();
        assert!(err.contains("a"));
        let err = run(r#"{"action":"create","id":"a","cron":"@daily","task":"y"}"#, d.path()).unwrap_err();
        assert_eq!(err.code, "SCHEDULE_DUPLICATE");
    }

    #[test]
    fn register_installs_the_tool_with_its_schema() {
        let d = dir();
        let mut reg = ToolRegistry::new();
        register(&mut reg, d.path().to_path_buf());
        let tool = reg.get("schedule").expect("registered");
        assert_eq!(tool.schema.name, "schedule");
        let props = &tool.schema.parameters["properties"];
        assert!(props["action"].is_object());
        assert!(props["cron"].is_object());
    }

    #[test]
    fn mutating_actions_declare_the_write_capability() {
        assert!(capabilities(r#"{"action":"list"}"#).is_empty());
        assert_eq!(
            capabilities(r#"{"action":"create","task":"x"}"#),
            vec![Capability::Other("schedule.write".into())]
        );
        assert_eq!(
            capabilities(r#"{"action":"remove","id":"x"}"#),
            vec![Capability::Other("schedule.write".into())]
        );
    }
}
