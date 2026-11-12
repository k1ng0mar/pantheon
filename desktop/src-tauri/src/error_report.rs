//! Durable diagnostics for panics and unrecovered failures.
//!
//! A panic on any thread must never be silent. The hook writes one line to
//! `pantheon-errors.log` in the app data dir and emits `app-error` so the
//! running UI can tell the user what happened and offer recovery.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Serialize;
use tauri::{AppHandle, Emitter};

use crate::storage;

static APP: OnceLock<AppHandle> = OnceLock::new();
static LOG: OnceLock<PathBuf> = OnceLock::new();

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppError {
    pub kind: &'static str,
    pub message: String,
    pub location: String,
    pub guidance: &'static str,
}

const PANIC_GUIDANCE: &str =
    "Pantheon hit an unexpected error. Your work is saved; reload the window to continue.";

/// Install the process-wide panic hook. Call once from `setup`, after the app
/// data dir exists.
pub fn install(app: &AppHandle, dir: &Path) {
    let _ = APP.set(app.clone());
    let _ = LOG.set(dir.join("pantheon-errors.log"));
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = payload(info.payload());
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_string());
        append(&format!("panic: {message} at {location}"));
        if let Some(app) = APP.get() {
            let _ = app.emit(
                "app-error",
                AppError { kind: "panic", message, location, guidance: PANIC_GUIDANCE },
            );
        }
        previous(info);
    }));
}

/// Durable log line for a background failure that never reaches a command
/// response. Log-only, so a recurring failure can never spam the UI.
pub fn record(kind: &'static str, message: impl Into<String>, location: impl Into<String>) {
    append(&format!("{kind}: {} at {}", message.into(), location.into()));
}

fn append(line: &str) {
    let Some(path) = LOG.get() else { return };
    let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = writeln!(file, "{} {line}", storage::now());
}

fn payload(value: &(dyn std::any::Any + Send)) -> String {
    if let Some(text) = value.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = value.downcast_ref::<String>() {
        text.clone()
    } else {
        "a panic occurred".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panic_payloads_keep_their_message() {
        let text = payload(&"boom");
        assert!(text.contains("boom"));
        let owned = payload(&"owned".to_string());
        assert!(owned.contains("owned"));
        assert_eq!(payload(&42), "a panic occurred");
    }

    #[test]
    fn log_lines_are_written_to_the_app_data_dir() {
        let dir = std::env::temp_dir().join(format!("pantheon-error-report-{}", storage::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _ = LOG.set(dir.join("pantheon-errors.log"));
        record("test", "something failed", "test.rs:1:1");
        let log = std::fs::read_to_string(dir.join("pantheon-errors.log")).unwrap();
        assert!(log.contains("test: something failed at test.rs:1:1"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
