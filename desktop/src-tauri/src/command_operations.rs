//! Pantheon-native command panels.
//!
//! Every panel is answered from a local, authoritative source: `pantheon stats`
//! for token and spend history, `pantheon plugins list` for the plugin
//! registry, and the data dir's `config.toml` for lifecycle hooks. Nothing
//! here talks to another agent's control channel - those panels were removed
//! with the harnesses that served them.
//!
//! The native caller owns authorization: these helpers receive a workspace and
//! never accept a path, a command or an import body from the webview.

use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Cap on rows any panel returns, so a huge ledger cannot flood the webview.
const MAX_ROWS: usize = 200;

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "camelCase", deny_unknown_fields)]
pub enum Request {
    Usage,
    Plugins,
    Hooks,
}

pub async fn execute(cwd: &Path, request: Request) -> Result<Value, String> {
    match request {
        Request::Usage => usage(cwd).await,
        Request::Plugins => plugins(cwd).await,
        Request::Hooks => hooks(cwd).await,
    }
}

/// Resolve the Pantheon binary the same way a task launch does, so the panels
/// never disagree with the app about which build is running.
fn pantheon() -> Result<PathBuf, String> {
    crate::providers::executable("pantheon").ok_or_else(|| {
        "The Pantheon executable was not found. Install it, or set its path in Settings."
            .to_string()
    })
}

