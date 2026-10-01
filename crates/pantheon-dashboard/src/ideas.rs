//! Ideas: the HTTP surface for nightly-gated proactive suggestions.
//!
//! `GET /api/ideas` lists them; `POST /api/ideas/:id/accept` spawns the
//! work (a new run for `general` ideas, a scheduled job for
//! `scheduled_task` ideas); `dismiss` and `feedback` record the user's
//! signals so nightly generation can tune itself.

use crate::{bad_json, conflict, err_json, json_ok, App};
use pantheon_gateway::http::{Request, Response};
use pantheon_storage::{FeedbackSignal, Idea, IdeaKind, IdeaStatus, IdeaStore};
use std::collections::HashMap;

fn store(app: &App) -> Result<IdeaStore, Response> {
    IdeaStore::open(&app.data_dir)
        .map_err(|e| err_json(500, "IDEA", &format!("open ideas store: {e}")))
}

fn idea_json(i: &Idea) -> serde_json::Value {
    let mut v = serde_json::json!({
        "id": i.id,
        "title": i.title,
        "description": i.description,
        "includes": i.includes,
        "kind": match i.kind {
            IdeaKind::General => "general",
            IdeaKind::ScheduledTask => "scheduled_task",
        },
        "status": match i.status {
            IdeaStatus::Pending => "pending",
            IdeaStatus::Accepted => "accepted",
            IdeaStatus::Dismissed => "dismissed",
            IdeaStatus::Done => "done",
        },
        "created_day": i.created_day,
    });
    // The schedule spec rides only on scheduled_task ideas, exactly the
    // shape the app renders.
    if let Some(s) = &i.schedule {
        v["schedule"] = serde_json::json!({
            "cron": s.cron,
            "deliver": s.deliver,
            "prompt": s.prompt,
        });
    }
    v
}

/// `GET /api/ideas`: all ideas, pending first, then newest day first.
pub fn list(app: &App) -> Response {
    let store = match store(app) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let ideas = match store.list() {
        Ok(v) => v,
        Err(e) => return err_json(500, "IDEA", &format!("list ideas: {e}")),
    };
    let mut ideas = ideas;
    ideas.sort_by(|a, b| {
        let pa = a.status == IdeaStatus::Pending;
        let pb = b.status == IdeaStatus::Pending;
        pb.cmp(&pa).then(b.created_day.cmp(&a.created_day))
    });
    json_ok(serde_json::json!({
        "ideas": ideas.iter().map(idea_json).collect::<Vec<_>>(),
    }))
}

/// Load a pending idea or answer 404/409.
fn pending_idea(app: &App, id: &str) -> Result<(IdeaStore, Idea), Response> {
    let store = store(app)?;
    let idea = match store.get(id) {
        Ok(Some(i)) => i,
        Ok(None) => return Err(err_json(404, "NOT_FOUND", "unknown idea")),
        Err(e) => return Err(err_json(500, "IDEA", &format!("get idea: {e}"))),
    };
    if idea.status != IdeaStatus::Pending {
        return Err(conflict(
            "IDEA_STATE",
            "idea is no longer pending; only pending ideas can be accepted or dismissed",
        ));
    }
    Ok((store, idea))
}

/// Fabricate the request `runs::create` expects, reusing its exact
/// admit-run + spawn-turn path instead of reimplementing it.
fn create_run_request(message: &str, title: &str) -> Request {
    Request {
        method: "POST".into(),
        path: "/api/runs".into(),
        query: HashMap::new(),
        headers: HashMap::new(),
        body: serde_json::json!({"message": message, "title": title})
            .to_string()
            .into_bytes(),
    }
}

fn buffered_body(resp: &Response) -> Option<(u16, serde_json::Value)> {
    match resp {
        Response::Buffered { status, body, .. } => {
            Some((*status, serde_json::from_slice(body).unwrap_or_default()))
        }
        _ => None,
    }
}

