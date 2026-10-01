//! `pantheon swarm` spawn path: arg parsing, profile resolution, and the
//! manifest-backed run loop driven by an injectable turn runner.
//!
//! Behavioral / integration tests per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval --test swarm_spawn`.
//!
//! No model, no network, no provider keys: `run_swarm_spawn` takes the turn
//! runner as a parameter, and the tests stub it. Temp dirs are real temp
//! dirs via `tempfile` and are removed when the guard drops.
use pantheon_tui::swarm::{
    load_all_swarms, parse_swarm_spawn, render_swarm_report, resolve_swarm_profiles,
    run_swarm_spawn, swarm_status_of, SpawnSpec,
};
use std::cell::Cell;
use std::path::Path;
use tempfile::tempdir;

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// Write a minimal config.toml into a temp data_dir. Each `[agents.<name>]`
/// table is a bare profile: every field optional, so an empty table
/// resolves to a standalone profile with the default policy.
fn declare_roles(data_dir: &Path, roles: &[&str]) {
    let mut body = String::new();
    for r in roles {
        body.push_str(&format!("[agents.{r}]\n"));
    }
    std::fs::write(data_dir.join("config.toml"), body).unwrap();
}

/// Parse args, declare + resolve the spec's roles, then drive the spawn with
/// the given stub. Returns the tempdir guard (keeps the dir alive) and the
/// outcomes.
fn spawn_with_stub(
    args: &[&str],
    run_turn: &dyn Fn(
        &pantheon_api::agent_profile::EffectiveProfile,
        &str,
        &str,
    ) -> Result<String, String>,
) -> (tempfile::TempDir, Vec<pantheon_tui::swarm::AgentOutcome>) {
    let dir = tempdir().unwrap();
    let spec: SpawnSpec = parse_swarm_spawn(&argv(args)).unwrap();
    declare_roles(
        dir.path(),
        &spec.roles.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
    );
    let profiles = resolve_swarm_profiles(dir.path(), &spec.roles).unwrap();
    assert_eq!(profiles.len(), spec.n);
    let outcomes = run_swarm_spawn(dir.path(), &spec, &profiles, run_turn).unwrap();
    (dir, outcomes)
}

#[test]
fn default_spawn_names_agents_one_through_n() {
    let spec = parse_swarm_spawn(&argv(&["pantheon", "swarm", "5", "research X"])).unwrap();
    assert_eq!(spec.n, 5);
    assert_eq!(spec.task, "research X");
    assert_eq!(
        spec.roles,
        vec!["agent-1", "agent-2", "agent-3", "agent-4", "agent-5"]
    );
    assert_eq!(spec.delivery, None);
}

// The parser's documented ordering is `<N> "<task>" [--roles ...]`: once
// `--roles` appears, all following positionals are treated as roles.
// Flags-first (roles before the task) currently folds the task into the
// role list, so the task check fires: `Err("error: need a task ...")`.
#[test]
fn roles_comma_form_maps_positionally() {
    let spec = parse_swarm_spawn(&argv(&[
        "pantheon",
        "swarm",
        "2",
        "do Y",
        "--roles",
        "researcher,critic",
    ]))
    .unwrap();
    assert_eq!(spec.roles, vec!["researcher", "critic"]);
    assert_eq!(spec.task, "do Y");
}

#[test]
fn roles_flags_before_task_parses() {
    // Umar's form: `pantheon swarm 5 --roles a,b,c,d,e "research X"`.
    // `--roles` takes exactly one comma-separated value, so flags may
    // precede the task.
    let spec = parse_swarm_spawn(&argv(&[
        "pantheon",
        "swarm",
        "2",
        "--roles",
        "researcher,critic",
        "do Y",
    ]))
    .unwrap();
    assert_eq!(spec.roles, vec!["researcher", "critic"]);
    assert_eq!(spec.task, "do Y");
}

#[test]
fn roles_multi_token_form_is_rejected() {
    // `--roles researcher critic` is ambiguous with the task, so the
    // parser takes only the single following token as the role list;
    // the leftover token joins the task and the count check fires.
    let err = parse_swarm_spawn(&argv(&[
        "pantheon",
        "swarm",
        "2",
        "do Y",
        "--roles",
        "researcher",
        "critic",
    ]))
    .unwrap_err();
    assert!(
        err.contains("1 role") && err.contains("2 agent"),
        "unexpected message: {err}"
    );
}

