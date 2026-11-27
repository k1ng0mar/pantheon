//! Provider command catalogs are metadata, never arbitrary executable instructions.
mod acp;

use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SlashCommand {
    pub id: String,
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub kind: String,
    pub source: String,
    pub reason: Option<String>,
    #[serde(skip)]
    pub invocation: Invocation,
}

#[derive(Clone, Debug)]
pub enum Invocation {
    Native,
    Action(String),
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Catalog {
    pub commands: Vec<SlashCommand>,
    pub warnings: Vec<String>,
}

#[derive(Clone)]
struct Cached {
    catalog: Option<Catalog>,
    updated: Instant,
    touched: Instant,
    revision: u64,
    terminal_names: HashSet<String>,
    discovery: Arc<tokio::sync::Mutex<()>>,
}

type Key = (String, PathBuf);
fn catalog_key(provider: &str, cwd: &Path) -> Key {
    (provider.into(), crate::providers::external_path(cwd))
}
#[derive(Default)]
struct CommandCache {
    entries: HashMap<Key, Cached>,
    revision: u64,
}

static CACHE: LazyLock<Mutex<CommandCache>> = LazyLock::new(|| Mutex::new(CommandCache::default()));
static DISCOVERY: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
const CACHE_AGE: Duration = Duration::from_secs(300);
const REFRESH_WARNING: &str =
    "Could not refresh Pantheon commands. Try Refresh, or start a task to sync its command list.";

enum Commit {
    Ready(Catalog),
    Invalidated,
}

impl CommandCache {
    fn next_revision(&mut self) -> u64 {
        self.revision = self.revision.wrapping_add(1);
        self.revision
    }

    fn insert(
        &mut self,
        key: Key,
        catalog: Option<Catalog>,
        terminal_names: HashSet<String>,
    ) -> u64 {
        let capacity = if self.entries.contains_key(&key) {
            16
        } else {
            15
        };
        while self.entries.len() > capacity {
            if let Some(oldest) = self
                .entries
                .iter()
                // Keep the shared gate while requests are waiting on it.
                .filter(|(candidate, value)| {
                    **candidate != key && Arc::strong_count(&value.discovery) == 1
                })
                .min_by_key(|(_, value)| value.touched)
                .map(|(key, _)| key.clone())
            {
                self.entries.remove(&oldest);
            } else {
                break;
            }
        }
        let revision = self.next_revision();
        let discovery = self
            .entries
            .get(&key)
            .map(|entry| Arc::clone(&entry.discovery))
            .unwrap_or_else(|| Arc::new(tokio::sync::Mutex::new(())));
        let now = Instant::now();
        self.entries.insert(
            key,
            Cached {
                catalog,
                updated: now,
                touched: now,
                revision,
                terminal_names,
                discovery,
            },
        );
        revision
    }

    fn cached(&mut self, key: &Key, since: Option<Instant>) -> Option<Catalog> {
        let entry = self.entries.get_mut(key)?;
        entry.touched = Instant::now();
        if entry.updated.elapsed() >= CACHE_AGE || since.is_some_and(|since| entry.updated < since)
        {
            return None;
        }
        entry.catalog.clone()
    }

    fn begin(&mut self, key: &Key) -> u64 {
        if let Some(entry) = self.entries.get_mut(key) {
            entry.touched = Instant::now();
            return entry.revision;
        }
        self.insert(key.clone(), None, HashSet::new())
    }

    fn discovery(&mut self, key: &Key) -> Arc<tokio::sync::Mutex<()>> {
        self.begin(key);
        Arc::clone(&self.entries[key].discovery)
    }

    fn snapshot(&mut self, key: &Key) -> Catalog {
        let mut catalog = self
            .entries
            .get_mut(key)
            .and_then(|entry| {
                entry.touched = Instant::now();
                entry.catalog.clone()
            })
            .unwrap_or_default();
        normalize(&mut catalog, &key.0);
        catalog
    }

