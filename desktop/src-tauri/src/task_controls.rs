//! Task-scoped native command workflows. Remote IDs and checkouts come from saved tasks.
use super::*;
use crate::goal_state::{GoalRequest, GoalState};
use serde_json::{json, Value};

fn context(state: &AppState, sid: &str) -> Result<(Session, PathBuf)> {
    let store = state.store.lock().map_err(|e| e.to_string())?;
    let task = store.session(sid)?;
    if task.provider != "pantheon" {
        return Err("Pantheon is the only provider this app can run.".into());
    }
    let cwd = store.workspace_root(&task.project_id, Some(sid))?;
    Ok((task, cwd))
}

#[tauri::command]
pub(super) async fn task_goal(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    request: GoalRequest,
) -> Result<Option<GoalState>> {
    if !matches!(request, GoalRequest::Get) {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        if crate::architect::load(&store, &session_id)?.is_some()
            || crate::architect::parent_of(&store, &session_id)?.is_some()
        {
            return Err(
                "Native goals cannot run inside an architect team. Use the team controls.".into(),
            );
        }
    }
    let (task, _cwd) = context(&state, &session_id)?;
    if let GoalRequest::Set {
        objective,
        token_budget,
    } = &request
    {
        if objective.trim().is_empty()
            || objective.len() > 32 * 1024
            || token_budget.is_some_and(|n| n == 0 || n > 9_007_199_254_740_991)
        {
            return Err("Enter a goal under 32 KiB and an optional positive token budget.".into());
        }
    }

    // A Pantheon goal is session state: an objective that rides the system
    // prompt for as long as it is active, plus the bookkeeping this panel
    // shows. Nothing about it is a remote protocol call, so reading, pausing,
    // resuming and clearing answer from the store and never start an agent.
    let previous = task.goal_state.clone();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let saved = match &request {
        GoalRequest::Get => previous.clone(),
        GoalRequest::Set {
            objective,
            token_budget,
        } => Some(GoalState {
            thread_id: session_id.clone(),
            objective: objective.trim().to_string(),
            status: "active".into(),
            token_budget: *token_budget,
            tokens_used: previous.as_ref().map(|g| g.tokens_used).unwrap_or(0),
            time_used_seconds: previous.as_ref().map(|g| g.time_used_seconds).unwrap_or(0),
            created_at: previous.as_ref().map(|g| g.created_at).unwrap_or(now),
            updated_at: now,
        }),
        GoalRequest::Pause => previous.clone().map(|mut goal| {
            goal.status = "paused".into();
            goal.updated_at = now;
            goal
        }),
        GoalRequest::Resume => previous.clone().map(|mut goal| {
            goal.status = "active".into();
            goal.updated_at = now;
            goal
        }),
        GoalRequest::Clear => None,
    };

    if !matches!(request, GoalRequest::Get) {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        store.record_goal(&session_id, saved.as_ref())?;
        let mut changed = super::event(&session_id, "goal", "");
        changed.data = serde_json::to_value(&saved).ok();
        let _ = app.emit("agent-event", changed);
    }
    if let GoalRequest::Set { objective, .. } = &request {
        // Setting a goal also asks the model to start on it. The objective is
        // stored above, so the run carries it as `goal_objective` and the
        // provider registers it with Pantheon before the turn.
        let text = objective.clone();
        launch_operation(app, state, session_id, Some(text), None, None).await?;
    }
    Ok(saved)
}

/// The panels this app can answer. Usage, plugins and hooks come from local
/// Pantheon sources; conversations come from this app's own task list.
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "action", rename_all = "camelCase", deny_unknown_fields)]
pub(super) enum PanelRequest {
    Usage,
    Plugins,
    Hooks,
    Conversations {
        #[serde(default)]
        cursor: Option<String>,
        #[serde(default)]
        search: Option<String>,
    },
}

#[tauri::command]
pub(super) async fn command_panel(
    state: State<'_, AppState>,
    session_id: String,
    request: PanelRequest,
) -> Result<Value> {
    let (task, cwd) = context(&state, &session_id)?;
    let request = match request {
        PanelRequest::Usage => command_operations::Request::Usage,
        PanelRequest::Plugins => command_operations::Request::Plugins,
        PanelRequest::Hooks => command_operations::Request::Hooks,
        PanelRequest::Conversations { cursor, search } => {
            return conversations(&state, &task, cursor.as_deref(), search.as_deref())
        }
    };
    command_operations::execute(&cwd, request).await
}

