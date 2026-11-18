use pantheon_runtime::harness::initialize_harness_artifacts;

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
