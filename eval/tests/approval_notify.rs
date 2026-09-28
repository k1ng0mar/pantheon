//! Phone approval notifications: scope parsing, redacted message
//! building, button callback round-trip, and grant/deny resolving the
//! pending scope through the supervisor.
//!
//! Behavioral tests per the test-hygiene policy.
//! Run with `cargo test -p pantheon-eval`.

use pantheon_api::events::Event;
use pantheon_gateway::channel::{parse_approval_callback, ApprovalAnswer};
use pantheon_runtime::Supervisor;
use pantheon_storage::Ledger;
use pantheon_tui::approval_notify::{
    build_notice, callback_data, format_message, parse_callback, parse_scope,
};

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "pantheon-eval-approval-{}-{}-{}",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn scope_parses_call_tool_and_args_with_colons() {
    // Args are JSON and may contain colons: only the first two are
    // separators.
    let scope = "turn_9-call_0_3:write_file:{\"path\":\"a:b.txt\",\"content\":\"x\"}";
    let (call, tool, args) = parse_scope(scope).expect("parses");
    assert_eq!(call, "turn_9-call_0_3");
    assert_eq!(tool, "write_file");
    assert_eq!(args, "{\"path\":\"a:b.txt\",\"content\":\"x\"}");
    assert!(parse_scope("no-colons").is_none());
}

#[test]
fn callback_round_trip_carries_run_id() {
    let scope = "turn_1-call_0_0:shell:{\"cmd\":\"ls\"}";
    let data = callback_data(true, "run_123_0001", scope);
    let (grant, run_id, out_scope) = parse_callback(&data).expect("parses");
    assert!(grant);
    assert_eq!(run_id.as_deref(), Some("run_123_0001"));
    assert_eq!(out_scope, scope);

    let data = callback_data(false, "run_123_0001", scope);
    let (grant, _, _) = parse_callback(&data).expect("parses");
    assert!(!grant);
}

#[test]
fn legacy_callback_without_run_id_still_parses() {
    // Buttons sent before the run-id format carry `grant:{scope}`.
    let (grant, run_id, scope) = parse_callback("deny:turn_1-call_0_0:shell:{}").expect("parses");
    assert!(!grant);
    assert_eq!(run_id, None);
    assert_eq!(scope, "turn_1-call_0_0:shell:{}");
    assert!(parse_callback("bogus").is_none());
}

#[test]
fn notifier_and_daemon_agree_on_callback_format() {
    // The daemon parses with pantheon_gateway; the notifier must produce
    // exactly what the daemon understands, including the run id.
    let scope = "turn_2-call_1_0:write_file:{\"path\":\"f\"}";
    let data = callback_data(false, "run_9_0007", scope);
    let (answer, run_id, out) = parse_approval_callback(&data).expect("parses");
    assert!(matches!(answer, ApprovalAnswer::Deny));
    assert_eq!(run_id.as_deref(), Some("run_9_0007"));
    assert_eq!(out, scope);
}

#[test]
fn notice_redacts_secrets_and_shows_diff() {
    let dir = temp_dir("notice");
    std::fs::write(dir.join("notes.txt"), "hello\n").unwrap();
    let scope = "turn_1-call_0_0:write_file:{\"path\":\"notes.txt\",\"content\":\"hello\\nworld\\n\",\"key\":\"sk-abc123\"}";
    let notice = build_notice("run_1_0001", scope, &dir).expect("notice");
    assert_eq!(notice.tool, "write_file");
    assert_eq!(notice.call_id, "turn_1-call_0_0");
    // Secrets are redacted in args and in the diff, never shipped raw.
    assert!(!notice.args_redacted.contains("sk-abc123"));
    assert!(notice.args_redacted.contains("[REDACTED]"));
    let diff = notice.diff.as_ref().expect("write_file gets a diff");
    assert!(!diff.contains("sk-abc123") || diff.contains("[REDACTED]"));
    let msg = format_message(&notice);
    assert!(msg.contains("write_file"));
    assert!(msg.contains("run_1_0001"));
    assert!(msg.contains("+world"));
    assert!(!msg.contains("sk-abc123"));
    // Buttons resolve back to this exact run and scope.
    let (grant, run_id, scope_out) = parse_callback(&notice.grant_callback).unwrap();
    assert!(grant && run_id.as_deref() == Some("run_1_0001"));
    assert_eq!(scope_out, scope);
}

#[test]
fn non_file_tool_gets_no_diff() {
    let dir = temp_dir("nodiff");
    let scope = "turn_1-call_0_0:shell:{\"cmd\":\"echo hi\"}";
    let notice = build_notice("run_2_0001", scope, &dir).expect("notice");
    assert!(notice.diff.is_none());
    assert!(format_message(&notice).contains("shell"));
}

fn parked_run(dir: &std::path::Path, run_id: &str, scope: &str) -> Supervisor {
    let sup = Supervisor::open(dir.to_path_buf()).expect("open supervisor");
    sup.start_run(run_id).expect("start run");
    Ledger::open(&dir.join("ledger.db"))
        .expect("open ledger")
        .append(&Event::ApprovalRequested {
            run_id: run_id.to_string(),
            scope: scope.to_string(),
        })
        .expect("park approval");
    sup
}

#[test]
fn phone_grant_resolves_pending_scope() {
    let dir = temp_dir("grant");
    let run_id = "run_100_0001";
    let scope = "turn_5-call_0_0:shell:{\"cmd\":\"ls\"}";
    let sup = parked_run(&dir, run_id, scope);
    assert_eq!(
        sup.pending_approvals(run_id).unwrap(),
        vec![scope.to_string()]
    );

    // Simulate the phone button: parse the callback, grant through the
    // supervisor exactly like the daemon does.
    let data = callback_data(true, run_id, scope);
    let (grant, cb_run, cb_scope) = parse_callback(&data).unwrap();
    assert!(grant);
    if grant {
        sup.grant(cb_run.as_deref().unwrap(), &cb_scope)
            .expect("grant resolves");
    }
    assert!(sup.pending_approvals(run_id).unwrap().is_empty());
    // Already resolved: a second grant is refused, not silently accepted.
    assert!(sup.grant(run_id, scope).is_err());
}

#[test]
fn phone_deny_resolves_pending_scope() {
    let dir = temp_dir("deny");
    let run_id = "run_100_0002";
    let scope = "turn_5-call_0_0:write_file:{\"path\":\"x\"}";
    let sup = parked_run(&dir, run_id, scope);

    let data = callback_data(false, run_id, scope);
    let (grant, cb_run, cb_scope) = parse_callback(&data).unwrap();
    assert!(!grant);
    if !grant {
        sup.deny(cb_run.as_deref().unwrap(), &cb_scope)
            .expect("deny resolves");
    }
    assert!(sup.pending_approvals(run_id).unwrap().is_empty());
}
