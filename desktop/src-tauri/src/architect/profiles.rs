//! User-owned reusable team setups. Runtime worker sessions never inherit this library.
use super::{TeamConfig, TeamSnapshot};
use crate::{
    storage::{self, Session, Store},
    AppState, Result,
};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use tauri::State;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelSelection {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}
impl ModelSelection {
    pub fn from_session(session: &Session) -> Self {
        Self {
            provider: session.provider.clone(),
            model: session.model.clone(),
            reasoning_effort: session.reasoning_effort.clone(),
        }
    }
    fn validate(&self) -> Result<()> {
        if ![
            "claude",
            "codex",
            "kimi",
            "opencode",
            "glm",
            "antigravity",
            "grok",
            "mimo",
            "pi",
            "deepseek",
            "commandcode",
            "cline",
            "kilo",
            "omp",
            "github-copilot",
            "muse",
        ]
        .contains(&self.provider.as_str())
            || self.model.len() > 200
            || self.model.contains('\0')
        {
            return Err("Choose a supported harness and a model ID under 200 bytes.".into());
        }
        crate::providers::validate_reasoning_effort(
            &self.provider,
            self.reasoning_effort.as_deref(),
        )
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TeamSetup {
    pub architect: ModelSelection,
    pub config: TeamConfig,
}
impl TeamSetup {
    pub fn validate(&self) -> Result<()> {
        self.architect.validate()?;
        self.config.validate()
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TeamProfile {
    pub id: String,
    pub name: String,
    pub setup: TeamSetup,
    pub updated_at: i64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TeamLibrary {
    pub enabled: bool,
    pub last_setup: Option<TeamSetup>,
    pub active_profile_id: Option<String>,
    pub profiles: Vec<TeamProfile>,
    /// Lead guidance for every team that does not set its own, from Settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lead_guidance: Option<String>,
}

pub(crate) fn initialize(store: &Store) -> Result<()> {
    let exists: bool = store
        .db.lock().unwrap().query_row(
            "SELECT EXISTS(SELECT 1 FROM architect_library WHERE id=1)",
            [],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if exists {
        return Ok(());
    }
    // Only a missing library is seeded. Explicitly disabling defaults is durable.
    let previous=store.db.lock().unwrap().query_row("SELECT s.provider,s.model,s.reasoning_effort,json_extract(t.content,'$.config') FROM architect_teams t JOIN sessions s ON s.id=t.session_id ORDER BY json_extract(t.content,'$.updatedAt') DESC LIMIT 1",[],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,String>(3)?))).optional().map_err(|e|e.to_string())?;
    let mut library = TeamLibrary::default();
    if let Some((provider, model, reasoning_effort, config)) = previous {
        let setup = TeamSetup {
            architect: ModelSelection {
                provider,
                model,
                reasoning_effort,
            },
            config: serde_json::from_str(&config).map_err(|e| e.to_string())?,
        };
        setup.validate()?;
        library.enabled = true;
        library.last_setup = Some(setup);
    }
    let content = serde_json::to_string(&library).map_err(|e| e.to_string())?;
    store
        .db.lock().unwrap().execute(
            "INSERT OR IGNORE INTO architect_library(id,content) VALUES(1,?1)",
            [content],
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}
pub(crate) fn read(store: &Store) -> Result<TeamLibrary> {
    read_on(&store.db.lock().unwrap())
}
fn read_on(conn: &rusqlite::Connection) -> Result<TeamLibrary> {
    let content = conn
        .query_row(
            "SELECT content FROM architect_library WHERE id=1",
            [],
            |r| r.get::<_, String>(0),
        )
        .map_err(|e| e.to_string())?;
    serde_json::from_str(&content).map_err(|e| format!("Saved team library is invalid: {e}"))
}
fn write(store: &Store, library: &TeamLibrary) -> Result<()> {
    write_on(&store.db.lock().unwrap(), library)
}
fn write_on(conn: &rusqlite::Connection, library: &TeamLibrary) -> Result<()> {
    let content = serde_json::to_string(library).map_err(|e| e.to_string())?;
    if content.len() > 256 * 1024 {
        return Err("Saved team setups exceed their storage limit.".into());
    }
    conn.execute(
        "UPDATE architect_library SET content=?1 WHERE id=1",
        [content],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}
fn validate_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 80 || name.chars().any(char::is_control) {
        return Err("Profile names must be 1–80 characters without control characters.".into());
    }
    Ok(name.into())
}
pub(crate) fn save_profile(
    store: &Store,
    id: Option<String>,
    name: String,
    setup: TeamSetup,
) -> Result<TeamLibrary> {
    setup.validate()?;
    let name = validate_name(&name)?;
    let conn = store.db.lock().unwrap();
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    let mut library = read_on(&tx)?;
    if library
        .profiles
        .iter()
        .any(|p| p.name.to_lowercase() == name.to_lowercase() && Some(&p.id) != id.as_ref())
    {
        return Err("A team profile with this name already exists.".into());
    }
    if let Some(id) = id {
        let profile = library
            .profiles
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or("This team profile no longer exists.")?;
        profile.name = name;
        profile.setup = setup;
        profile.updated_at = storage::now();
    } else {
        if library.profiles.len() >= 20 {
            return Err("Save up to 20 team profiles. Update or delete one first.".into());
        }
        library.profiles.push(TeamProfile {
            id: storage::id(),
            name,
            setup,
            updated_at: storage::now(),
        });
    }
    // Named profile edits never silently replace the applied default or current task.
    if library.active_profile_id.as_ref().is_some_and(|id| {
        library
            .profiles
            .iter()
            .find(|p| p.id == *id)
            .is_none_or(|p| Some(&p.setup) != library.last_setup.as_ref())
    }) {
        library.active_profile_id = None;
    }
    write_on(&tx, &library)?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(library)
}
pub(crate) fn delete_profile(store: &Store, id: &str) -> Result<TeamLibrary> {
    let conn = store.db.lock().unwrap();
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    let mut library = read_on(&tx)?;
    let index = library
        .profiles
        .iter()
        .position(|p| p.id == id)
        .ok_or("This team profile no longer exists.")?;
    library.profiles.remove(index);
    if library.active_profile_id.as_deref() == Some(id) {
        library.active_profile_id = None;
    }
    write_on(&tx, &library)?;
    tx.commit().map_err(|e| e.to_string())?;
    Ok(library)
}
/// Test-only entry point for `remember_on`: takes the store lock itself so a test
/// does not have to hold a transaction.
#[cfg(test)]
pub(super) fn remember(
    store: &Store,
    setup: Option<TeamSetup>,
    profile_id: Option<&str>,
) -> Result<()> {
    remember_on(&store.db.lock().unwrap(), setup, profile_id)
}
/// Must share the task configuration transaction so Save setup is all-or-nothing.
pub(super) fn remember_on(
    conn: &rusqlite::Connection,
    setup: Option<TeamSetup>,
    profile_id: Option<&str>,
) -> Result<()> {
    let mut library = read_on(conn)?;
    if let Some(setup) = setup {
        setup.validate()?;
        let profile = profile_id
            .map(|id| {
                library
                    .profiles
                    .iter()
                    .find(|p| p.id == id)
                    .ok_or("This team profile no longer exists.")
            })
            .transpose()?;
        if profile.is_some_and(|p| p.setup != setup) {
            return Err("The selected profile does not match this team setup. Save it as a custom setup or update the profile first.".into());
        }
        library.active_profile_id = profile.map(|p| p.id.clone());
        library.enabled = true;
        library.last_setup = Some(setup);
    } else {
        library.enabled = false;
    }
    write_on(conn, &library)
}
/// Reject a missing or mismatched profile before any session writes.
pub(super) fn check_remember_on(
    conn: &rusqlite::Connection,
    setup: &TeamSetup,
    profile_id: Option<&str>,
) -> Result<()> {
    setup.validate()?;
    let library = read_on(conn)?;
    if let Some(id) = profile_id {
        let profile = library
            .profiles
            .iter()
            .find(|p| p.id == id)
            .ok_or("This team profile no longer exists.")?;
        if profile.setup != *setup {
            return Err("The selected profile does not match this team setup. Save it as a custom setup or update the profile first.".into());
        }
    }
    Ok(())
}
/// Test-only entry point for `apply_selection_on`: takes the store lock itself.
#[cfg(test)]
pub(super) fn apply_selection(store: &Store, sid: &str, selection: &ModelSelection) -> Result<()> {
    apply_selection_on(&store.db.lock().unwrap(), sid, selection)
}
pub(super) fn apply_selection_on(
    conn: &rusqlite::Connection,
    sid: &str,
    selection: &ModelSelection,
) -> Result<()> {
    selection.validate()?;
    conn.execute("UPDATE sessions SET provider=?2,model=?3,reasoning_effort=?4,context_used_tokens=NULL,context_window=NULL WHERE id=?1",params![sid,selection.provider,selection.model,selection.reasoning_effort]).map_err(|e|e.to_string())?;
    Ok(())
}
/// Called only by user-facing new-task entrypoints, inside their creation transaction.
pub(crate) fn apply_default_on(
    conn: &rusqlite::Connection,
    sid: &str,
) -> Result<Option<TeamSnapshot>> {
    let library = read_on(conn)?;
    if !library.enabled {
        return Ok(None);
    }
    let setup = library
        .last_setup
        .ok_or("The saved default team setup is missing.")?;
    setup.validate()?;
    let has_history: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE session_id=?1)",
            [sid],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    if has_history || super::parent_of_on(conn, sid)?.is_some() || super::load_on(conn, sid)?.is_some()
    {
        return Err("Default team settings only apply to a new user task.".into());
    }
    // An auto team is led by whatever model the new task was created with.
    if !setup.config.auto {
        apply_selection_on(conn, sid, &setup.architect)?;
    }
    let mut team = super::new_team(sid.into(), setup.config);
    super::save_on(conn, &mut team)?;
    Ok(Some(team))
}
pub(crate) fn create_user_task(
    store: &Store,
    project_id: &str,
    provider: &str,
    model: &str,
    mode: Option<&str>,
    details: Option<(&str, &str)>,
) -> Result<Session> {
    if let Some((title, description)) = details {
        Store::validate_task_details(title, description)?;
    }
    store.project(project_id)?;
    let sid = {
        let conn = store.db.lock().unwrap();
        let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
        let sid = Store::insert_new_session(
            &tx,
            project_id,
            provider,
            model,
            mode.unwrap_or(storage::DEFAULT_PERMISSION_MODE),
        )?;
        if let Some((title, description)) = details {
            tx.execute(
                "UPDATE sessions SET title=?2,title_origin='manual',task_description=?3 WHERE id=?1",
                params![sid, title.trim(), description.trim()],
            )
            .map_err(|e| e.to_string())?;
        }
        apply_default_on(&tx, &sid)?;
        tx.commit().map_err(|e| e.to_string())?;
        sid
    };
    store.session(&sid)
}
/// Replaces the app-wide lead guidance. Blank or absent means the built-in rules alone.
pub(crate) fn set_lead_guidance(
    store: &Store,
    guidance: Option<String>,
) -> Result<TeamLibrary> {
    let mut library = read(store)?;
    library.lead_guidance = guidance
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty());
    if let Some(guidance) = &library.lead_guidance {
        if guidance.len() > super::MAX_LEAD_GUIDANCE || guidance.contains('\0') {
            return Err(format!(
                "Lead guidance must be under {} bytes.",
                super::MAX_LEAD_GUIDANCE
            ));
        }
    }
    write(store, &library)?;
    Ok(library)
}
#[tauri::command]
pub async fn architect_profiles(state: State<'_, AppState>) -> Result<TeamLibrary> {
    let store = state.store.lock().map_err(|e| e.to_string())?;
    read(&store)
}
#[tauri::command]
pub async fn architect_save_profile(
    state: State<'_, AppState>,
    id: Option<String>,
    name: String,
    setup: TeamSetup,
) -> Result<TeamLibrary> {
    let store = state.store.lock().map_err(|e| e.to_string())?;
    save_profile(&store, id, name, setup)
}
#[tauri::command]
pub async fn architect_set_lead_guidance(
    state: State<'_, AppState>,
    guidance: Option<String>,
) -> Result<TeamLibrary> {
    let store = state.store.lock().map_err(|e| e.to_string())?;
    set_lead_guidance(&store, guidance)
}
#[tauri::command]
pub async fn architect_delete_profile(
    state: State<'_, AppState>,
    id: String,
) -> Result<TeamLibrary> {
    let store = state.store.lock().map_err(|e| e.to_string())?;
    delete_profile(&store, &id)
}
#[cfg(test)]
mod tests;