#[test]
fn roles_count_mismatch_names_both_counts() {
    let err = parse_swarm_spawn(&argv(&["pantheon", "swarm", "3", "do Y", "--roles", "a,b"]))
        .unwrap_err();
    assert!(
        err.contains("2 role") && err.contains("3 agent"),
        "unexpected message: {err}"
    );
}

#[test]
fn arg_errors_are_all_rejected() {
    for args in [
        // n = 0
        argv(&["pantheon", "swarm", "0", "task"]),
        // n above the 20-agent cap
        argv(&["pantheon", "swarm", "21", "task"]),
        // non-numeric n
        argv(&["pantheon", "swarm", "five", "task"]),
        // missing task
        argv(&["pantheon", "swarm", "2"]),
        // missing n entirely
        argv(&["pantheon", "swarm"]),
    ] {
        assert!(parse_swarm_spawn(&args).is_err(), "accepted: {args:?}");
    }
}

#[test]
fn missing_profile_names_the_role() {
    let dir = tempdir().unwrap();
    declare_roles(dir.path(), &["other"]);
    let err = resolve_swarm_profiles(dir.path(), &["researcher".to_string()]).unwrap_err();
    assert!(
        err.contains("researcher"),
        "error should name the missing role: {err}"
    );
}

#[test]
fn declared_profiles_resolve() {
    let dir = tempdir().unwrap();
    declare_roles(dir.path(), &["researcher", "critic"]);
    let profiles = resolve_swarm_profiles(
        dir.path(),
        &["researcher".to_string(), "critic".to_string()],
    )
    .unwrap();
    assert_eq!(profiles.len(), 2);
}

#[test]
fn successful_spawn_is_recorded_for_list_and_status() {
    let (dir, outcomes) = spawn_with_stub(&["pantheon", "swarm", "3", "research X"], &|_, _, _| {
        Ok("done".to_string())
    });
    assert_eq!(outcomes.len(), 3);
    assert!(outcomes.iter().all(|o| o.result.is_ok()));

    let swarms = load_all_swarms(dir.path());
    assert_eq!(swarms.len(), 1, "expected one recorded swarm");
    let rec = &swarms[0];
    assert_eq!(rec.agents.len(), 3);
    assert_eq!(rec.task, "research X");
    assert_eq!(rec.status, "complete");
    assert_eq!(swarm_status_of(&outcomes), "complete");
    // Manifest agent entries carry the run id and a final status.
    for a in &rec.agents {
        assert!(!a.run_id.is_empty());
        assert_eq!(a.status, "ok");
    }
}

#[test]
fn partial_failure_is_honest_in_outcomes_report_and_manifest() {
    let calls = Cell::new(0usize);
    let (dir, outcomes) = spawn_with_stub(&["pantheon", "swarm", "3", "research X"], &|_, _, _| {
        let i = calls.get();
        calls.set(i + 1);
        if i == 1 {
            Err("boom".to_string())
        } else {
            Ok(format!("agent {i} done"))
        }
    });

    // The error is visible on the failing outcome, verbatim.
    assert!(outcomes[0].result.is_ok());
    let err = outcomes[1].result.as_ref().unwrap_err();
    assert!(err.contains("boom"), "unexpected error: {err}");
    assert!(outcomes[2].result.is_ok());

    assert_eq!(swarm_status_of(&outcomes), "partial");

    let report = render_swarm_report(&outcomes);
    assert!(report.contains("boom"), "report hides the error:\n{report}");
    for o in &outcomes {
        let header = format!("=== {} ({}) ===", o.label, o.role);
        assert!(
            report.contains(&header),
            "report missing header {header:?}:\n{report}"
        );
    }

    let swarms = load_all_swarms(dir.path());
    assert_eq!(swarms.len(), 1);
    assert_eq!(swarms[0].status, "partial");
    let statuses: Vec<&str> = swarms[0].agents.iter().map(|a| a.status.as_str()).collect();
    assert_eq!(statuses, vec!["ok", "error", "ok"]);
}

#[test]
fn all_agents_failing_reports_failed() {
    let (_dir, outcomes) =
        spawn_with_stub(&["pantheon", "swarm", "2", "research X"], &|_, _, _| {
            Err("total outage".to_string())
        });
    assert_eq!(swarm_status_of(&outcomes), "failed");
    let report = render_swarm_report(&outcomes);
    assert!(report.contains("total outage"));
}