    fn commit(&mut self, key: Key, revision: u64, discovered: Result<Catalog, String>) -> Commit {
        let Some(previous) = self.entries.get(&key) else {
            return Commit::Invalidated;
        };
        if previous.revision != revision {
            return previous
                .catalog
                .clone()
                .map(Commit::Ready)
                .unwrap_or(Commit::Invalidated);
        }
        let terminal_names = previous.terminal_names.clone();
        let mut value = match discovered {
            Ok(value) => value,
            Err(error) => {
                let mut value = previous.catalog.clone().unwrap_or_default();
                // Only known protocol conditions get specific public warnings.
                // Process output stays behind the generic message.
                let warning = if error.starts_with(crate::providers::acp::ACP_STARTUP_TIMEOUT) {
                    crate::providers::acp::ACP_STARTUP_TIMEOUT
                } else if error == acp::NO_COMMANDS {
                    acp::NO_COMMANDS
                } else {
                    REFRESH_WARNING
                };
                value.warnings.retain(|warning| {
                    warning != REFRESH_WARNING
                        && warning != crate::providers::acp::ACP_STARTUP_TIMEOUT
                        && warning != acp::NO_COMMANDS
                });
                value.warnings.insert(0, warning.into());
                value
            }
        };
        apply_terminal_names(&mut value, &terminal_names);
        normalize(&mut value, &key.0);
        // The comparison and write share one lock, so a live frame cannot land
        // between the stale-result check and this write.
        self.insert(key, Some(value.clone()), terminal_names);
        Commit::Ready(value)
    }

