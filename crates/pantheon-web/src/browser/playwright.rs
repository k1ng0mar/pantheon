//! Playwright backend: the official stateful `@playwright/cli` subprocess
//! fallback.
//!
//! The CLI keeps one browser session per `-s=<session>` name, so Pantheon
//! sessions map 1:1 onto CLI sessions. Canonical commands translate to
//! CLI commands ([`translate`], pure and unit-tested); `wait-for` polls a
//! condition probe through `eval`; `act`/`act-instruction` are unsupported
//! (no agent in this loop).
//!
//! Verified against the live CLI on 2026-09-29 (`npx -y @playwright/cli
//! --help`): `playwright-cli -s=<session> --json <command>...` (`--json`
//! is a global option). `snapshot` captures the page to obtain element
//! refs (`e15`-style); `click`/`fill`/`hover` take a ref or unique
//! selector; `type <text>` has no selector slot (canonical `type
//! <selector> <text>` is scoped via `eval`); `screenshot --filename
//! --type <png|jpeg|webp>`; `close` ends the session.

use super::backend::BrowserBackend;
use super::cdp::{build_extract_arrow, js_str, wait_plan, WaitPlan};
use super::error::BrowserError;
use super::proc::{run_with_timeout, snip, SpawnSpec};
use std::path::PathBuf;
use std::time::Duration;

/// Default CLI binary name (on `PATH` after `npm i -g @playwright/cli`).
pub const DEFAULT_BINARY: &str = "playwright-cli";

/// Poll interval for `wait-for` probes.
const WAIT_POLL: Duration = Duration::from_millis(500);

/// Playwright backend configuration.
#[derive(Debug, Clone)]
pub struct PlaywrightConfig {
    /// CLI binary. `None` = [`DEFAULT_BINARY`].
    pub binary: Option<PathBuf>,
    /// Command timeout, in seconds.
    pub timeout_secs: u64,
}

impl Default for PlaywrightConfig {
    fn default() -> Self {
        Self {
            binary: None,
            timeout_secs: 120,
        }
    }
}

impl PlaywrightConfig {
    fn binary(&self) -> PathBuf {
        self.binary
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_BINARY))
    }
}

/// Translate a canonical argv (without the program name) to
/// `playwright-cli` command args. `None` means the command needs special
/// handling (`wait-for`) or is unsupported (`act*`) - see [`invoke_kind`].
fn translate(argv: &[String]) -> Result<Vec<String>, InvokeKind> {
    let s = |i: usize| argv.get(i).cloned().unwrap_or_default();
    let flag = |name: &str| {
        argv.iter()
            .position(|a| a == name)
            .and_then(|i| argv.get(i + 1))
            .cloned()
    };
    let cmd = argv.first().map(String::as_str).unwrap_or("");
    let args: Vec<String> = match cmd {
        "navigate" => vec!["goto".into(), s(1)],
        "back" => vec!["go-back".into()],
        "forward" => vec!["go-forward".into()],
        "reload" => vec!["reload".into()],
        "snapshot" => vec!["snapshot".into()],
        "click-ref" => vec!["click".into(), s(1)],
        "hover-ref" => vec!["hover".into(), s(1)],
        "fill-ref" => vec!["fill".into(), s(1), s(2)],
        "click" => vec!["click".into(), s(1)],
        // `type <text>` has no selector slot; scope via eval on the target.
        "type" => vec!["eval".into(), type_fn(&s(2)), s(1)],
        "press" => vec!["press".into(), s(1)],
        "extract" => vec![
            "eval".into(),
            build_extract_arrow(flag("--schema").as_deref(), flag("--selector").as_deref()),
        ],
        "screenshot" => {
            let mut a = vec!["screenshot".into()];
            if let Some(out) = flag("--output") {
                a.push("--filename".into());
                a.push(out);
            }
            if let Some(fmt) = flag("--format") {
                a.push("--type".into());
                a.push(fmt);
            }
            a
        }
        "page-source" => vec![
            "eval".into(),
            "()=>document.documentElement.outerHTML".into(),
        ],
        "wait-for" => return Err(InvokeKind::WaitFor),
        "act" | "act-instruction" => return Err(InvokeKind::Unsupported(cmd.to_string())),
        other => return Err(InvokeKind::Unsupported(other.to_string())),
    };
    Ok(args)
}

