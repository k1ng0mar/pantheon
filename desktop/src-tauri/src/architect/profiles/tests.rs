use super::*;
use crate::architect::{self, WorkerProfile};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{atomic::AtomicU8, Arc, Mutex},
};

struct Disk(PathBuf);
impl Disk {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("pantheon-team-profiles-{}", storage::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn database(&self) -> PathBuf {
        self.0.join("state.sqlite")
    }
}
impl Drop for Disk {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn setup() -> TeamSetup {
    TeamSetup {
        architect: ModelSelection {
            provider: "pantheon".into(),
            model: "gpt-test".into(),
            reasoning_effort: Some("high".into()),
        },
        config: TeamConfig {
            auto: false,
            roster_only: false,
            name: None,
            workers: vec![
                WorkerProfile {
                    id: "quick".into(),
                    name: "Quick".into(),
                    provider: "pantheon".into(),
                    model: "".into(),
                    reasoning_effort: Some("medium".into()),
                },
                WorkerProfile {
                    id: "builder".into(),
                    name: "Builder".into(),
                    provider: "pantheon".into(),
                    model: "default".into(),
                    reasoning_effort: None,
                },
            ],
            max_parallel: 2,
            max_rounds: 7,
            max_assignments: 15,
            workspace: "shared".into(),
            summary_limit: 1500,
            lead_guidance: None,
        },
    }
}
fn state(store: Store) -> AppState {
    AppState {
        store: Arc::new(Mutex::new(store)),
        agents: Arc::new(Mutex::new(HashMap::new())),
        terminals: Arc::new(Mutex::new(HashMap::new())),
        shutdown: AtomicU8::new(0),
    }
}
fn task(store: &Store) -> Session {
    let project = store.add_project(&std::env::temp_dir()).unwrap();
    store
        .create_session(&project.id, "pantheon", "sonnet", "full")
        .unwrap()
}
fn apply(
    state: &AppState,
    sid: &str,
    setup: &TeamSetup,
    profile: Option<&str>,
) -> Result<Option<TeamSnapshot>> {
    architect::configure_state(
        state,
        sid,
        Some(setup.config.clone()),
        Some(setup.architect.clone()),
        profile,
    )
}
fn library_json(store: &Store) -> serde_json::Value {
    serde_json::to_value(read(store).unwrap()).unwrap()
}

#[test]
fn saved_profiles_and_applied_defaults_survive_reopen_without_rewriting_existing_tasks() {
    let disk = Disk::new();
    let store = Store::open(&disk.database()).unwrap();
    let original = task(&store);
    let unrelated = task(&store);
    let state = state(store);
    let setup = setup();
    let profile = {
        let store = state.store.lock().unwrap();
        let library = save_profile(&store, None, "  Daily team  ".into(), setup.clone()).unwrap();
        assert!(!library.enabled);
        assert!(library.last_setup.is_none());
        assert_eq!(library.profiles[0].name, "Daily team");
        assert_eq!(store.session(&original.id).unwrap().provider, "pantheon");
        library.profiles[0].id.clone()
    };
    apply(&state, &original.id, &setup, Some(&profile)).unwrap();
    drop(state);
    let store = Store::open(&disk.database()).unwrap();
    let library = read(&store).unwrap();
    assert!(library.enabled);
    assert_eq!(library.active_profile_id.as_deref(), Some(profile.as_str()));
    assert_eq!(library.last_setup, Some(setup.clone()));
    assert_eq!(
        ModelSelection::from_session(&store.session(&original.id).unwrap()),
        setup.architect
    );
    assert_eq!(store.session(&unrelated.id).unwrap().provider, "pantheon");
    assert!(architect::load(&store, &unrelated.id).unwrap().is_none());
    assert_eq!(
        architect::load(&store, &original.id)
            .unwrap()
            .unwrap()
            .config,
        setup.config
    );
}
#[test]
fn new_user_tasks_default_to_full_without_overriding_requested_modes_or_saved_tasks() {
    for enabled in [false, true] {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let source = task(&store);
        let state = state(store);
        if enabled {
            apply(&state, &source.id, &setup(), None).unwrap();
        }
        let store = state.store.lock().unwrap();
        for provider in [
            "pantheon", "pantheon", "pantheon", "pantheon", "pantheon", "pantheon", "pantheon",
            "pantheon", "pantheon", "pantheon", "pantheon", "pantheon", "pantheon",
        ] {
            let existing = store
                .create_session(&source.project_id, provider, "default", "plan")
                .unwrap();
            for mode in [None, Some("ask"), Some("auto"), Some("plan"), Some("full")] {
                for details in [None, Some(("New board task", "Details"))] {
                    let created = create_user_task(
                        &store,
                        &source.project_id,
                        provider,
                        "default",
                        mode,
                        details,
                    )
                    .unwrap();
                    assert_eq!(created.mode, mode.unwrap_or("full"));
                }
            }
            assert_eq!(store.session(&existing.id).unwrap().mode, "plan");
        }
    }
}

#[test]
fn new_user_conversation_and_board_inherit_but_raw_workers_and_agent_tasks_do_not() {
    let store = Store::open(std::path::Path::new(":memory:")).unwrap();
    let source = task(&store);
    let state = state(store);
    let setup = setup();
    apply(&state, &source.id, &setup, None).unwrap();
    let store = state.store.lock().unwrap();
    for details in [None, Some(("Board task", "Acceptance criteria"))] {
        let created = create_user_task(
            &store,
            &source.project_id,
            "pantheon",
            "ignored",
            Some("full"),
            details,
        )
        .unwrap();
        assert_eq!(ModelSelection::from_session(&created), setup.architect);
        assert_eq!(created.mode, "full");
        assert_eq!(
            architect::load(&store, &created.id)
                .unwrap()
                .unwrap()
                .config,
            setup.config
        );
        assert!(store.messages(&created.id, None).unwrap().is_empty());
        if details.is_some() {
            assert_eq!(created.title, "Board task");
            assert_eq!(created.task_description, "Acceptance criteria");
        }
    }
    let worker = store
        .create_session(&source.project_id, "pantheon", "worker-model", "plan")
        .unwrap();
    assert_eq!(worker.provider, "pantheon");
    assert_eq!(worker.mode, "plan");
    assert!(architect::load(&store, &worker.id).unwrap().is_none());
    let agent_task = store
        .create_board_task(
            &source.project_id,
            "pantheon",
            "sonnet",
            "plan",
            "Agent assignment",
            "",
        )
        .unwrap();
    assert_eq!(agent_task.provider, "pantheon");
    assert!(architect::load(&store, &agent_task.id).unwrap().is_none());
    let count = store.sessions().unwrap().len();
    assert!(create_user_task(
        &store,
        &source.project_id,
        "pantheon",
        "default",
        Some("ask"),
        Some((" ", ""))
    )
    .is_err());
    assert_eq!(store.sessions().unwrap().len(), count);
}
#[test]
fn user_worktrees_inherit_defaults_and_agent_worktrees_preserve_source_selection() {
    let disk = Disk::new();
    let root = disk.0.join("project");
    std::fs::create_dir(&root).unwrap();
    for args in [
        &["init"][..],
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "Fixture",
        ],
    ] {
        let output = std::process::Command::new("git")
            .current_dir(&root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let store = Store::open(&disk.database()).unwrap();
    let project = store.add_project(&root).unwrap();
    let source = store
        .create_session(&project.id, "pantheon", "sonnet", "plan")
        .unwrap();
    let config_task = store
        .create_session(&project.id, "pantheon", "default", "ask")
        .unwrap();
    let state = state(store);
    let setup = setup();
    apply(&state, &config_task.id, &setup, None).unwrap();
    let user = crate::create_worktree_task_with_defaults(
        &state.store,
        &source.id,
        "user-team".into(),
        None,
        false,
        true,
    )
    .unwrap();
    assert_eq!(ModelSelection::from_session(&user), setup.architect);
    assert_eq!(user.mode, "full");
    assert_eq!(user.project_id, source.project_id);
    assert!(user.worktree_path.is_some());
    let agent = crate::create_worktree_task(&state.store, &source.id, "agent-task".into()).unwrap();
    assert_eq!(agent.provider, source.provider);
    assert_eq!(agent.mode, "plan");
    let store = state.store.lock().unwrap();
    assert!(architect::load(&store, &agent.id).unwrap().is_none());
    assert_eq!(store.projects().unwrap().len(), 1);
}
#[test]
fn template_update_delete_and_disable_keep_the_last_applied_setup() {
    let disk = Disk::new();
    let store = Store::open(&disk.database()).unwrap();
    let task = task(&store);
    let state = state(store);
    let setup = setup();
    let id = {
        let store = state.store.lock().unwrap();
        save_profile(&store, None, "Original".into(), setup.clone())
            .unwrap()
            .profiles[0]
            .id
            .clone()
    };
    apply(&state, &task.id, &setup, Some(&id)).unwrap();
    let mut changed = setup.clone();
    changed.config.max_rounds = 9;
    {
        let store = state.store.lock().unwrap();
        let renamed =
            save_profile(&store, Some(id.clone()), "Renamed".into(), setup.clone()).unwrap();
        assert_eq!(renamed.active_profile_id.as_deref(), Some(id.as_str()));
        let edited = save_profile(&store, Some(id.clone()), "Renamed".into(), changed).unwrap();
        assert_eq!(edited.last_setup, Some(setup.clone()));
        assert!(edited.active_profile_id.is_none());
        assert_eq!(
            architect::load(&store, &task.id).unwrap().unwrap().config,
            setup.config
        );
        let deleted = delete_profile(&store, &id).unwrap();
        assert!(deleted.enabled);
        assert_eq!(deleted.last_setup, Some(setup.clone()));
        assert!(deleted.profiles.is_empty());
    }
    architect::configure_state(&state, &task.id, None, None, None).unwrap();
    drop(state);
    let store = Store::open(&disk.database()).unwrap();
    let disabled = read(&store).unwrap();
    assert!(!disabled.enabled);
    assert_eq!(disabled.last_setup, Some(setup));
    let normal = create_user_task(
        &store,
        &task.project_id,
        "pantheon",
        "normal",
        Some("plan"),
        None,
    )
    .unwrap();
    assert_eq!(normal.provider, "pantheon");
    assert_eq!(normal.mode, "plan");
    assert!(architect::load(&store, &normal.id).unwrap().is_none());
}
#[test]
fn missing_library_migrates_latest_team_once_and_never_reenables_disabled_defaults() {
    let disk = Disk::new();
    let expected = setup();
    {
        let store = Store::open(&disk.database()).unwrap();
        let first = task(&store);
        let second = task(&store);
        for (task, time) in [(&first, 10), (&second, 20)] {
            apply_selection(&store, &task.id, &expected.architect).unwrap();
            let mut team = architect::new_team(task.id.clone(), expected.config.clone());
            if time == 10 {
                team.config.max_rounds = 2;
            }
            architect::save(&store, &mut team).unwrap();
            store.conn().execute("UPDATE architect_teams SET content=json_set(content,'$.updatedAt',?2) WHERE session_id=?1",params![task.id,time]).unwrap();
        }
        store
            .conn()
            .execute_batch("DROP TABLE architect_library; PRAGMA user_version=11;")
            .unwrap();
    }
    {
        let store = Store::open(&disk.database()).unwrap();
        let seeded = read(&store).unwrap();
        assert!(seeded.enabled);
        assert_eq!(seeded.last_setup, Some(expected.clone()));
        assert!(seeded.profiles.is_empty());
        remember(&store, None, None).unwrap();
    }
    let store = Store::open(&disk.database()).unwrap();
    assert!(!read(&store).unwrap().enabled);
    assert_eq!(read(&store).unwrap().last_setup, Some(expected));
    assert_eq!(
        store
            .conn()
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .unwrap(),
        crate::storage::SCHEMA_VERSION
    );
}
#[test]
fn profile_validation_uniqueness_capacity_and_invalid_ids_are_atomic() {
    let store = Store::open(std::path::Path::new(":memory:")).unwrap();
    let setup = setup();
    save_profile(&store, None, "Build".into(), setup.clone()).unwrap();
    let before = library_json(&store);
    for name in [" ".into(), "bUiLd".into(), "a\nb".into(), "x".repeat(81)] {
        assert!(save_profile(&store, None, name, setup.clone()).is_err());
        assert_eq!(library_json(&store), before);
    }
    let mut bad = setup.clone();
    bad.architect.provider = "unknown".into();
    assert!(save_profile(&store, None, "Bad provider".into(), bad).is_err());
    let mut bad = setup.clone();
    bad.architect.reasoning_effort = Some("invalid effort".into());
    assert!(save_profile(&store, None, "Bad effort".into(), bad).is_err());
    assert!(save_profile(
        &store,
        Some("missing".into()),
        "Missing".into(),
        setup.clone()
    )
    .is_err());
    assert!(delete_profile(&store, "missing").is_err());
    assert_eq!(library_json(&store), before);
    for index in 1..20 {
        save_profile(&store, None, format!("Profile {index}"), setup.clone()).unwrap();
    }
    assert!(save_profile(&store, None, "Twenty one".into(), setup).is_err());
    assert_eq!(read(&store).unwrap().profiles.len(), 20);
}
#[test]
fn mismatched_profile_or_storage_failure_roll_back_task_selection_and_library() {
    let store = Store::open(std::path::Path::new(":memory:")).unwrap();
    let task = task(&store);
    let setup = setup();
    let id = save_profile(&store, None, "Saved".into(), setup.clone())
        .unwrap()
        .profiles[0]
        .id
        .clone();
    let before = library_json(&store);
    let state = state(store);
    let mut mismatched = setup.clone();
    mismatched.config.max_parallel = 1;
    for (setup, profile) in [(&mismatched, Some(id.as_str())), (&setup, Some("missing"))] {
        assert!(apply(&state, &task.id, setup, profile).is_err());
        let store = state.store.lock().unwrap();
        assert_eq!(store.session(&task.id).unwrap().provider, task.provider);
        assert!(architect::load(&store, &task.id).unwrap().is_none());
        assert_eq!(library_json(&store), before);
    }
    state.store.lock().unwrap().conn().execute_batch("CREATE TRIGGER reject_defaults BEFORE UPDATE ON architect_library BEGIN SELECT RAISE(ABORT,'fixture write failure'); END;").unwrap();
    assert!(apply(&state, &task.id, &setup, Some(&id)).is_err());
    let store = state.store.lock().unwrap();
    assert_eq!(
        ModelSelection::from_session(&store.session(&task.id).unwrap()),
        ModelSelection::from_session(&task)
    );
    assert!(architect::load(&store, &task.id).unwrap().is_none());
    assert_eq!(library_json(&store), before);
}
#[test]
fn used_teams_switch_setup_in_place_and_composer_can_change_the_architect_model() {
    let store = Store::open(std::path::Path::new(":memory:")).unwrap();
    let task = task(&store);
    store
        .message(&task.id, "user", "Keep my conversation")
        .unwrap();
    store.resume(&task.id, "existing-provider-context").unwrap();
    // A row left behind by an older build cannot silently become a Pantheon
    // architect conversation; the handoff needs a fresh task.
    store
        .conn()
        .execute(
            "UPDATE sessions SET provider='legacy' WHERE id=?1",
            [&task.id],
        )
        .unwrap();
    let state = state(store);
    let mut setup = setup();
    assert!(apply(&state, &task.id, &setup, None).is_err());
    state
        .store
        .lock()
        .unwrap()
        .conn()
        .execute(
            "UPDATE sessions SET provider='pantheon' WHERE id=?1",
            [&task.id],
        )
        .unwrap();
    setup.architect = ModelSelection {
        provider: "pantheon".into(),
        model: "another-model".into(),
        reasoning_effort: Some("high".into()),
    };
    apply(&state, &task.id, &setup, None).unwrap();
    {
        let store = state.store.lock().unwrap();
        let mut team = architect::load(&store, &task.id).unwrap().unwrap();
        team.round = 1;
        team.status = "paused".into();
        architect::save(&store, &mut team).unwrap();
        // A team run assembles its own prompt, so the native context is fresh
        // and the transcript is replayed through context recovery instead.
        assert!(store.session(&task.id).unwrap().resume_id.is_none());
        assert!(
            crate::context_recovery::pending(&store, &store.session(&task.id).unwrap()).unwrap()
        );
        assert_eq!(
            store
                .messages(&task.id, None)
                .unwrap()
                .first()
                .unwrap()
                .content,
            "Keep my conversation"
        );
    }
    {
        let store = state.store.lock().unwrap();
        assert!(store
            .configure(&task.id, "pantheon", "changed-via-composer", None)
            .is_ok());
        assert!(store.set_reasoning_effort(&task.id, Some("low")).is_ok());
        assert!(store
            .configure(&task.id, "pantheon", &setup.architect.model, None)
            .is_ok());
    }
    setup.architect.model = "changed-again".into();
    setup.config.workers[0].model = "changed-worker".into();
    setup.config.max_rounds = 12;
    assert_eq!(
        apply(&state, &task.id, &setup, None)
            .unwrap()
            .unwrap()
            .status,
        "paused"
    );
    let store = state.store.lock().unwrap();
    assert_eq!(store.session(&task.id).unwrap().model, "changed-again");
    // Teams drive their own prompt, so the task moves to a fresh context with
    // the earlier transcript queued for replay.
    assert!(store.session(&task.id).unwrap().resume_id.is_none());
    assert!(crate::context_recovery::pending(&store, &store.session(&task.id).unwrap()).unwrap());
    assert_eq!(
        architect::load(&store, &task.id).unwrap().unwrap().config,
        setup.config
    );
}

#[test]
fn saving_ordinary_mode_preserves_existing_context_for_every_provider() {
    for provider in ["pantheon", "pantheon", "pantheon"] {
        let disk = Disk::new();
        let store = Store::open(&disk.database()).unwrap();
        let project = store.add_project(&std::env::temp_dir()).unwrap();
        let task = store
            .create_session(&project.id, provider, "default", "ask")
            .unwrap();
        store
            .message(&task.id, "user", "Write a C++ greeting.")
            .unwrap();
        store
            .message(&task.id, "assistant", "int main() {}")
            .unwrap();
        store.resume(&task.id, "existing-context").unwrap();
        let state = state(store);
        architect::configure_state(&state, &task.id, None, None, None).unwrap();
        drop(state);
        let reopened = Store::open(&disk.database()).unwrap();
        assert_eq!(
            reopened.session(&task.id).unwrap().resume_id.as_deref(),
            Some("existing-context"),
            "Saving ordinary mode must not reset {provider}"
        );
        assert_eq!(reopened.messages(&task.id, None).unwrap().len(), 2);
        assert!(
            !crate::context_recovery::pending(&reopened, &reopened.session(&task.id).unwrap())
                .unwrap()
        );
    }
}

#[test]
fn enabling_a_team_keeps_the_transcript_and_starts_a_fresh_context() {
    for provider in ["pantheon"] {
        let store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let project = store.add_project(&std::env::temp_dir()).unwrap();
        let task = store
            .create_session(&project.id, provider, "default", "ask")
            .unwrap();
        store
            .message(&task.id, "assistant", "int main() {}")
            .unwrap();
        store.resume(&task.id, "existing-context").unwrap();
        store
            .conn()
            .execute(
                "UPDATE sessions SET context_used_tokens=1234,context_window=100000 WHERE id=?1",
                [&task.id],
            )
            .unwrap();
        let state = state(store);
        let mut setup = setup();
        setup.architect = ModelSelection::from_session(&task);
        for enabled in [true, true, false, false] {
            if enabled {
                apply(&state, &task.id, &setup, None).unwrap();
            } else {
                architect::configure_state(&state, &task.id, None, None, None).unwrap();
            }
            let store = state.store.lock().unwrap();
            let current = store.session(&task.id).unwrap();
            assert_eq!(current.provider, provider);
            let rows = store.messages(&task.id, None).unwrap();
            assert!(rows.iter().any(|m| m.content == "int main() {}"));
            if enabled {
                // A team run assembles a new system prompt, so the harness
                // gets a fresh context and the transcript is replayed into it.
                assert!(current.resume_id.is_none());
                assert!(current.context_usage.is_none());
                assert!(
                    crate::context_recovery::pending(&store, &current).unwrap(),
                    "the transcript must be carried into the new context"
                );
                let (prompt, _) =
                    crate::context_recovery::prepare(&store, &current, "Continue").unwrap();
                assert!(prompt.contains("int main() {}"));
            } else {
                // Leaving a team also rewrites the prompt, so the context is
                // rebuilt from the transcript either way.
                assert!(current.resume_id.is_none());
                assert!(crate::context_recovery::pending(&store, &current).unwrap());
            }
        }
    }
}

#[test]
fn resaving_team_settings_keeps_its_existing_provider_context() {
    let store = Store::open(std::path::Path::new(":memory:")).unwrap();
    let task = task(&store);
    let state = state(store);
    let mut setup = setup();
    apply(&state, &task.id, &setup, None).unwrap();
    state
        .store
        .lock()
        .unwrap()
        .resume(&task.id, "team-context")
        .unwrap();
    setup.config.max_rounds += 1;
    apply(&state, &task.id, &setup, None).unwrap();
    assert_eq!(
        state
            .store
            .lock()
            .unwrap()
            .session(&task.id)
            .unwrap()
            .resume_id
            .as_deref(),
        Some("team-context")
    );
}

#[test]
fn catalog_changes_replace_native_context_but_retain_local_transcript() {
    let store = Store::open(std::path::Path::new(":memory:")).unwrap();
    let project = store.add_project(&std::env::temp_dir()).unwrap();
    let task = store
        .create_session(&project.id, "pantheon", "default", "ask")
        .unwrap();
    store
        .message(&task.id, "user", "Write a C++ greeting.")
        .unwrap();
    store
        .message(&task.id, "assistant", "int main() {}")
        .unwrap();
    store.resume(&task.id, "ordinary-context").unwrap();
    let state = state(store);
    let mut setup = setup();
    setup.architect = ModelSelection::from_session(&task);
    apply(&state, &task.id, &setup, None).unwrap();
    {
        let store = state.store.lock().unwrap();
        assert!(store.session(&task.id).unwrap().resume_id.is_none());
        let messages = store.messages(&task.id, None).unwrap();
        assert_eq!(messages[0].content, "Write a C++ greeting.");
        assert_eq!(messages[1].content, "int main() {}");
        assert_eq!(
            messages[2].content,
            "Earlier conversation will be restored in the new architect context."
        );
        let current = store.session(&task.id).unwrap();
        let (prompt, through) =
            crate::context_recovery::prepare(&store, &current, "Put this in a file.").unwrap();
        assert!(prompt.contains("Write a C++ greeting."));
        assert!(prompt.contains("int main() {}"));
        assert_eq!(prompt.matches("int main() {}").count(), 1);
        assert!(prompt.ends_with("Put this in a file."));
        crate::context_recovery::acknowledge(&store, &task.id, through.unwrap()).unwrap();
        store.resume(&task.id, "team-context").unwrap();
        store
            .message(&task.id, "user", "Use greeting.cpp.")
            .unwrap();
        store
            .message(&task.id, "assistant", "Saved greeting.cpp.")
            .unwrap();
    }
    architect::configure_state(&state, &task.id, None, None, None).unwrap();
    let store = state.store.lock().unwrap();
    assert!(store.session(&task.id).unwrap().resume_id.is_none());
    assert!(architect::load(&store, &task.id).unwrap().is_none());
    assert_eq!(
        store.messages(&task.id, None).unwrap()[1].content,
        "int main() {}"
    );
    let current = store.session(&task.id).unwrap();
    let (prompt, through) =
        crate::context_recovery::prepare(&store, &current, "Change it.").unwrap();
    assert!(through.is_some());
    assert!(prompt.contains("int main() {}"));
    assert!(prompt.contains("Saved greeting.cpp."));
}

#[test]
fn failed_team_configuration_preserves_resume_and_messages() {
    let store = Store::open(std::path::Path::new(":memory:")).unwrap();
    let project = store.add_project(&std::env::temp_dir()).unwrap();
    let task = store
        .create_session(&project.id, "pantheon", "default", "ask")
        .unwrap();
    store
        .message(&task.id, "assistant", "int main() {}")
        .unwrap();
    store.resume(&task.id, "ordinary-context").unwrap();
    let state = state(store);
    let mut setup = setup();
    setup.architect = ModelSelection::from_session(&task);
    assert!(apply(&state, &task.id, &setup, Some("missing-profile")).is_err());
    let store = state.store.lock().unwrap();
    assert_eq!(
        store.session(&task.id).unwrap().resume_id.as_deref(),
        Some("ordinary-context")
    );
    assert!(architect::load(&store, &task.id).unwrap().is_none());
    assert_eq!(store.messages(&task.id, None).unwrap().len(), 1);
    assert!(!crate::context_recovery::pending(&store, &store.session(&task.id).unwrap()).unwrap());
}

#[test]
fn pending_team_setup_survives_restart_and_applies_before_explicit_resume() {
    let disk = Disk::new();
    let store = Store::open(&disk.database()).unwrap();
    let task = task(&store);
    let state = state(store);
    let mut current = setup();
    current.architect = ModelSelection::from_session(&task);
    apply(&state, &task.id, &current, None).unwrap();
    {
        let store = state.store.lock().unwrap();
        let mut team = architect::load(&store, &task.id).unwrap().unwrap();
        team.status = "running".into();
        team.generation = 1;
        team.round = 1;
        team.objective = "Keep the conversation and finish the task".into();
        architect::save(&store, &mut team).unwrap();
        store.resume(&task.id, "old-native-context").unwrap();
    }
    let (tx, _rx) = crate::unbounded_channel();
    let run = crate::prepare_task_run_in_project(
        &state,
        task.id.clone(),
        Some("Keep the conversation and finish the task".into()),
        vec![],
        tx,
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        crate::RunAdmission {
            team: Some(architect::Admission {
                parent: task.id.clone(),
                generation: 1,
            }),
            ..Default::default()
        },
    )
    .unwrap();
    state
        .store
        .lock()
        .unwrap()
        .message(&task.id, "assistant", "Persisted partial progress")
        .unwrap();
    let next = setup();
    assert!(apply(&state, &task.id, &next, None)
        .unwrap()
        .unwrap()
        .pending_setup
        .is_some());
    drop(run);
    drop(state);

    let store = Store::open(&disk.database()).unwrap();
    let team = architect::load(&store, &task.id).unwrap().unwrap();
    assert_eq!(team.status, "interrupted");
    assert_eq!(team.pending_setup, Some(next.clone()));
    assert_eq!(store.session(&task.id).unwrap().provider, "pantheon");
    let state = self::state(store);
    assert!(architect::next_action_state(&state, &mut |_| {})
        .unwrap()
        .is_none());
    let resumed = architect::control_state(&state, &task.id, "resume").unwrap();
    assert!(resumed.pending_setup.is_none());
    assert_eq!(resumed.config, next.config);
    let store = state.store.lock().unwrap();
    let session = store.session(&task.id).unwrap();
    assert_eq!(ModelSelection::from_session(&session), next.architect);
    assert_eq!(store.messages(&task.id, None).unwrap().len(), 2);
    let (prompt, _) =
        crate::context_recovery::prepare(&store, &session, "Resume the task").unwrap();
    assert!(
        !prompt.is_empty(),
        "the new context receives the saved transcript"
    );
}

#[test]
fn auto_team_default_keeps_the_new_tasks_own_model_as_lead() {
    let store = Store::open(std::path::Path::new(":memory:")).unwrap();
    let source = task(&store);
    let state = state(store);
    let mut auto = setup();
    auto.config.auto = true;
    apply(&state, &source.id, &auto, None).unwrap();
    let store = state.store.lock().unwrap();
    let created = create_user_task(
        &store,
        &source.project_id,
        "pantheon",
        "pantheon-chosen",
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        (created.provider.as_str(), created.model.as_str()),
        ("pantheon", "pantheon-chosen")
    );
    let team = architect::load(&store, &created.id).unwrap().unwrap();
    assert!(team.config.auto);
    assert_eq!(team.config.workers, auto.config.workers);
}