    fn observe(&mut self, key: Key, data: &Value, mut value: Catalog) {
        let data = if data["type"] == "control_response" {
            &data["response"]["response"]
        } else {
            data
        };
        let explicit_terminals = data
            .get("terminal_slash_commands")
            .and_then(Value::as_array);
        let mut terminal_names = self
            .entries
            .get(&key)
            .map(|entry| entry.terminal_names.clone())
            .unwrap_or_default();
        if let Some(names) = explicit_terminals {
            terminal_names = names
                .iter()
                .take(512)
                .filter_map(Value::as_str)
                .map(|name| format!("/{}", name.strip_prefix('/').unwrap_or(name)))
                .filter(|name| valid_name(name))
                .collect();
            if data.get("subtype").and_then(Value::as_str) == Some("init") {
                if let Some(previous) = self
                    .entries
                    .get(&key)
                    .and_then(|entry| entry.catalog.as_ref())
                {
                    for command in &mut value.commands {
                        if let Some(old) = previous
                            .commands
                            .iter()
                            .find(|old| old.name == command.name)
                        {
                            if command.description.is_empty() {
                                command.description = old.description.clone();
                            }
                            if command.argument_hint.is_none() {
                                command.argument_hint = old.argument_hint.clone();
                            }
                        }
                    }
                }
            }
            apply_terminal_names(&mut value, &terminal_names);
        }
        normalize(&mut value, &key.0);
        self.insert(key, Some(value), terminal_names);
    }
}

fn apply_terminal_names(catalog: &mut Catalog, terminal_names: &HashSet<String>) {
    for command in &mut catalog.commands {
        if terminal_names.contains(&command.name)
            && matches!(command.invocation, Invocation::Native)
        {
            command.kind = "action".into();
            command.invocation =
                Invocation::Action(format!("console:{}", command.name.trim_start_matches('/')));
            command.source = "Pantheon console".into();
            command.reason = None;
        }
    }
}

/// No polling or filesystem watchers: refresh only when a picker opens or the harness pushes metadata.
pub async fn catalog(provider: &str, cwd: &Path, force: bool) -> Result<Catalog, String> {
    validate_provider(provider)?;
    let key = catalog_key(provider, cwd);
    let started = Instant::now();
    let discovery = {
        let mut cache = CACHE
            .lock()
            .map_err(|_| "Command discovery is unavailable.")?;
        if !force {
            if let Some(value) = cache.cached(&key, None) {
                return Ok(value);
            }
        }
        cache.discovery(&key)
    };
    // Coalesce requests for the same project before they occupy one of the
    // bounded process slots. Concurrent picker opens share the completed result.
    let _discovery = discovery.lock().await;
    if let Some(value) = CACHE
        .lock()
        .ok()
        .and_then(|mut cache| cache.cached(&key, Some(started)))
    {
        return Ok(value);
    }
    let _permit = DISCOVERY
        .acquire()
        .await
        .map_err(|_| "Command discovery is unavailable.")?;
    discover_catalog(provider, cwd, key, started).await
}

/// Local actions and the latest known catalog are available without starting a
/// harness. A stale snapshot is for completion; command dispatch still validates.
pub fn snapshot(provider: &str, cwd: &Path) -> Result<Catalog, String> {
    validate_provider(provider)?;
    CACHE
        .lock()
        .map_err(|_| "Command discovery is unavailable.".into())
        .map(|mut cache| cache.snapshot(&catalog_key(provider, cwd)))
}

fn validate_provider(provider: &str) -> Result<(), String> {
    if provider != "pantheon" {
        return Err("Pantheon is the only supported provider.".into());
    }
    Ok(())
}

async fn discover_catalog(
    provider: &str,
    cwd: &Path,
    key: Key,
    started: Instant,
) -> Result<Catalog, String> {
    for attempt in 0..2 {
        let revision = {
            let mut cache = CACHE
                .lock()
                .map_err(|_| "Command discovery is unavailable.")?;
            // A queued refresh can reuse a discovery or live update that landed
            // while it waited. No lock is held while a harness process runs.
            if let Some(value) = cache.cached(&key, Some(started)) {
                return Ok(value);
            }
            cache.begin(&key)
        };
        let discovered = acp::discover(provider, cwd).await;
        let committed = CACHE
            .lock()
            .map_err(|_| "Command discovery is unavailable.")?
            .commit(key.clone(), revision, discovered);
        match committed {
            Commit::Ready(value) => return Ok(value),
            Commit::Invalidated if attempt == 0 => continue,
            Commit::Invalidated => {
                return Err("Commands changed during discovery. Refresh to try again.".into())
            }
        }
    }
    unreachable!()
}

/// Called only for command metadata frames from the parent provider session.
pub fn observe_runtime(provider: &str, cwd: &Path, data: &Value) -> bool {
    let key = catalog_key(provider, cwd);
    let value = acp::parse_runtime(provider, data);
    if let Some(value) = value {
        if let Ok(mut cache) = CACHE.lock() {
            cache.observe(key, data, value);
        }
        true
    } else {
        false
    }
}

fn normalize(catalog: &mut Catalog, _provider: &str) {
    catalog.commands.retain(|command| valid_name(&command.name));
    catalog.commands.truncate(1000);
    for command in &mut catalog.commands {
        command.description = bounded(&command.description, 500);
        command.argument_hint = command
            .argument_hint
            .as_ref()
            .map(|hint| bounded(hint, 160));
        command.reason = command.reason.as_ref().map(|reason| bounded(reason, 500));
    }
    // Controls change Pantheon's persisted state, never hidden CLI state.
    // Other local actions only fill gaps in the provider's command catalog.
    for &(name, description, action, hint) in LOCAL_ACTIONS {
        if let Some(existing) = catalog
            .commands
            .iter_mut()
            .find(|command| command.name == name)
        {
            let console_only = matches!(&existing.invocation, Invocation::Action(action) if action.starts_with("console:"));
            if existing.kind != "unsupported" && !console_only && !is_local_control(name) {
                continue;
            }
            *existing = action_command(name, description, action, hint);
        } else {
            catalog
                .commands
                .push(action_command(name, description, action, hint));
        }
    }
    let mut names = HashSet::new();
    catalog
        .commands
        .retain(|command| names.insert(command.name.clone()));
    catalog.warnings.truncate(4);
    catalog
        .warnings
        .iter_mut()
        .for_each(|warning| *warning = bounded(warning, 500));
}

const LOCAL_ACTIONS: &[(&str, &str, &str, Option<&str>)] = &[
    ("/new", "Start a new task in this project", "new", None),
    (
        "/model",
        "Choose a model and reasoning effort",
        "model",
        None,
    ),
    (
        "/permissions",
        "Choose this task’s approval behavior",
        "permissions",
        None,
    ),
    ("/settings", "Open Pantheon settings", "settings", None),
    (
        "/theme",
        "Switch between dark and light appearance",
        "theme",
        None,
    ),
    ("/help", "Browse available slash commands", "help", None),
    (
        "/skills",
        "Browse Pantheon skills and commands",
        "skills",
        None,
    ),
    (
        "/diff",
        "Show changes in this task’s checkout",
        "diff",
        None,
    ),
    (
        "/status",
        "Show this task’s model and context usage",
        "status",
        None,
    ),
    (
        "/context",
        "Show the context reported by Pantheon",
        "status",
        None,
    ),
    ("/mcp", "Show configured integrations", "integrations", None),
    (
        "/resume",
        "Resume another conversation",
        "conversations",
        Some("[search]"),
    ),
    ("/rename", "Rename this task", "rename", Some("<title>")),
    ("/copy", "Copy the latest agent response", "copy", None),
    ("/pwd", "Show this task’s working directory", "pwd", None),
    (
        "/console",
        "Open Pantheon in a console",
        "console",
        Some("[command]"),
    ),
];

fn action_command(name: &str, description: &str, action: &str, hint: Option<&str>) -> SlashCommand {
    SlashCommand {
        id: format!("pantheon:{action}:{name}"),
        name: name.into(),
        description: description.into(),
        argument_hint: hint.map(str::to_string),
        kind: "action".into(),
        source: "Pantheon".into(),
        reason: None,
        invocation: Invocation::Action(action.into()),
    }
}

fn is_local_control(name: &str) -> bool {
    matches!(
        name,
        "/new"
            | "/model"
            | "/permissions"
            | "/settings"
            | "/theme"
            | "/resume"
            | "/rename"
            | "/copy"
            | "/pwd"
            | "/diff"
            | "/console"
    )
}

fn local_control(provider: &str, name: &str) -> Option<SlashCommand> {
    if !is_local_control(name) {
        return None;
    }
    LOCAL_ACTIONS
        .iter()
        .find(|entry| entry.0 == name)
        .map(|&(name, description, action, hint)| {
            let _ = provider;
            action_command(name, description, action, hint)
        })
}

fn bounded(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}
fn valid_name(name: &str) -> bool {
    if name == "/?" {
        return true;
    }
    name.len() > 1
        && name.len() <= 160
        && name.starts_with('/')
        && name[1..]
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'))
}

pub fn split_command(text: &str) -> Option<(&str, &str)> {
    let text = text.trim_start();
    let end = text.find(char::is_whitespace).unwrap_or(text.len());
    let name = &text[..end];
    valid_name(name).then(|| (name, text[end..].trim_start()))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Resolution {
    pub action: Option<String>,
    pub arguments: String,
    pub accepts_attachments: bool,
    #[serde(skip)]
    pub maintenance: bool,
    #[serde(skip)]
    pub prompt: String,
    #[serde(skip)]
    pub skill: Option<(String, PathBuf)>,
}

pub async fn resolve(provider: &str, cwd: &Path, text: &str) -> Result<Resolution, String> {
    let Some((name, _)) = split_command(text) else {
        return Ok(plain_resolution(text));
    };
    if is_local_control(name) {
        return resolve_from_catalog(provider, text, &Catalog::default());
    }
    let catalog = catalog(provider, cwd, false).await?;
    resolve_from_catalog(provider, text, &catalog)
}

fn plain_resolution(text: &str) -> Resolution {
    Resolution {
        action: None,
        arguments: String::new(),
        accepts_attachments: true,
        maintenance: false,
        prompt: text.into(),
        skill: None,
    }
}

fn resolve_from_catalog(
    provider: &str,
    text: &str,
    catalog: &Catalog,
) -> Result<Resolution, String> {
    let mut result = plain_resolution(text);
    let Some((name, arguments)) = split_command(text) else {
        return Ok(result);
    };
    let local = local_control(provider, name);
    let command = local
        .as_ref()
        .or_else(|| catalog.commands.iter().find(|command| command.name == name))
        .ok_or_else(|| {
            format!(
                "Unknown command {name}. Type / to browse Pantheon commands, or refresh the list."
            )
        })?;
    match &command.invocation {
        Invocation::Native => {
            // Pantheon's ACP surface accepts a literal space separator, not tabs/newlines.
            result.prompt = format!(
                "{name}{}",
                if arguments.is_empty() {
                    String::new()
                } else {
                    format!(" {arguments}")
                }
            );
            result.maintenance = matches!(
                name,
                "/compact"
                    | "/status"
                    | "/context"
                    | "/usage"
                    | "/cost"
                    | "/mcp"
                    | "/tasks"
                    | "/help"
            );
            result.accepts_attachments = !result.maintenance;
        }
        Invocation::Action(action) => {
            if !arguments.is_empty() && !action_accepts_arguments(action) {
                return Err(format!(
                    "{name} opens a Pantheon control and does not accept arguments."
                ));
            }
            result.action = Some(action.clone());
            result.arguments = arguments.into();
        }
    }
    Ok(result)
}

fn action_accepts_arguments(action: &str) -> bool {
    action.starts_with("console:")
        || matches!(
            action,
            "rename"
                | "goal"
                | "usage"
                | "conversations"
                | "fork"
                | "export"
                | "worktree"
                | "plan"
                | "fast"
                | "mention"
                | "cd"
                | "init"
                | "recap"
                | "console"
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn native_catalog(names: &[&str]) -> Catalog {
        Catalog {
            commands: names
                .iter()
                .map(|name| SlashCommand {
                    id: format!("test:{name}"),
                    name: (*name).into(),
                    description: format!("Run {name}"),
                    argument_hint: None,
                    kind: "prompt".into(),
                    source: "test".into(),
                    reason: None,
                    invocation: Invocation::Native,
                })
                .collect(),
            warnings: vec![],
        }
    }

    fn key(provider: &str) -> Key {
        (provider.into(), PathBuf::from("project"))
    }

    fn command<'a>(catalog: &'a Catalog, name: &str) -> &'a SlashCommand {
        catalog
            .commands
            .iter()
            .find(|command| command.name == name)
            .unwrap()
    }

    fn ready(commit: Commit) -> Catalog {
        match commit {
            Commit::Ready(value) => value,
            Commit::Invalidated => panic!("Expected a catalog"),
        }
    }

    #[test]
    fn commands_do_not_capture_file_paths_or_embedded_prose() {
        assert_eq!(
            split_command(" /skill:name  keep  spacing"),
            Some(("/skill:name", "keep  spacing"))
        );
        assert_eq!(split_command("/Users/me/src is the project"), None);
        assert_eq!(split_command("Read /file"), None);
        assert_eq!(split_command("/"), None);
    }

    #[test]
    fn local_actions_do_not_shadow_provider_commands() {
        let mut catalog = Catalog {
            commands: vec![SlashCommand {
                id: "pantheon:help".into(),
                name: "/help".into(),
                description: "Native help".into(),
                argument_hint: None,
                kind: "prompt".into(),
                source: "Pantheon".into(),
                reason: None,
                invocation: Invocation::Native,
            }],
            warnings: vec![],
        };
        normalize(&mut catalog, "pantheon");
        assert!(matches!(
            catalog
                .commands
                .iter()
                .find(|v| v.name == "/help")
                .unwrap()
                .invocation,
            Invocation::Native
        ));
        assert_eq!(
            catalog
                .commands
                .iter()
                .filter(|v| v.name == "/model")
                .count(),
            1
        );
    }

    #[test]
    fn a_live_catalog_wins_over_an_older_in_flight_discovery() {
        let mut cache = CommandCache::default();
        let key = key("pantheon");
        let revision = cache.begin(&key);
        cache.observe(key.clone(), &json!({}), native_catalog(&["/skill:current"]));
        let value = ready(cache.commit(
            key.clone(),
            revision,
            Ok(native_catalog(&["/skill:removed"])),
        ));
        assert!(value
            .commands
            .iter()
            .any(|entry| entry.name == "/skill:current"));
        assert!(!value
            .commands
            .iter()
            .any(|entry| entry.name == "/skill:removed"));
        assert_eq!(
            cache.cached(&key, None).unwrap().commands[0].name,
            "/skill:current"
        );
    }

    #[test]
    fn failed_refresh_preserves_last_good_commands_without_leaking_errors() {
        let mut cache = CommandCache::default();
        let key = key("pantheon");
        cache.insert(
            key.clone(),
            Some(native_catalog(&["/ship"])),
            HashSet::new(),
        );
        for _ in 0..2 {
            let revision = cache.begin(&key);
            let value = ready(cache.commit(
                key.clone(),
                revision,
                Err("secret token at /private/config".into()),
            ));
            assert_eq!(command(&value, "/ship").description, "Run /ship");
            assert_eq!(value.warnings, vec![REFRESH_WARNING]);
        }
        assert!(cache.cached(&key, None).is_some());
    }

    #[test]
    fn lru_cache_is_bounded_and_expired_entries_are_not_reused() {
        let mut cache = CommandCache::default();
        let now = Instant::now();
        for index in 0..16 {
            let key = ("pantheon".into(), PathBuf::from(format!("project-{index}")));
            cache.insert(
                key.clone(),
                Some(native_catalog(&["/compact"])),
                HashSet::new(),
            );
            cache.entries.get_mut(&key).unwrap().touched = now - Duration::from_secs(30 - index);
        }
        let oldest = ("pantheon".into(), PathBuf::from("project-0"));
        let next_oldest = ("pantheon".into(), PathBuf::from("project-1"));
        assert!(cache.cached(&oldest, None).is_some());
        cache.insert(
            key("pantheon"),
            Some(native_catalog(&["/status"])),
            HashSet::new(),
        );
        assert_eq!(cache.entries.len(), 16);
        assert!(cache.entries.contains_key(&oldest));
        assert!(!cache.entries.contains_key(&next_oldest));
        cache.entries.get_mut(&oldest).unwrap().updated = now - CACHE_AGE;
        assert!(cache.cached(&oldest, None).is_none());
        assert!(
            cache.entries[&oldest].catalog.is_some(),
            "Refresh failures can still recover the prior catalog"
        );
    }

    #[test]
    fn startup_timeouts_preserve_cached_commands_without_echoing_process_output() {
        let mut cache = CommandCache::default();
        let key = key("pantheon");
        let revision = cache.insert(
            key.clone(),
            Some(native_catalog(&["/review"])),
            HashSet::new(),
        );
        let timeout = crate::providers::acp::ACP_STARTUP_TIMEOUT;
        let catalog = ready(cache.commit(
            key.clone(),
            revision,
            Err(format!("{timeout} SECRET process output")),
        ));
        assert_eq!(catalog.warnings, vec![timeout]);
        assert_eq!(command(&catalog, "/review").name, "/review");
        let revision = cache.begin(&key);
        let catalog = ready(cache.commit(key, revision, Err(acp::NO_COMMANDS.into())));
        assert_eq!(catalog.warnings, vec![acp::NO_COMMANDS]);
        assert_eq!(command(&catalog, "/review").name, "/review");
    }

    #[test]
    fn cache_snapshots_retain_stale_commands_while_refresh_is_required() {
        let mut cache = CommandCache::default();
        let key = key("pantheon");
        cache.insert(
            key.clone(),
            Some(native_catalog(&["/review"])),
            HashSet::new(),
        );
        cache.entries.get_mut(&key).unwrap().updated = Instant::now() - CACHE_AGE;
        assert!(cache.cached(&key, None).is_none());
        assert_eq!(command(&cache.snapshot(&key), "/review").name, "/review");
        assert_eq!(command(&cache.snapshot(&key), "/model").kind, "action");
    }

    #[test]
    fn evicted_discovery_cannot_overwrite_a_recreated_key() {
        let mut cache = CommandCache::default();
        let key = key("pantheon");
        let old_revision = cache.begin(&key);
        cache.entries.remove(&key);
        let new_revision = cache.begin(&key);
        assert_ne!(old_revision, new_revision);
        assert!(matches!(
            cache.commit(key.clone(), old_revision, Ok(native_catalog(&["/old"]))),
            Commit::Invalidated
        ));
        let value = ready(cache.commit(key, new_revision, Ok(native_catalog(&["/new-skill"]))));
        assert_eq!(value.commands[0].name, "/new-skill");
    }

    #[test]
    fn native_commands_normalize_only_the_separator_and_keep_attachments() {
        let value = native_catalog(&["/skill:build", "/compact"]);
        for text in [
            "/skill:build\t  keep  spacing\nand lines",
            "/skill:build\nkeep  spacing\nand lines",
        ] {
            let result = resolve_from_catalog("pantheon", text, &value).unwrap();
            assert_eq!(result.prompt, "/skill:build keep  spacing\nand lines");
            assert!(result.accepts_attachments);
            assert!(!result.maintenance);
        }
        let result = resolve_from_catalog("pantheon", "/compact\t\n ", &value).unwrap();
        assert_eq!(result.prompt, "/compact");
        assert!(result.maintenance);
        assert!(!result.accepts_attachments);
        assert!(
            resolve_from_catalog("pantheon", "Build from this screenshot", &value)
                .unwrap()
                .accepts_attachments
        );
    }

    #[test]
    fn app_controls_override_native_commands_without_changing_native_work_commands() {
        let mut value = native_catalog(&[
            "/model",
            "/permissions",
            "/new",
            "/theme",
            "/rename",
            "/compact",
            "/review",
            "/help",
            "/status",
        ]);
        normalize(&mut value, "pantheon");
        for name in ["/model", "/permissions", "/new", "/theme", "/rename"] {
            assert!(matches!(
                command(&value, name).invocation,
                Invocation::Action(_)
            ));
            assert_eq!(command(&value, name).source, "Pantheon");
            assert!(resolve_from_catalog("pantheon", name, &value)
                .unwrap()
                .action
                .is_some());
        }
        for name in ["/compact", "/review", "/help", "/status"] {
            assert!(matches!(
                command(&value, name).invocation,
                Invocation::Native
            ));
            assert!(resolve_from_catalog("pantheon", name, &value)
                .unwrap()
                .action
                .is_none());
        }
        assert!(resolve_from_catalog("pantheon", "/model opus", &value)
            .err()
            .unwrap()
            .contains("does not accept arguments"));
        assert_eq!(
            resolve_from_catalog("pantheon", "/rename\tMy new task", &value)
                .unwrap()
                .arguments,
            "My new task"
        );
    }

    #[tokio::test]
    async fn local_controls_resolve_without_waiting_for_discovery() {
        let permits = DISCOVERY.acquire_many(2).await.unwrap();
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            resolve("pantheon", Path::new("missing-project"), "/model"),
        )
        .await
        .expect("A local control must not wait for CLI discovery")
        .unwrap();
        assert_eq!(result.action.as_deref(), Some("model"));
        let console = resolve("pantheon", Path::new("missing-project"), "/console /login")
            .await
            .unwrap();
        assert_eq!(console.action.as_deref(), Some("console"));
        assert_eq!(console.arguments, "/login");
        let resume = resolve("pantheon", Path::new("missing-project"), "/resume game")
            .await
            .unwrap();
        assert_eq!(resume.action.as_deref(), Some("conversations"));
        assert_eq!(resume.arguments, "game");
        drop(permits);
    }