/// `POST /api/ideas/:id/accept`: spawn the work behind the idea.
/// `general` → a new run carrying the idea's plan; `scheduled_task` →
/// a scheduled job built from the proposed spec.
pub fn accept(app: &App, id: &str) -> Response {
    let (store, idea) = match pending_idea(app, id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    match idea.kind {
        IdeaKind::General => {
            let plan = format!(
                "Idea: {}\n\n{}\n\nPlan:\n{}",
                idea.title,
                truncate(&idea.description, 2000),
                idea.includes
                    .iter()
                    .map(|s| format!("- {s}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            let resp = crate::runs::create(app, &create_run_request(&plan, &idea.title));
            let (status, body) = match buffered_body(&resp) {
                Some(v) => v,
                None => {
                    return err_json(500, "IDEA", "run creation returned a streaming response");
                }
            };
            if status != 201 {
                // The run never started: leave the idea pending so the
                // user can retry. Propagate the real error shape.
                return resp;
            }
            let run_id = body
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if let Err(e) = finalize(&store, &idea, true) {
                return err_json(500, "IDEA", &format!("mark accepted: {e}"));
            }
            json_ok(serde_json::json!({
                "ok": true,
                "status": "accepted",
                "run_id": run_id,
            }))
        }
        IdeaKind::ScheduledTask => {
            let spec = match &idea.schedule {
                Some(s) => s,
                None => return bad_json("scheduled_task idea has no schedule spec"),
            };
            let body = serde_json::json!({
                "task": spec.prompt,
                "cron": spec.cron,
                "deliver": spec.deliver,
            });
            let req = Request {
                method: "POST".into(),
                path: "/api/schedule/jobs".into(),
                query: HashMap::new(),
                headers: HashMap::new(),
                body: body.to_string().into_bytes(),
            };
            // The shared schedule creation path: template expansion,
            // cron + deliver validation, the locked read-modify-write on
            // schedule.json. A 400 here (e.g. bad cron) leaves the idea
            // pending so the user can retry.
            let resp = crate::schedule::create_job(app, &req);
            let (status, v) = match buffered_body(&resp) {
                Some(v) => v,
                None => {
                    return err_json(500, "IDEA", "job creation returned a streaming response");
                }
            };
            if status != 200 {
                return resp;
            }
            let schedule_id = v
                .get("job")
                .and_then(|j| j.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if let Err(e) = finalize(&store, &idea, true) {
                return err_json(500, "IDEA", &format!("mark accepted: {e}"));
            }
            json_ok(serde_json::json!({
                "ok": true,
                "status": "accepted",
                "schedule_id": schedule_id,
            }))
        }
    }
}

/// `POST /api/ideas/:id/dismiss`: record the dismissal and its topic
/// signal (generation downranks repeatedly dismissed topics).
pub fn dismiss(app: &App, id: &str) -> Response {
    let (store, idea) = match pending_idea(app, id) {
        Ok(v) => v,
        Err(r) => return r,
    };
    if let Err(e) = finalize(&store, &idea, false) {
        return err_json(500, "IDEA", &format!("mark dismissed: {e}"));
    }
    json_ok(serde_json::json!({"ok": true, "status": "dismissed"}))
}

/// `POST /api/ideas/:id/feedback` with `{"signal":"more"}` or
/// `{"signal":"less"}`: explicit tuning feedback on the idea and topic.
pub fn feedback(app: &App, id: &str, req: &Request) -> Response {
    let store = match store(app) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let body: serde_json::Value = match serde_json::from_slice(&req.body) {
        Ok(v) => v,
        Err(e) => return bad_json(&format!("invalid JSON body: {e}")),
    };
    let signal = match body.get("signal").and_then(|v| v.as_str()) {
        Some("more") => FeedbackSignal::More,
        Some("less") => FeedbackSignal::Less,
        _ => return bad_json("field \"signal\" must be \"more\" or \"less\""),
    };
    match store.record_feedback(id, signal) {
        Ok(true) => json_ok(serde_json::json!({"ok": true})),
        Ok(false) => err_json(404, "NOT_FOUND", "unknown idea"),
        Err(e) => err_json(500, "IDEA", &format!("record feedback: {e}")),
    }
}

/// Durably mark the idea accepted/dismissed and bump its topic signal.
fn finalize(
    store: &IdeaStore,
    idea: &Idea,
    accepted: bool,
) -> Result<(), pantheon_api::error::PantheonError> {
    let status = if accepted {
        IdeaStatus::Accepted
    } else {
        IdeaStatus::Dismissed
    };
    store.set_status(&idea.id, status, Some(now_ms()))?;
    store.bump_topic(&idea.topic, accepted)?;
    Ok(())
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}