/// Other tasks in this app, newest first. Resuming one is a task switch: the
/// desktop already owns the transcript, so no agent round-trip is involved.
fn conversations(
    state: &State<'_, AppState>,
    current: &Session,
    cursor: Option<&str>,
    search: Option<&str>,
) -> Result<Value> {
    const PAGE: usize = 30;
    let store = state.store.lock().map_err(|e| e.to_string())?;
    let needle = search.unwrap_or_default().trim().to_lowercase();
    let after = cursor.and_then(|c| c.parse::<i64>().ok());
    let mut items: Vec<Value> = store
        .sessions()?
        .into_iter()
        .filter(|task| task.id != current.id)
        .filter(|task| {
            needle.is_empty()
                || task.title.to_lowercase().contains(&needle)
                || task.task_description.to_lowercase().contains(&needle)
        })
        .filter(|task| after.is_none_or(|bound| task.updated_at < bound))
        .take(PAGE + 1)
        .map(|task| {
            json!({
                "id": task.id,
                "title": task.title,
                "description": task.task_description,
                "updatedAt": task.updated_at,
            })
        })
        .collect();
    let next = if items.len() > PAGE {
        items.truncate(PAGE);
        items
            .last()
            .and_then(|item| item["updatedAt"].as_i64())
            .map(|at| at.to_string())
    } else {
        None
    };
    Ok(json!({
        "kind": "conversations",
        "mode": "resume",
        "items": items,
        "cursor": next,
        "notes": if items.is_empty() { json!(["No other conversations yet."]) } else { json!([]) },
    }))
}

#[tauri::command]
pub(super) async fn command_jobs(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    stop_id: Option<String>,
) -> Result<Value> {
    let (task, _) = context(&state, &session_id)?;
    // Pantheon runs one agent process per task and the console is a plain
    // shell, so "jobs" here means the work this app itself is running: this
    // task's turn, its architect team, and any other task still working.
    let running: Vec<(String, String)> = state
        .agents
        .lock()
        .map_err(|e| e.to_string())?
        .iter()
        .map(|(id, agent)| (id.clone(), agent.run_id.clone()))
        .collect();
    if let Some(id) = stop_id {
        if id != session_id {
            return Err("Only this task's own work can be stopped from here.".into());
        }
        if !running.iter().any(|(sid, _)| sid == &session_id) {
            return Err("That task is not running. Refresh the list.".into());
        }
        super::stop_session(app, state, session_id).await?;
        return Ok(
            json!({"kind":"result","title":"Task stopped","description":"Pantheon stopped the running turn."}),
        );
    }
    let mut jobs = Vec::new();
    if let Some((_, run_id)) = running.iter().find(|(sid, _)| sid == &session_id) {
        jobs.push(json!({
            "id": session_id,
            "title": format!("{} · turn running", task.title.chars().take(200).collect::<String>()),
            "status": "Running",
            "canStop": true,
        }));
        let _ = run_id;
    }
    let team = {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        crate::architect::load(&store, &session_id)?
    };
    if let Some(team) = team {
        for agent in team.workers.iter().filter(|w| w.status == "running") {
            jobs.push(json!({
                "id": agent.id,
                "title": format!("{} · team member", agent.name.chars().take(200).collect::<String>()),
                "status": "Running",
                "canStop": false,
            }));
        }
    }
    let notes = if jobs.is_empty() {
        vec!["Nothing is running in this task. Pantheon keeps no background shell jobs."]
    } else {
        Vec::new()
    };
    Ok(json!({"kind":"jobs","jobs":jobs,"notes":notes}))
}