    #[test]
    fn console_commands_preserve_arguments_without_becoming_model_prompts() {
        let mut value = native_catalog(&["/interactive-only"]);
        apply_terminal_names(&mut value, &HashSet::from(["/interactive-only".into()]));
        let result =
            resolve_from_catalog("pantheon", "/interactive-only exact  args", &value).unwrap();
        assert_eq!(result.action.as_deref(), Some("console:interactive-only"));
        assert_eq!(result.arguments, "exact  args");
        assert!(result.skill.is_none());
        assert!(!result.maintenance);
        assert_eq!(command(&value, "/interactive-only").kind, "action");
    }

    #[test]
    fn each_argument_aware_app_route_preserves_user_input() {
        for action in [
            "goal",
            "usage",
            "conversations",
            "fork",
            "export",
            "worktree",
            "plan",
            "fast",
            "mention",
            "cd",
            "init",
            "recap",
            "console",
            "console:feedback",
        ] {
            let value = Catalog {
                commands: vec![action_command(
                    "/fixture",
                    "Fixture",
                    action,
                    Some("[arguments]"),
                )],
                warnings: Vec::new(),
            };
            let result =
                resolve_from_catalog("pantheon", "/fixture preserve  this", &value).unwrap();
            assert_eq!(result.action.as_deref(), Some(action));
            assert_eq!(result.arguments, "preserve  this");
        }
        for action in [
            "model",
            "permissions",
            "jobs",
            "archive",
            "delete",
            "preferences",
        ] {
            let value = Catalog {
                commands: vec![action_command("/fixture", "Fixture", action, None)],
                warnings: Vec::new(),
            };
            assert!(resolve_from_catalog("pantheon", "/fixture extra", &value).is_err());
        }
    }
}
