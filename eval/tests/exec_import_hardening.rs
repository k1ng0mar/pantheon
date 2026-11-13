//! Remote imports cannot smuggle git options or local transports
//! (audit SEC-12, SEC-13).

use pantheon_exec::plugins::is_full_commit_sha;

#[test]
fn catalog_sha_must_be_a_full_commit_id() {
    assert!(is_full_commit_sha(
        "0123456789abcdef0123456789abcdef01234567"
    ));
    assert!(!is_full_commit_sha("--upload-pack=touch /tmp/pwned"));
    assert!(!is_full_commit_sha("main"));
    assert!(!is_full_commit_sha("0123456"));
}

#[test]
fn skill_repo_import_refuses_option_and_local_transport_urls() {
    let dir = tempfile::tempdir().unwrap();
    for repo in [
        "--upload-pack=touch /tmp/pantheon-sec12",
        "ext::sh -c touch% /tmp/pantheon-sec12",
        "file:///etc",
        "/etc",
        "http://example.com/repo.git",
    ] {
        let e =
            pantheon_exec::skills::import_skills_from_repo(dir.path(), repo, None).expect_err(repo);
        assert_eq!(e.code, "SKILL_REPO_URL", "{repo}: {e:?}");
    }
    assert!(!std::path::Path::new("/tmp/pantheon-sec12").exists());
}