// Reserve the task across a remote mutation without holding a mutex across await.
struct TaskLease {
    agents: Agents,
    sid: String,
    run_id: String,
}
impl TaskLease {
    fn acquire(state: &AppState, sid: &str) -> Result<Self> {
        let mut agents = state.agents.lock().map_err(|e| e.to_string())?;
        if state.shutdown.load(Ordering::SeqCst) != 0 {
            return Err("Pantheon is shutting down.".into());
        }
        if agents.contains_key(sid) {
            return Err("Stop this task before changing its conversation.".into());
        }
        {
            let store = state.store.lock().map_err(|e| e.to_string())?;
            crate::architect::check_manual(&store, sid)?;
        }
        let (tx, _rx) = unbounded_channel();
        let agent = RunningAgent::new(tx, Arc::new(AtomicBool::new(false)));
        let lease = Self {
            agents: state.agents.clone(),
            sid: sid.into(),
            run_id: agent.run_id.clone(),
        };
        agents.insert(sid.into(), agent);
        Ok(lease)
    }
}
impl Drop for TaskLease {
    fn drop(&mut self) {
        if let Ok(mut agents) = self.agents.lock() {
            if agents
                .get(&self.sid)
                .is_some_and(|a| a.run_id == self.run_id)
            {
                agents.remove(&self.sid);
            }
        }
    }
}

#[tauri::command]
pub(super) async fn fork_session_at_message(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    message_id: i64,
    exclusive: Option<bool>,
) -> Result<Session> {
    let store = state.store.clone();
    let task = blocking(move || {
        let store = store.lock().map_err(|e| e.to_string())?;
        if exclusive == Some(true) {
            crate::conversation_fork::before_message(&store, &session_id, message_id)
        } else {
            crate::conversation_fork::at_message(&store, &session_id, message_id)
        }
    })
    .await?;
    let _ = app.emit("sessions-changed", ());
    Ok(task)
}

#[tauri::command]
pub(super) async fn get_message_content(
    state: State<'_, AppState>,
    session_id: String,
    message_id: i64,
) -> Result<String> {
    let store = state.store.clone();
    blocking(move || {
        let store = store.lock().map_err(|e| e.to_string())?;
        store.message_content(&session_id, message_id)
    })
    .await
}

/// Fork a task's transcript locally: a new task in the same checkout whose
/// history is a copy up to `through_message` (or the newest message).
#[tauri::command]
pub(super) async fn fork_task(
    state: State<'_, AppState>,
    session_id: String,
    through_message: Option<i64>,
) -> Result<Session> {
    let _lease = TaskLease::acquire(&state, &session_id)?;
    let store = state.store.lock().map_err(|e| e.to_string())?;
    let boundary = match through_message {
        Some(id) => id,
        None => store
            .last_message_id(&session_id)?
            .ok_or("This task has no messages to fork yet.")?,
    };
    crate::conversation_fork::at_message(&store, &session_id, boundary)
}

#[tauri::command]
pub(super) async fn archive_task(state: State<'_, AppState>, session_id: String) -> Result<()> {
    let _lease = TaskLease::acquire(&state, &session_id)?;
    context(&state, &session_id)?;
    state
        .store
        .lock()
        .map_err(|e| e.to_string())?
        .update(&session_id, None, Some(true), None, None)
}

