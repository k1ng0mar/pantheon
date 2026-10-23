//! Opt-in post-edit formatting.
//!
//! The runtime constructs one [`Formatter`] from `[formatters]` and hands
//! it to the built-in and safe-write tools. A formatter is a separate
//! subprocess with a hard timeout; a formatting failure is reported in
//! the tool result but never undoes the write. The model cannot choose the
//! command: rules come from operator config or this module's built-ins.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// One formatter invocation result. `success == false` is informational;
/// callers must not roll back a successful write because formatting failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatOutcome {
    pub command: String,
    pub success: bool,
    pub output: String,
}

/// A formatting failure or success report for a set of paths.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FormatReport {
    pub outcomes: Vec<(PathBuf, FormatOutcome)>,
}

impl FormatReport {
    pub fn is_empty(&self) -> bool {
        self.outcomes.is_empty()
    }

    /// A compact line for tool output. No command path is secret: the
    /// command is operator config, but output may contain file content, so
    /// it is bounded and kept short.
    pub fn summary(&self) -> String {
        if self.outcomes.is_empty() {
            return String::new();
        }
        let ok = self.outcomes.iter().filter(|(_, o)| o.success).count();
        let failed = self.outcomes.len() - ok;
        let mut out = format!("formatted {ok} file(s)");
        if failed > 0 {
            out.push_str(&format!("; {failed} failed"));
            for (path, outcome) in self.outcomes.iter().filter(|(_, o)| !o.success) {
                out.push_str(&format!(
                    "\n  {}: {}",
                    path.display(),
                    truncate(&outcome.output, 300)
                ));
            }
        }
        out
    }
}

/// Operator configuration for post-edit formatting.
#[derive(Debug, Clone)]
pub struct Formatter {
    timeout: Duration,
    /// Extension (without the dot, lowercase) -> command template.
    commands: HashMap<String, String>,
}

impl Formatter {
    pub fn new(timeout: Duration, commands: HashMap<String, String>) -> Self {
        let commands = commands
            .into_iter()
            .filter_map(|(ext, command)| {
                let ext = normalize_ext(&ext)?;
                let command = command.trim().to_string();
                (!command.is_empty()).then_some((ext, command))
            })
            .collect();
        Self {
            timeout: timeout.max(Duration::from_millis(250)),
            commands,
        }
    }

    /// The command that would run for this path, if any. Pure and cheap;
    /// used by tests and by `/settings` style diagnostics.
    pub fn command_for(&self, path: &Path) -> Option<String> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        self.commands
            .get(&ext)
            .cloned()
            .or_else(|| builtin_command(&ext).map(str::to_string))
    }

    /// Format one file. `None` means no rule matched (not a failure).
    pub fn format(&self, path: &Path) -> Option<FormatOutcome> {
        let command = self.command_for(path)?;
        Some(self.run(&command, path))
    }

    /// Format a batch, skipping paths with no rule. Callers use the report
    /// for the tool result; failures do not propagate as errors.
    pub fn format_paths<'a>(&self, paths: impl IntoIterator<Item = &'a Path>) -> FormatReport {
        let mut report = FormatReport::default();
        for path in paths {
            if let Some(outcome) = self.format(path) {
                report.outcomes.push((path.to_path_buf(), outcome));
            }
        }
        report
    }

    fn run(&self, template: &str, path: &Path) -> FormatOutcome {
        // Keep `{file}` substitution shell-safe: the path is passed as
        // $1, never interpolated into the command string. A template
        // without `{file}` gets the path appended.
        let script = if template.contains("{file}") {
            template.replace("{file}", "\"$1\"")
        } else {
            format!("{template} \"$1\"")
        };
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.arg("/C").arg(script.replace("$1", "%1"));
            c
        } else {
            let mut c = Command::new("sh");
            c.arg("-c").arg(script).arg("pantheon-format").arg(path);
            c
        };
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            cmd.current_dir(parent);
        }
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                return FormatOutcome {
                    command: template.to_string(),
                    success: false,
                    output: format!("spawn failed: {e}"),
                }
            }
        };
        let deadline = Instant::now() + self.timeout;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return FormatOutcome {
                        command: template.to_string(),
                        success: false,
                        output: format!("timed out after {}s", self.timeout.as_secs()),
                    };
                }
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return FormatOutcome {
                        command: template.to_string(),
                        success: false,
                        output: format!("wait failed: {e}"),
                    };
                }
            }
        }
        let output = match child.wait_with_output() {
            Ok(out) => out,
            Err(e) => {
                return FormatOutcome {
                    command: template.to_string(),
                    success: false,
                    output: format!("read output failed: {e}"),
                }
            }
        };
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.trim().is_empty() {
            if !text.trim().is_empty() {
                text.push('\n');
            }
            text.push_str(stderr.trim());
        }
        FormatOutcome {
            command: template.to_string(),
            success: output.status.success(),
            output: truncate(text.trim(), 2_000),
        }
    }
}

fn normalize_ext(ext: &str) -> Option<String> {
    let ext = ext.trim().trim_start_matches('.').to_ascii_lowercase();
    (!ext.is_empty()).then_some(ext)
}

fn builtin_command(ext: &str) -> Option<&'static str> {
    match ext {
        "rs" => Some("rustfmt --edition 2021 {file}"),
        "go" => Some("gofmt -w {file}"),
        "py" => Some("ruff format {file}"),
        "js" | "jsx" | "ts" | "tsx" | "json" | "jsonc" | "css" | "scss" | "less" | "md" | "mdx"
        | "yml" | "yaml" | "html" => Some("prettier --write {file}"),
        "sh" | "bash" | "zsh" => Some("shfmt -w {file}"),
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" => Some("clang-format -i {file}"),
        "toml" => Some("taplo fmt {file}"),
        _ => None,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn custom_rule_overrides_builtin_and_normalizes_extension() {
        let f = Formatter::new(
            Duration::from_secs(1),
            HashMap::from([("RS".to_string(), "myfmt {file}".to_string())]),
        );
        assert_eq!(
            f.command_for(Path::new("src/lib.rs")).as_deref(),
            Some("myfmt {file}")
        );
    }

    #[test]
    fn builtin_rules_cover_common_languages() {
        let f = Formatter::new(Duration::from_secs(1), HashMap::new());
        assert!(f
            .command_for(Path::new("main.go"))
            .unwrap()
            .contains("gofmt"));
        assert!(f
            .command_for(Path::new("app.tsx"))
            .unwrap()
            .contains("prettier"));
        assert!(f.command_for(Path::new("README")).is_none());
    }

    #[test]
    fn missing_command_is_a_reported_failure_not_a_panic() {
        let f = Formatter::new(
            Duration::from_secs(1),
            HashMap::from([(
                "txt".to_string(),
                "definitely-not-a-real-formatter-xyz {file}".to_string(),
            )]),
        );
        let out = f.format(Path::new("note.txt")).expect("rule matched");
        assert!(!out.success);
        assert!(out.output.contains("spawn failed") || !out.output.is_empty());
    }

    #[test]
    fn custom_command_receives_the_file_path_without_shell_interpolation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.foo");
        std::fs::write(&path, "x").unwrap();
        let f = Formatter::new(
            Duration::from_secs(2),
            HashMap::from([(
                "foo".to_string(),
                "sh -c 'printf formatted > \"$1\"' -- {file}".to_string(),
            )]),
        );
        let out = f.format(&path).expect("rule matched");
        assert!(out.success, "{out:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "formatted");
    }
}