/// JS for canonical `type <selector> <text>`: focus, append, dispatch.
fn type_fn(text: &str) -> String {
    let t = js_str(text);
    format!(
        "(el)=>{{el.focus();const cur=(el.value!==undefined)?el.value:(el.textContent||'');\
         if(el.value!==undefined){{el.value=cur+{t};}}else{{el.textContent=cur+{t};}}\
         el.dispatchEvent(new Event('input',{{bubbles:true}}));\
         el.dispatchEvent(new Event('change',{{bubbles:true}}));}}"
    )
}

#[derive(Debug)]
enum InvokeKind {
    Cli(Vec<String>),
    WaitFor,
    Unsupported(String),
}

fn invoke_kind(argv: &[String]) -> InvokeKind {
    match translate(argv) {
        Ok(args) => InvokeKind::Cli(args),
        Err(k) => k,
    }
}

/// Parse `--json` CLI output defensively: JSON when it parses, otherwise
/// a wrapped text blob.
fn parse_output(stdout: &str) -> serde_json::Value {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return serde_json::json!({"ok": true});
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return v;
    }
    serde_json::json!({"output": trimmed})
}

/// Playwright subprocess backend.
pub struct PlaywrightBackend {
    config: PlaywrightConfig,
}

impl PlaywrightBackend {
    pub fn new(config: PlaywrightConfig) -> Self {
        Self { config }
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(self.config.timeout_secs.max(1))
    }

    /// Run one CLI command: `playwright-cli -s=<session> --json <args...>`.
    fn run_cli(&self, session: &str, args: &[String]) -> Result<serde_json::Value, BrowserError> {
        let mut argv: Vec<String> = vec![format!("-s={session}"), "--json".to_string()];
        argv.extend(args.iter().cloned());
        let binary = self.config.binary();
        let empty_env = std::collections::HashMap::new();
        let spec = SpawnSpec {
            binary: &binary,
            args: &argv,
            extra_env: &empty_env,
            timeout_secs: self.config.timeout_secs,
            error_argv: args,
        };
        let out = run_with_timeout(&spec)?;
        if !out.status.success() {
            return Err(BrowserError::Failed {
                argv: args.to_vec(),
                exit: out.status.code(),
                stderr: snip(&String::from_utf8_lossy(&out.stderr), 2000),
            });
        }
        Ok(parse_output(&String::from_utf8_lossy(&out.stdout)))
    }

    /// `wait-for`: poll a condition probe through `eval` until it is true
    /// or the command timeout elapses. `delay` sleeps once instead.
    fn wait_for(&self, session: &str, argv: &[String]) -> Result<serde_json::Value, BrowserError> {
        match wait_plan(argv, "playwright")? {
            WaitPlan::Delay(ms) => {
                let capped = ms.min(self.timeout().as_millis() as u64);
                std::thread::sleep(Duration::from_millis(capped));
                Ok(serde_json::json!({"ok": true, "slept_ms": capped}))
            }
            WaitPlan::Probe(probe) => {
                let probe = format!("()=>{{return {};}}", probe);
                let deadline = std::time::Instant::now() + self.timeout();
                loop {
                    let v = self.run_cli(session, &["eval".to_string(), probe.clone()])?;
                    // `--json` eval of a boolean expression yields JSON `true`/`false`.
                    let hit = v.as_bool().unwrap_or(false);
                    if hit {
                        return Ok(serde_json::json!({"ok": true}));
                    }
                    if std::time::Instant::now() >= deadline {
                        return Err(BrowserError::Failed {
                            argv: argv.to_vec(),
                            exit: None,
                            stderr: "wait-for condition not met in time".into(),
                        });
                    }
                    std::thread::sleep(WAIT_POLL);
                }
            }
        }
    }
}

impl BrowserBackend for PlaywrightBackend {
    fn invoke(&self, argv: &[String], session: &str) -> Result<serde_json::Value, BrowserError> {
        match invoke_kind(argv) {
            InvokeKind::Cli(args) => self.run_cli(session, &args),
            InvokeKind::WaitFor => self.wait_for(session, argv),
            InvokeKind::Unsupported(cmd) => Err(BrowserError::UnsupportedCommand {
                command: cmd,
                backend: "playwright".to_string(),
            }),
        }
    }

    fn stop_daemon(&self, session: &str) -> Result<(), BrowserError> {
        // Best-effort: a missing/closed CLI session is not an error.
        let _ = self.run_cli(session, &["close".to_string()]);
        Ok(())
    }
}
