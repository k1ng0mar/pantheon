//! SkillSpector integration: security scanning for agent skills.
//!
//! SkillSpector is NVIDIA's Apache-2.0 scanner for agent skills. It answers
//! one question: is this skill safe to install. The static stage covers 71
//! patterns across 17 categories with regex, YARA, and OSV.dev CVE lookup.
//!
//! This module provides:
//! - `scan_skill_dir`: scan a skill directory with SkillSpector
//! - `record_verdict`: write the scan verdict to disk
//! - `GateOutcome`: the policy decision (Blocked, Quarantined, Allowed, Unavailable)
//!
//! The scanner is invoked as a CLI subprocess. When the binary is missing,
//! the caller decides fail-open or fail-closed from
//! `PANTHEON_SKILL_SCAN_UNAVAILABLE` (default open).

use pantheon_api::error::{Layer, PantheonError};
use std::path::{Path, PathBuf};
use std::process::Command;

fn serr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(code, Layer::Execution, false, cause, "check skill scan", "")
}

/// The policy decision for a skill import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    /// Skill is blocked (DO_NOT_INSTALL). The directory is removed.
    Blocked(String),
    /// Skill is quarantined (CAUTION). The skill body stays readable,
    /// but executables are blocked until the operator clears the quarantine.
    Quarantined(String),
    /// Skill is allowed (SAFE or no findings).
    Allowed(String),
    /// Scanner is unavailable. The skill is imported but flagged.
    Unavailable(String),
}

/// A parsed SkillSpector verdict.
#[derive(Debug, Clone)]
pub struct ScanVerdict {
    pub risk_score: u32,
    pub severity: String,
    pub recommendation: String,
    pub safe_to_install: bool,
    pub findings: Vec<String>,
}

/// Find the SkillSpector binary.
fn skillspector_bin() -> Result<String, PantheonError> {
    if let Ok(bin) = std::env::var("PANTHEON_SKILLSPECTOR_BIN") {
        if !bin.is_empty() {
            return Ok(bin);
        }
    }
    // Try PATH lookup
    if let Ok(which) = Command::new("which").arg("skillspector").output() {
        if which.status.success() {
            let path = String::from_utf8_lossy(&which.stdout).trim().to_string();
            if !path.is_empty() {
                return Ok(path);
            }
        }
    }
    Err(serr(
        "SKILL_SCAN_BIN",
        "skillspector binary not found. Install it with: uv tool install skillspector".to_string(),
    ))
}

/// Scan a skill directory with SkillSpector.
///
/// Runs `skillspector scan <dir> --no-llm --format json --fail-on-incomplete`.
/// The `--fail-on-incomplete` flag is load-bearing: a partial scan exits 1
/// with a report that names the uninspected files, and the gate treats a
/// partial verdict as a block, not an allow.
pub fn scan_skill_dir(dir: &Path) -> Result<ScanVerdict, PantheonError> {
    let bin = skillspector_bin()?;
    let dir_str = dir
        .to_str()
        .ok_or_else(|| serr("SKILL_SCAN_DIR", "skill dir is not UTF-8".to_string()))?;

    let out = Command::new(&bin)
        .args([
            "scan",
            dir_str,
            "--no-llm",
            "--format",
            "json",
            "--fail-on-incomplete",
        ])
        .output()
        .map_err(|e| serr("SKILL_SCAN_SPAWN", e.to_string()))?;

    if out.status.code() == Some(2) {
        return Err(serr(
            "SKILL_SCAN_FAILED",
            "skillspector exit 2 (scan error)".to_string(),
        ));
    }

    parse_verdict(&out.stdout, dir)
}

