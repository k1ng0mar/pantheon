use pantheon_runtime::harness::{initialize_harness_artifacts, score_against_rubric};

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
