use pantheon_runtime::harness::{
    cap_before_done_checks, initialize_harness_artifacts, run_scored_loop, score_against_rubric,
};

#[test]
fn initializer_writes_feature_list_and_progress_file() {
    let dir = tempfile::tempdir().unwrap();
    let list = initialize_harness_artifacts(dir.path(), "build a small chat app").unwrap();
    assert_eq!(list.features.len(), 1);
    assert_eq!(list.features[0].description, "build a small chat app");
    assert!(!list.features[0].passes);
    assert!(dir.path().join("feature_list.json").exists());
    assert!(dir.path().join("progress.txt").exists());
}

#[test]
fn rubric_scoring_returns_missing_criteria() {
    let (score, missing) = score_against_rubric(
        &["edit one file".into(), "run tests".into()],
        "Result: edit one file. Also wrote a note.",
    );
    assert!((score - 0.5).abs() < f64::EPSILON);
    assert_eq!(missing, vec!["run tests"]);
}

#[test]
fn judge_round_cap_stops_repeated_scoring() {
    let result = run_scored_loop(&["never satisfied".into()], "no matching evidence", 100);
    assert_eq!(result.rounds, 3);
    assert!(result.capped);
    assert!(result.score < 1.0);
}

#[test]
fn before_done_checks_are_capped() {
    assert_eq!(cap_before_done_checks(100), 5);
    assert_eq!(cap_before_done_checks(2), 2);
}
