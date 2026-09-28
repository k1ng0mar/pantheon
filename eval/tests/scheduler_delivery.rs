//! Behavioral tests for scheduled-task delivery routing: per-channel
//! dispatch through a mock sender, redaction + truncation of the summary,
//! and the guarantee that delivery failure never fails the job.
use pantheon_gateway::schedule_delivery::{
    build_summary, deliver_best_effort, deliver_summary, ChannelSender, Deliver, RestChannelSender,
};
use std::sync::Mutex;

struct MockSender {
    calls: Mutex<Vec<(String, String)>>,
    fail: bool,
}

impl MockSender {
    fn ok() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            fail: false,
        }
    }
    fn failing() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            fail: true,
        }
    }
    fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap().clone()
    }
}

impl ChannelSender for MockSender {
    fn send_telegram(&self, text: &str) -> Result<(), String> {
        if self.fail {
            return Err("telegram down".into());
        }
        self.calls
            .lock()
            .unwrap()
            .push(("telegram".into(), text.into()));
        Ok(())
    }
    fn send_discord(&self, text: &str) -> Result<(), String> {
        if self.fail {
            return Err("discord down".into());
        }
        self.calls
            .lock()
            .unwrap()
            .push(("discord".into(), text.into()));
        Ok(())
    }
}

#[test]
fn deliver_parse_accepts_all_targets() {
    assert_eq!(Deliver::parse("log").unwrap(), Deliver::Log);
    assert_eq!(Deliver::parse("telegram").unwrap(), Deliver::Telegram);
    assert_eq!(Deliver::parse("discord").unwrap(), Deliver::Discord);
    assert_eq!(Deliver::parse("notify").unwrap(), Deliver::Notify);
    assert!(matches!(
        Deliver::parse("file:/tmp/hits.log").unwrap(),
        Deliver::File(_)
    ));
    assert!(Deliver::parse("file:").is_err(), "empty file path");
    assert!(Deliver::parse("pigeon").is_err());
    assert!(Deliver::parse("").is_err());
}

#[test]
fn telegram_routes_to_the_telegram_sender() {
    let s = MockSender::ok();
    deliver_summary(&s, &Deliver::Telegram, "job1", "did the thing").unwrap();
    let calls = s.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "telegram");
    assert!(calls[0].1.contains("did the thing"));
}

#[test]
fn discord_routes_to_the_discord_sender() {
    let s = MockSender::ok();
    deliver_summary(&s, &Deliver::Discord, "job1", "did the thing").unwrap();
    let calls = s.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "discord");
    assert!(calls[0].1.contains("did the thing"));
}

#[test]
fn log_delivery_sends_nothing() {
    let s = MockSender::ok();
    deliver_summary(&s, &Deliver::Log, "job1", "did the thing").unwrap();
    assert!(s.calls().is_empty());
}

#[test]
fn file_delivery_appends_the_summary() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hits.log");
    let target = Deliver::File(path.clone());
    let s = MockSender::ok();
    deliver_summary(&s, &target, "job1", "price moved").unwrap();
    deliver_summary(&s, &target, "job1", "price moved again").unwrap();
    let body = std::fs::read_to_string(&path).unwrap();
    assert!(body.contains("price moved"));
    assert!(body.contains("price moved again"));
    assert!(body.contains("job1"));
}

#[test]
fn delivery_failure_returns_an_error_without_panicking() {
    // The contract: a failed delivery must never fail the job itself. The
    // caller turns this into a log line, not an error return.
    let s = MockSender::failing();
    let err = deliver_best_effort(&s, &Deliver::Telegram, "job1", "summary").unwrap();
    assert!(err.contains("telegram down"), "unexpected: {err}");
    // And the strict variant surfaces the error for that log line.
    assert!(deliver_summary(&s, &Deliver::Discord, "job1", "summary").is_err());
}

#[test]
fn summary_is_redacted_before_it_reaches_the_sender() {
    let s = MockSender::ok();
    let raw = "done. used key sk-abc123 for the call";
    let summary = build_summary(raw);
    assert!(!summary.contains("sk-abc123"), "secret leaked: {summary}");
    assert!(summary.contains("[REDACTED]"));
    deliver_summary(&s, &Deliver::Telegram, "job1", &summary).unwrap();
    let sent = &s.calls()[0].1;
    assert!(!sent.contains("sk-abc123"), "secret reached sender: {sent}");
}

#[test]
fn summary_is_truncated_to_about_2000_chars() {
    let long = "x".repeat(5000);
    let summary = build_summary(&long);
    assert!(summary.chars().count() <= 2000, "len={}", summary.len());
    assert!(summary.ends_with('…'));
    let short = "fine";
    assert_eq!(build_summary(short), "fine");
}

#[test]
fn rest_sender_reports_missing_config_as_error() {
    // Without tokens/chat targets configured, delivery fails with a
    // message naming the missing variable — never a panic, never a hang.
    // (Cleared explicitly: no other test reads these.)
    std::env::remove_var("PANTHEON_TELEGRAM_BOT_TOKEN");
    std::env::remove_var("PANTHEON_DELIVER_TELEGRAM_TO");
    let s = RestChannelSender;
    let err = deliver_summary(&s, &Deliver::Telegram, "job1", "x").unwrap_err();
    assert!(
        err.contains("PANTHEON_TELEGRAM_BOT_TOKEN"),
        "unexpected: {err}"
    );
}
