//! Ollama onboarding: discover, hardware check, verified tool call gate.
//!
//! The helper probes the local endpoint, checks total memory from
//! /proc/meminfo or sysctl, and reports whether a model is ready.
//! It never downloads a model or changes the default model. The
//! verified tool call step is a live check the caller runs after
//! this helper returns ready.

/// Minimum RAM in GiB recommended for the smallest local recipes.
pub const MIN_RAM_GIB: u64 = 8;

pub struct OllamaReport {
    pub reachable: bool,
    pub models: Vec<String>,
    pub ram_gib: Option<u64>,
    pub ready: bool,
}

pub fn total_ram_gib() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
            for line in text.lines() {
                let line = line.trim();
                if let Some(rest) = line.strip_prefix("MemTotal:") {
                    let kb: u64 = rest
                        .split_whitespace()
                        .next()
                        .and_then(|n| n.parse().ok())
                        .unwrap_or(0);
                    return Some((kb / 1024) / 1024);
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
        {
            if out.status.success() {
                if let Ok(bytes) = String::from_utf8(out.stdout)
                    .unwrap_or_default()
                    .trim()
                    .parse::<u64>()
                {
                    return Some(bytes / 1024 / 1024 / 1024);
                }
            }
        }
    }
    None
}

pub fn check_local_models(base_url: &str) -> OllamaReport {
    // Reuse the picker discovery path indirectly via a plain GET here to
    // avoid depending on ureq agent setup in this small helper. The
    // caller already has fetch_models for the full picker flow.
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut models = Vec::new();
    let reachable;
    match std::process::Command::new("curl")
        .args(["-s", "--max-time", "5", &url])
        .output()
    {
        Ok(out) if out.status.success() => {
            reachable = true;
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) {
                if let Some(arr) = v.get("data").and_then(|d| d.as_array()) {
                    for m in arr {
                        if let Some(id) = m.get("id").and_then(|i| i.as_str()) {
                            models.push(id.to_string());
                        }
                    }
                }
            }
        }
        _ => {
            reachable = false;
        }
    }
    let ram_gib = total_ram_gib();
    let ready =
        reachable && !models.is_empty() && ram_gib.map(|r| r >= MIN_RAM_GIB).unwrap_or(true);
    OllamaReport {
        reachable,
        models,
        ram_gib,
        ready,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ram_probe_never_panics() {
        let _ = total_ram_gib();
    }

    #[test]
    fn unreachable_endpoint_reports_not_ready() {
        let r = check_local_models("http://127.0.0.1:1");
        assert!(!r.reachable);
        assert!(!r.ready);
    }
}