/// Parse the JSON output from SkillSpector.
fn parse_verdict(stdout: &[u8], _dir: &Path) -> Result<ScanVerdict, PantheonError> {
    let text = String::from_utf8_lossy(stdout);
    let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        serr(
            "SKILL_SCAN_PARSE",
            format!("invalid JSON from skillspector: {e}"),
        )
    })?;

    let risk_score = json
        .get("risk_score")
        .and_then(|v| v.as_u64())
        .unwrap_or(100) as u32;
    let severity = json
        .get("severity")
        .and_then(|v| v.as_str())
        .unwrap_or("UNKNOWN")
        .to_string();
    let recommendation = json
        .get("recommendation")
        .and_then(|v| v.as_str())
        .unwrap_or("DO_NOT_INSTALL")
        .to_string();
    let safe_to_install = json
        .get("safe_to_install")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let findings = json
        .get("findings")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|f| {
                    let rule_id = f.get("rule_id").and_then(|v| v.as_str()).unwrap_or("");
                    let message = f.get("message").and_then(|v| v.as_str()).unwrap_or("");
                    if rule_id.is_empty() {
                        None
                    } else {
                        Some(format!("{rule_id}: {message}"))
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(ScanVerdict {
        risk_score,
        severity,
        recommendation,
        safe_to_install,
        findings,
    })
}

/// Record the scan verdict to disk.
///
/// Writes `<skill_dir>/.skillspector.json` with the verdict details.
/// For blocked skills (where the directory is removed), the verdict is
/// written to `<data_dir>/skills/.quarantine/<name>.skillspector.json`.
pub fn record_verdict(
    skill_dir: &Path,
    name: &str,
    verdict: &ScanVerdict,
    outcome: &str,
) -> Result<(), PantheonError> {
    let record = serde_json::json!({
        "name": name,
        "risk_score": verdict.risk_score,
        "severity": verdict.severity,
        "recommendation": verdict.recommendation,
        "safe_to_install": verdict.safe_to_install,
        "outcome": outcome,
        "findings": verdict.findings,
        "scanned_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_default(),
    });

    let target = if outcome == "blocked" {
        // Directory is removed; write to quarantine dir
        let quarantine_dir = skill_dir
            .parent()
            .ok_or_else(|| serr("SKILL_SCAN_DIR", "no parent dir".to_string()))?
            .join(".quarantine");
        std::fs::create_dir_all(&quarantine_dir)
            .map_err(|e| serr("SKILL_SCAN_DIR", format!("create quarantine dir: {e}")))?;
        quarantine_dir.join(format!("{name}.skillspector.json"))
    } else {
        skill_dir.join(".skillspector.json")
    };

    let json = serde_json::to_string_pretty(&record)
        .map_err(|e| serr("SKILL_SCAN_PARSE", format!("serialize verdict: {e}")))?;
    std::fs::write(&target, json)
        .map_err(|e| serr("SKILL_SCAN_DIR", format!("write verdict: {e}")))?;
    Ok(())
}

/// Gate an imported skill: scan it, record the verdict, and return the outcome.
///
/// This is the single policy point for skill imports. It is called after
/// the skill is written to disk but before it is available for use.
///
/// The verdict is policy, not a raw score:
/// - `DO_NOT_INSTALL` -> Blocked (directory removed)
/// - `CAUTION` -> Quarantined (executables blocked, body readable)
/// - `SAFE` or no findings -> Allowed
pub fn gate_imported_skill(data_dir: &Path, name: &str) -> Result<GateOutcome, PantheonError> {
    let skill_dir = data_dir.join("skills").join(name);
    let verdict = match scan_skill_dir(&skill_dir) {
        Ok(v) => v,
        Err(e) => {
            let block = std::env::var("PANTHEON_SKILL_SCAN_UNAVAILABLE")
                .ok()
                .map(|v| v.eq_ignore_ascii_case("block"))
                .unwrap_or(false);
            if block {
                let _ = std::fs::remove_dir_all(&skill_dir);
                return Ok(GateOutcome::Blocked(e.code));
            }
            return Ok(GateOutcome::Unavailable(e.code));
        }
    };

    let rec = verdict.recommendation.to_uppercase();
    match rec.as_str() {
        "DO_NOT_INSTALL" => {
            let _ = std::fs::remove_dir_all(&skill_dir);
            record_verdict(&skill_dir, name, &verdict, "blocked")?;
            Ok(GateOutcome::Blocked(rec))
        }
        "CAUTION" => {
            set_skill_quarantined(data_dir, name, true)?;
            record_verdict(&skill_dir, name, &verdict, "quarantined")?;
            Ok(GateOutcome::Quarantined(rec))
        }
        _ => {
            record_verdict(&skill_dir, name, &verdict, "allowed")?;
            Ok(GateOutcome::Allowed(rec))
        }
    }
}

/// Quarantine registry: `<data_dir>/skills/quarantined.json`, a JSON
/// array of skill names that are quarantined by a CAUTION verdict.
pub fn quarantined_skill_names(data_dir: &Path) -> Vec<String> {
    let path = data_dir.join("skills").join("quarantined.json");
    if !path.exists() {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// Set or clear the quarantine flag for a skill.
fn set_skill_quarantined(
    data_dir: &Path,
    name: &str,
    quarantined: bool,
) -> Result<(), PantheonError> {
    let path = data_dir.join("skills").join("quarantined.json");
    let mut names = if path.exists() {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| serr("SKILL_SCAN_DIR", format!("read quarantined.json: {e}")))?;
        serde_json::from_str(&text).unwrap_or_default()
    } else {
        Vec::new()
    };

    if quarantined {
        if !names.contains(&name.to_string()) {
            names.push(name.to_string());
        }
    } else {
        names.retain(|n| n != name);
    }

    let json = serde_json::to_string_pretty(&names).map_err(|e| {
        serr(
            "SKILL_SCAN_PARSE",
            format!("serialize quarantined.json: {e}"),
        )
    })?;
    std::fs::write(&path, json)
        .map_err(|e| serr("SKILL_SCAN_DIR", format!("write quarantined.json: {e}")))?;
    Ok(())
}

/// Clear the quarantine for a skill (operator override).
pub fn clear_skill_quarantine(data_dir: &Path, name: &str) -> Result<(), PantheonError> {
    set_skill_quarantined(data_dir, name, false)
}

/// Check if a skill is quarantined.
pub fn is_skill_quarantined(data_dir: &Path, name: &str) -> bool {
    quarantined_skill_names(data_dir).contains(&name.to_string())
}

/// Get the data dir from the environment or default.
fn data_dir() -> PathBuf {
    std::env::var_os("PANTHEON_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default();
            home.join(".pantheon")
        })
}

/// Check if a skill is quarantined (using the default data dir).
pub fn is_skill_quarantined_default(name: &str) -> bool {
    is_skill_quarantined(&data_dir(), name)
}