#[tauri::command]
pub(super) async fn export_task(state: State<'_, AppState>, session_id: String) -> Result<String> {
    let store = state.store.lock().map_err(|e| e.to_string())?;
    let task = store.session(&session_id)?;
    let mut output = format!("# {}\n\n", task.title);
    // Read the transcript under the connection, then drop the lock before
    // projecting turns, which looks up legacy coordination kinds.
    let rows = {
        let conn = store.conn();
        let mut query = conn
            .prepare("SELECT id,role,content FROM messages WHERE session_id=?1 ORDER BY id")
            .map_err(|e| e.to_string())?;
        let rows = query
            .query_map([&session_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?
    };
    for (id, role, content) in rows {
        let (role, content, _) = crate::architect::internal_turns::project_text(
            &store,
            &session_id,
            id,
            &role,
            &content,
        )?;
        if output.len() + content.len() > 8 * 1024 * 1024 {
            return Err("This transcript exceeds the 8 MiB export limit.".into());
        }
        output.push_str(&format!("## {role}\n\n{content}\n\n"));
    }
    Ok(output)
}

#[tauri::command]
pub(super) async fn save_task_export(
    state: State<'_, AppState>,
    session_id: String,
    path: String,
) -> Result<()> {
    let path = PathBuf::from(path);
    if !path.is_absolute()
        || !matches!(
            path.extension().and_then(|s| s.to_str()),
            Some("md" | "txt")
        )
    {
        return Err("Choose a Markdown or text file for the export.".into());
    }
    if std::fs::symlink_metadata(&path).is_ok_and(|m| !m.is_file() || m.file_type().is_symlink()) {
        return Err("Choose a regular file for the export.".into());
    }
    let text = export_task(state, session_id).await?;
    blocking(move || std::fs::write(path, text).map_err(|e| e.to_string())).await
}

pub(super) async fn rename_task(state: &AppState, sid: &str, title: &str) -> Result<()> {
    if title.trim().is_empty() || title.len() > 200 || title.chars().any(char::is_control) {
        return Err("Choose a task name of 1–200 characters.".into());
    }
    let _lease = TaskLease::acquire(state, sid)?;
    context(state, sid)?;
    state
        .store
        .lock()
        .map_err(|e| e.to_string())?
        .update(sid, Some(title.into()), None, None, None)
}

#[cfg(test)]
fn verify_console_task(store: &Store, expected: &Session) -> Result<Session> {
    let current = store.session(&expected.id)?;
    if current.provider != expected.provider
        || current.resume_id != expected.resume_id
        || current.project_id != expected.project_id
        || current.worktree_path != expected.worktree_path
        || current.model != expected.model
        || current.mode != expected.mode
        || current.reasoning_effort != expected.reasoning_effort
    {
        return Err("This task changed while its console was opening. Try again.".into());
    }
    Ok(current)
}
#[cfg(test)]
fn ensure_console_goal_inactive(goal: Option<&GoalState>) -> Result<()> {
    if goal.is_some_and(|goal| goal.status == "active") {
        return Err("Pause the goal before opening its console.".into());
    }
    Ok(())
}

#[cfg(test)]
mod command_tests {
    use super::*;
    use std::path::Path;

    fn state() -> AppState {
        AppState {
            store: Arc::new(Mutex::new(Store::open(Path::new(":memory:")).unwrap())),
            agents: Arc::new(Mutex::new(HashMap::new())),
            terminals: Arc::new(Mutex::new(HashMap::new())),
            shutdown: AtomicU8::new(0),
        }
    }
    #[test]
    fn console_uses_refreshed_goal_and_rejects_changes_to_its_native_session() {
        let store = Store::open(Path::new(":memory:")).unwrap();
        let project = store.add_project(&std::env::temp_dir()).unwrap();
        let task = store
            .create_session(&project.id, "pantheon", "default", "ask")
            .unwrap();
        let remote = uuid::Uuid::new_v4().to_string();
        store.resume(&task.id, &remote).unwrap();
        let expected = store.session(&task.id).unwrap();
        let mut goal = GoalState {
            thread_id: remote,
            objective: "Finish work".into(),
            status: "paused".into(),
            token_budget: None,
            tokens_used: 1,
            time_used_seconds: 1,
            created_at: 1,
            updated_at: 2,
        };
        store.record_goal(&task.id, Some(&goal)).unwrap();
        assert!(
            ensure_console_goal_inactive(store.session(&task.id).unwrap().goal_state.as_ref())
                .is_ok()
        );
        goal.status = "active".into();
        verify_console_task(&store, &expected).unwrap();
        store.record_goal(&task.id, Some(&goal)).unwrap();
        assert!(
            ensure_console_goal_inactive(store.session(&task.id).unwrap().goal_state.as_ref())
                .is_err()
        );
        store.record_goal(&task.id, None).unwrap();
        assert!(
            ensure_console_goal_inactive(store.session(&task.id).unwrap().goal_state.as_ref())
                .is_ok()
        );
        store
            .resume(&task.id, &uuid::Uuid::new_v4().to_string())
            .unwrap();
        assert!(verify_console_task(&store, &expected).is_err());
    }
    #[test]
    fn task_lease_preserves_other_owners_and_rejects_shutdown() {
        let state = state();
        let lease = TaskLease::acquire(&state, "one").unwrap();
        assert!(TaskLease::acquire(&state, "one").is_err());
        let (tx, _rx) = unbounded_channel();
        let replacement = RunningAgent::new(tx, Arc::new(AtomicBool::new(false)));
        let replacement_id = replacement.run_id.clone();
        state
            .agents
            .lock()
            .unwrap()
            .insert("one".into(), replacement);
        drop(lease);
        assert_eq!(state.agents.lock().unwrap()["one"].run_id, replacement_id);
        state.shutdown.store(1, Ordering::SeqCst);
        assert!(TaskLease::acquire(&state, "two").is_err());
        assert!(!state.agents.lock().unwrap().contains_key("two"));
    }
}