/// Run one read-only Pantheon command in the workspace and return stdout.
fn run(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new(pantheon()?)
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("Could not run Pantheon: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("Pantheon failed");
        return Err(line.chars().take(300).collect());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Token and spend history from the event ledger (`pantheon stats --json`).
///
/// The heatmap needs a year of days, so the window is one year wide; totals
/// and the per-model table keep their own honest labels. A ledger with no rows
/// is not an error - it is a new install, and the panel says so.
async fn usage(cwd: &Path) -> Result<Value, String> {
    let today = civil_today();
    let from = civil_days_before(&today, 364);
    let raw = run(cwd, &["stats", "--json", "--from", &from, "--to", &today])?;
    let stats: Value = serde_json::from_str(&raw)
        .map_err(|e| format!("Pantheon returned unreadable stats: {e}"))?;
    let totals = &stats["totals"];
    let number = |key: &str| totals[key].as_u64().unwrap_or(0);
    let cost = totals["cost_usd"].as_f64().unwrap_or(0.0);
    let stats_rows = vec![
        json!({"label": "Calls", "value": number("calls").to_string()}),
        json!({"label": "Input tokens", "value": number("input_tokens").to_string()}),
        json!({"label": "Output tokens", "value": number("output_tokens").to_string()}),
        json!({"label": "Total tokens", "value": number("total_tokens").to_string()}),
        json!({"label": "Spend", "value": format!("${cost:.2}")}),
    ];
    let daily: Vec<Value> = stats["by_day"]
        .as_array()
        .map(|days| {
            days.iter()
                .filter_map(|day| {
                    Some(json!({
                        "date": day["date"].as_str()?,
                        "tokens": day["total_tokens"].as_u64().unwrap_or(0),
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    let by_model: Vec<Value> = stats["by_model"]
        .as_array()
        .map(|models| {
            models
                .iter()
                .take(MAX_ROWS)
                .map(|model| {
                    json!({
                        "id": model["model"].as_str().unwrap_or("unknown"),
                        "title": model["model"].as_str().unwrap_or("unknown"),
                        "description": format!(
                            "{} calls · {} tokens · ${:.2}",
                            model["calls"].as_u64().unwrap_or(0),
                            model["total_tokens"].as_u64().unwrap_or(0),
                            model["cost_usd"].as_f64().unwrap_or(0.0)
                        ),
                        "source": "ledger",
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let mut panel = json!({
        "kind": "usage",
        "accountLabel": "Pantheon ledger",
        "plan": format!("{from} to {today}"),
        "windows": [],
        "stats": stats_rows,
        "daily": daily,
    });
    if !by_model.is_empty() {
        panel["models"] = Value::Array(by_model);
    }
    if number("calls") == 0 {
        panel["notes"] =
            json!(["No model calls in the last year. Usage appears here after the first turn."]);
    }
    Ok(panel)
}

/// The plugin registry (`pantheon plugins list`), parsed from its table.
async fn plugins(cwd: &Path) -> Result<Value, String> {
    let raw = run(cwd, &["plugins", "list"])?;
    let mut items = Vec::new();
    for line in raw.lines() {
        let line = line.trim_end();
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with("NAME") || trimmed.starts_with("usage:") {
            continue;
        }
        // `NAME  KIND  VERSION  STATUS  DESCRIPTION`
        let cells: Vec<&str> = line.split_whitespace().collect();
        if cells.len() < 4 {
            continue;
        }
        let (name, kind, version, status) = (cells[0], cells[1], cells[2], cells[3]);
        let description = line
            .split_once(status)
            .map(|(_, rest)| rest.trim().to_string())
            .unwrap_or_default();
        items.push(json!({
            "id": name,
            "title": name,
            "description": if description.is_empty() { format!("{kind} {version}") } else { description },
            "status": status,
        }));
        if items.len() >= MAX_ROWS {
            break;
        }
    }
    Ok(json!({
        "kind": "plugins",
        "items": items,
        "notes": if items.is_empty() { json!(["No plugins are installed."]) } else { json!([]) },
    }))
}

/// Lifecycle hooks from the run's own config (`[hooks]` in config.toml).
async fn hooks(cwd: &Path) -> Result<Value, String> {
    let path = config_path(cwd)?;
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("Could not read {}: {e}", path.display()))?;
    let document: toml::Value =
        toml::from_str(&text).map_err(|e| format!("config.toml is not valid TOML: {e}"))?;
    let mut items = Vec::new();
    if let Some(hooks) = document.get("hooks").and_then(|h| h.as_table()) {
        for (name, value) in hooks {
            let enabled = value
                .get("enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let command = value
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            items.push(json!({
                "id": name,
                "title": name,
                "description": command,
                "source": path.display().to_string(),
                "enabled": enabled,
            }));
        }
    }
    Ok(json!({
        "kind": "hooks",
        "items": items,
        "notes": if items.is_empty() { json!(["No lifecycle hooks are configured in config.toml."]) } else { json!([]) },
    }))
}

/// The data dir's config file (`pantheon config path`), so a relocated data dir
/// is respected instead of guessing `~/.pantheon`.
fn config_path(cwd: &Path) -> Result<PathBuf, String> {
    let raw = run(cwd, &["config", "path"])?;
    raw.lines()
        .map(str::trim)
        .rev()
        .find(|line| !line.is_empty() && !line.to_lowercase().contains("bundled plugin"))
        .map(PathBuf::from)
        .ok_or_else(|| "Pantheon did not report a config path.".to_string())
}

/// `YYYY-MM-DD` for today, in UTC (the ledger records UTC days).
fn civil_today() -> String {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0);
    civil_from_days(days as i64)
}

fn civil_days_before(today: &str, days: i64) -> String {
    let (y, m, d) = parse_civil(today);
    civil_from_days(days_from_civil(y, m, d) - days)
}

fn parse_civil(value: &str) -> (i64, u32, u32) {
    let mut parts = value.split('-').filter_map(|p| p.parse::<i64>().ok());
    (
        parts.next().unwrap_or(1970),
        parts.next().unwrap_or(1) as u32,
        parts.next().unwrap_or(1) as u32,
    )
}

/// Howard Hinnant's civil-date algorithms: one small dependency-free calendar
/// conversion, used only to label a one-year stats window.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = ((m as i64) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> String {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_one_year_window_is_365_days_wide() {
        let today = civil_today();
        let from = civil_days_before(&today, 364);
        assert_eq!(
            days_from_civil(
                parse_civil(&today).0,
                parse_civil(&today).1,
                parse_civil(&today).2
            ) - days_from_civil(
                parse_civil(&from).0,
                parse_civil(&from).1,
                parse_civil(&from).2
            ),
            364
        );
        assert_eq!(civil_from_days(0), "1970-01-01");
        assert_eq!(civil_from_days(19_000), "2022-01-08");
    }

    #[test]
    fn the_plugin_table_parses_into_rows_and_skips_its_header() {
        let table = "  NAME      KIND VERSION   STATUS    DESCRIPTION\n  cf-ops    tool 1.0.0     disabled  Cloudflare operations.\n";
        let mut items = Vec::new();
        for line in table.lines() {
            let trimmed = line.trim_start();
            if trimmed.is_empty() || trimmed.starts_with("NAME") || trimmed.starts_with("usage:") {
                continue;
            }
            let cells: Vec<&str> = line.split_whitespace().collect();
            if cells.len() < 4 {
                continue;
            }
            items.push((cells[0], cells[3]));
        }
        assert_eq!(items, vec![("cf-ops", "disabled")]);
    }

    #[test]
    fn hooks_read_the_section_the_runtime_reads() {
        let document: toml::Value =
            toml::from_str("[hooks.after_turn]\ncommand = \"pantheon doctor\"\nenabled = false\n")
                .unwrap();
        let hooks = document.get("hooks").and_then(|h| h.as_table()).unwrap();
        assert_eq!(hooks.len(), 1);
        assert_eq!(
            hooks["after_turn"].get("command").and_then(|v| v.as_str()),
            Some("pantheon doctor")
        );
    }
}
