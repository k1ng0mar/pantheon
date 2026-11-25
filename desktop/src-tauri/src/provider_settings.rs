//! Pantheon's own executable override. There are no provider API keys here:
//! the Pantheon CLI owns its provider registry and credentials, and the
//! desktop never stores a key of its own.
use crate::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Settings {
    executable_paths: BTreeMap<String, String>,
}
struct Runtime {
    settings: Settings,
    file: PathBuf,
}
static CONFIG: OnceLock<Mutex<Runtime>> = OnceLock::new();

pub fn setup(dir: &Path) -> Result<()> {
    let path = dir.join("providers.json");
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(format!("Could not read provider settings: {error}")),
    };
    let settings = if bytes.is_empty() {
        Settings::default()
    } else {
        // A file from a build that stored more is still readable: unknown keys
        // are dropped here instead of refusing to start.
        serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|value| {
                serde_json::from_value::<Settings>(serde_json::json!({
                    "executablePaths": value.get("executablePaths").cloned().unwrap_or_default(),
                }))
                .ok()
            })
            .unwrap_or_default()
    };
    let _ = CONFIG.set(Mutex::new(Runtime {
        settings,
        file: path,
    }));
    Ok(())
}

/// The one setting this desktop owns: which `pantheon` binary to launch.
/// Credentials stay in the CLI's own store, never here.
pub fn executable_setting(provider: &str) -> Option<String> {
    CONFIG
        .get()?
        .lock()
        .ok()?
        .settings
        .executable_paths
        .get(provider)
        .cloned()
}

pub fn save_executable(provider: &str, input: Option<&str>) -> Result<Option<String>> {
    let runtime = CONFIG
        .get()
        .ok_or("Provider settings are not available yet.")?;
    let mut runtime = runtime.lock().map_err(|e| e.to_string())?;
    let value = match input.map(str::trim).filter(|v| !v.is_empty()) {
        Some(path) => Some(
            tidy(&validated_executable_path(path)?)
                .to_string_lossy()
                .into_owned(),
        ),
        None => None,
    };
    let mut next = runtime.settings.clone();
    match &value {
        Some(path) => {
            next.executable_paths
                .insert(provider.to_string(), path.clone());
        }
        None => {
            next.executable_paths.remove(provider);
        }
    }
    // Written before memory changes, so a failed write keeps the old choice.
    if let Some(dir) = runtime.file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("Could not save the setting: {e}"))?;
    }
    let bytes = serde_json::to_vec_pretty(&next).map_err(|e| e.to_string())?;
    std::fs::write(&runtime.file, bytes).map_err(|e| format!("Could not save the setting: {e}"))?;
    runtime.settings = next;
    Ok(value)
}

pub fn executable_override(provider: &str) -> Option<PathBuf> {
    CONFIG
        .get()?
        .lock()
        .ok()?
        .settings
        .executable_paths
        .get(provider)
        .map(PathBuf::from)
}

pub fn validated_executable_path(input: &str) -> Result<PathBuf> {
    let path = PathBuf::from(input.trim());
    if input.len() > 4096 || path.as_os_str().is_empty() {
        return Err("Choose an executable for Pantheon.".into());
    }
    if !path.is_absolute() || !path.is_file() {
        return Err("Choose an existing executable file (an absolute path).".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(&path)
            .map_err(|error| format!("Could not inspect that file: {error}"))?
            .permissions()
            .mode()
            & 0o111
            == 0
        {
            return Err("That file is not executable.".into());
        }
    }
    Ok(path)
}

/// Collapses `//` and `.` so the stored path matches what the OS resolves.
fn tidy(path: &Path) -> PathBuf {
    path.components().collect()
}

/// The saved choice for Settings, or null when discovery decides.
#[tauri::command]
pub fn provider_executable(provider: String) -> Result<Option<String>> {
    if provider != "pantheon" {
        return Err("Pantheon is the only supported provider.".into());
    }
    Ok(executable_setting(&provider))
}

/// Saves or clears the executable Settings launches. An empty path clears it.
#[tauri::command]
pub fn provider_save_executable(provider: String, path: Option<String>) -> Result<Option<String>> {
    if provider != "pantheon" {
        return Err("Pantheon is the only supported provider.".into());
    }
    save_executable(&provider, path.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_absolute_executables_are_accepted() {
        assert!(validated_executable_path("").is_err());
        assert!(validated_executable_path("relative/pantheon").is_err());
        assert!(validated_executable_path("/nonexistent/pantheon").is_err());
    }

    #[test]
    fn settings_from_a_wider_build_keep_their_executable_path() {
        let dir = std::env::temp_dir().join(format!("pantheon-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("providers.json"),
            br#"{"executablePaths":{"pantheon":"/opt/pantheon/bin/pantheon"},"opencodeModels":["a"],"mimoEndpoint":"api","claudeConnectionMode":"inherit"}"#,
        )
        .unwrap();
        setup(&dir).unwrap();
        assert_eq!(
            executable_override("pantheon"),
            Some(PathBuf::from("/opt/pantheon/bin/pantheon"))
        );
        // The override is editable, and clearing it goes back to discovery.
        let bin = dir.join("pantheon");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let saved = save_executable("pantheon", Some(&bin.to_string_lossy())).unwrap();
        assert_eq!(saved.as_deref(), Some(bin.to_string_lossy().as_ref()));
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("providers.json")).unwrap()).unwrap();
        assert_eq!(
            written["executablePaths"]["pantheon"],
            bin.to_string_lossy().as_ref()
        );
        assert!(save_executable("pantheon", Some("relative/pantheon")).is_err());
        assert_eq!(executable_override("pantheon"), Some(bin.clone()));
        save_executable("pantheon", None).unwrap();
        assert_eq!(executable_override("pantheon"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
