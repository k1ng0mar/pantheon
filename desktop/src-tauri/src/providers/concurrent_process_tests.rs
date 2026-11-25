//! The application-shutdown test kills every registered child process, so tests
//! that start real providers must not run beside it. They take this lock.
use std::sync::OnceLock;

pub(crate) fn process_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}
