//! Direct, bounded stdio adapters. CLI credentials stay with each harness;
//! the optional GLM Coding Plan credential is supplied by native secure storage.
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, Command},
    sync::mpsc::UnboundedReceiver,
};

mod attachments;
#[cfg(all(test, unix))]
pub(crate) mod concurrent_process_tests;
mod host_inputs;
mod host_tools;
mod mcp_host;
use host_tools::apply_step;
pub use host_tools::ToolHost;
pub(crate) mod acp;
mod diagnostics;
mod goals;
mod launch;
pub(crate) use launch::{command, executable_path, external_path, widen_process_path};
pub(crate) mod pipe;
mod steering;
#[cfg(test)]
mod stream_tests;
mod subagents;
mod text_stream;
#[cfg(windows)]
pub(crate) mod windows_job;
pub use subagents::SubagentActivity;

const MAX_FRAME: usize = 4 * 1024 * 1024;
const STDERR_LIMIT: usize = 16 * 1024;
const TOOL_INPUT_LIMIT: usize = 4096;
const TOOL_OUTPUT_LIMIT: usize = 8192;
const TOOL_CACHE_LIMIT: usize = 64;
const PREVIEW_TRUNCATED: &str = "\n[Preview truncated]";
pub(crate) const PROVIDERS: [(&str, &str); 1] = [("pantheon", "Pantheon")];

/// Every provider `run_inner` can launch. This is the single allowlist: the model
/// catalog, team rosters and connection checks all validate against it, so a
/// provider cannot be advertised and then refused at launch.
pub(crate) const HARNESSES: &[&str] = &["pantheon"];

/// Never runs an agent-reviewed task under some other policy.
/// Pantheon has no agent-reviewed approvals, so `review` has no peer mode to
/// select. Reject it here rather than starting a run the server will refuse.
fn review_supported(_provider: &str, mode: &str) -> Result<(), String> {
    if mode == "review" {
        return Err("Pantheon has no agent-reviewed approvals. Choose Standard, Accept edits, Plan or Full access.".into());
    }
    Ok(())
}

/// Harnesses that take app instructions as a system prompt (`--append-system-prompt`).
pub fn takes_system_instructions(provider: &str) -> bool {
    matches!(provider, "pantheon")
}

#[derive(Clone, Debug, Default)]
pub struct HarnessContext {
    pub instructions: String,
    pub skills: Vec<String>,
    pub git_head: String,
    pub today: String,
    pub todos: Vec<String>,
}

impl HarnessContext {
    /// Build a context for one run. Git state and date are volatile by design.
    pub fn for_run(cwd: &Path, instructions: String) -> Self {
        let git_head = std::process::Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .current_dir(cwd)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .unwrap_or_default();
        Self {
            instructions,
            git_head,
            today: chrono::Local::now().format("%Y-%m-%d").to_string(),
            ..Self::default()
        }
    }

    /// Stable content only. This is what goes into the cached system prompt.
    pub fn static_prefix(&self) -> String {
        let mut out = self.instructions.clone();
        for name in &self.skills {
            out.push_str("\nskill: ");
            out.push_str(name);
        }
        out
    }

    /// Volatile content only. Append this to the first prompt of a fresh run.
    pub fn dynamic_suffix(&self) -> String {
        format!(
            "\n\nruntime context: git HEAD {}, date {}, todos {:?}\n",
            self.git_head, self.today, self.todos
        )
    }
}

#[derive(Clone, Debug)]
pub struct RunConfig {
    pub session_id: String,
    pub provider: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub mode: String,
    pub cwd: PathBuf,
    pub resume_id: Option<String>,
    pub prompt: String,
    /// Fixed app instructions. Claude-family harnesses receive them as an appended system
    /// prompt, which is cached with the rest of the prefix instead of re-sent in every message.
    pub instructions: String,
    /// Exact skill reference resolved from the provider's native command catalog.
    pub skill: Option<(String, PathBuf)>,
    /// The goal the model should keep pursuing, sent to Pantheon as
    /// `_pantheon/goal` before the turn. Pantheon carries a goal in the
    /// session's system prompt, so it survives the turn that set it.
    pub goal_objective: Option<String>,
    /// Validated per-task settings applied before any new Codex turn.
    /// Only native-validated, managed screenshot paths; never arbitrary web content.
    pub images: Vec<PathBuf>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentEvent {
    pub session_id: String,
    pub kind: String,
    pub text: String,
    pub request_id: Option<String>,
    pub data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolActivity>,
}

fn explicit_write_paths(name: &str, input: &Value) -> Option<Vec<String>> {
    match name {
        "Write" | "Edit" | "MultiEdit" => Some(
            input["file_path"]
                .as_str()
                .filter(|p| p.len() <= 4096)
                .map(str::to_owned)
                .into_iter()
                .collect(),
        ),
        "NotebookEdit" => Some(
            input["notebook_path"]
                .as_str()
                .filter(|p| p.len() <= 4096)
                .map(str::to_owned)
                .into_iter()
                .collect(),
        ),
        "Bash" | "Command" => Some(vec![]),
        _ => None,
    }
}
fn tool_write_paths(name: &str, input: &str) -> Option<Vec<String>> {
    if matches!(name, "Bash" | "Command") {
        return Some(vec![]);
    }
    if !matches!(name, "Write" | "Edit" | "MultiEdit" | "NotebookEdit") {
        return None;
    }
    serde_json::from_str(input)
        .ok()
        .and_then(|input| explicit_write_paths(name, &input))
}

/// Write targets from a tool call with already parsed JSON arguments. Used by
/// team workers, whose tool host has to gate writes against their file scope.
pub(crate) fn tool_write_paths_value(name: &str, args: &Value) -> Option<Vec<String>> {
    explicit_write_paths(name, args)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolActivity {
    pub id: String,
    pub name: String,
    pub status: String,
    pub input: String,
    pub output: String,
    /// Explicit write targets from the provider, never inferred from checkout status.
    /// An empty list denotes a command whose write targets are not reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_paths: Option<Vec<String>>,
}

// The serializer stops as soon as the preview fills: no multi-megabyte temporary
// String or cloned JSON subtree is built to display a small tool result.
struct Preview {
    bytes: Vec<u8>,
    limit: usize,
    truncated: bool,
}
impl Preview {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            truncated: false,
        }
    }
    fn text(&mut self, text: &str) {
        let _ = self.write_all(text.as_bytes());
    }
    fn finish(mut self) -> String {
        if self.truncated {
            self.bytes
                .truncate(self.limit.saturating_sub(PREVIEW_TRUNCATED.len()));
        }
        // A bounded byte write may stop inside a Unicode character.
        if let Err(error) = std::str::from_utf8(&self.bytes) {
            self.bytes.truncate(error.valid_up_to());
        }
        let mut text = String::from_utf8(self.bytes).unwrap_or_default();
        if self.truncated {
            text.push_str(PREVIEW_TRUNCATED);
        }
        text
    }
}
impl Write for Preview {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.truncated {
            return Err(std::io::Error::other("Preview full"));
        }
        let count = bytes.len().min(self.limit.saturating_sub(self.bytes.len()));
        self.bytes.extend_from_slice(&bytes[..count]);
        if count != bytes.len() {
            self.truncated = true;
            return Err(std::io::Error::other("Preview full"));
        }
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn limited(text: &str, limit: usize) -> String {
    let mut preview = Preview::new(limit);
    preview.text(text);
    preview.finish()
}
fn preview_json(value: &Value, limit: usize) -> String {
    let mut preview = Preview::new(limit);
    if let Some(text) = value.as_str() {
        preview.text(text);
    } else if !value.is_null() {
        let _ = serde_json::to_writer_pretty(&mut preview, &ToolInput(value));
    }
    preview.finish()
}

struct ToolInput<'a>(&'a Value);
impl Serialize for ToolInput<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        const FIRST: &[&str] = &[
            "command",
            "file_path",
            "path",
            "pattern",
            "query",
            "url",
            "description",
        ];
        let Some(object) = self.0.as_object() else {
            return self.0.serialize(serializer);
        };
        let mut map = serializer.serialize_map(Some(object.len()))?;
        for key in FIRST {
            if let Some(value) = object.get(*key) {
                map.serialize_entry(key, value)?;
            }
        }
        for (key, value) in object {
            if !FIRST.contains(&key.as_str()) {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

// Structured results can contain both useful JSON and binary resources. Project
// them while serializing, without cloning the result or exposing opaque metadata.
struct ToolOutput<'a>(&'a Value);
impl Serialize for ToolOutput<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Value::Object(object) => {
                if matches!(
                    field(self.0, "type"),
                    "image"
                        | "inputImage"
                        | "image_url"
                        | "audio"
                        | "inputAudio"
                        | "video"
                        | "document"
                ) {
                    return serializer.serialize_str("[Non-text content omitted]");
                }
                let mut map = serializer.serialize_map(None)?;
                for (key, value) in object {
                    if !matches!(key.as_str(), "_meta" | "blob" | "base64") {
                        map.serialize_entry(key, &ToolOutput(value))?;
                    }
                }
                map.end()
            }
            Value::Array(items) => {
                let mut sequence = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    sequence.serialize_element(&ToolOutput(item))?;
                }
                sequence.end()
            }
            Value::String(value)
                if value.starts_with("data:image/") || value.starts_with("data:audio/") =>
            {
                serializer.serialize_str("[Non-text content omitted]")
            }
            _ => self.0.serialize(serializer),
        }
    }
}
fn output_json(value: &Value, limit: usize) -> String {
    if let Some(text) = value.as_str() {
        return limited(
            if text.starts_with("data:image/") || text.starts_with("data:audio/") {
                "[Non-text content omitted]"
            } else {
                text
            },
            limit,
        );
    }
    let mut preview = Preview::new(limit);
    let _ = serde_json::to_writer_pretty(&mut preview, &ToolOutput(value));
    preview.finish()
}
fn field<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or_default()
}

fn content_preview(value: &Value, limit: usize) -> String {
    fn content(value: &Value, preview: &mut Preview, depth: usize) {
        if preview.truncated {
            return;
        }
        if depth > 8 {
            preview.text("[Nested content omitted]");
            return;
        }
        match value {
            Value::String(text) => preview.text(text),
            Value::Array(items) => {
                for (index, item) in items.iter().take(128).enumerate() {
                    if index > 0 {
                        preview.text("\n");
                    }
                    content(item, preview, depth + 1);
                    if preview.truncated {
                        break;
                    }
                }
                if items.len() > 128 {
                    preview.truncated = true;
                }
            }
            Value::Object(_) => match field(value, "type") {
                "text" | "inputText" => preview.text(field(value, "text")),
                "content" => content(&value["content"], preview, depth + 1),
                "resource" => {
                    if value["resource"]["text"].is_string() {
                        preview.text(field(&value["resource"], "text"));
                    } else {
                        preview.text("[Binary resource omitted]");
                    }
                }
                "resource_link" => {
                    preview.text(field(value, "name"));
                    preview.text("\n");
                    preview.text(field(value, "uri"));
                }
                "diff" => {
                    preview.text(field(value, "path"));
                    preview.text("\n--- Before\n");
                    preview.text(field(value, "oldText"));
                    preview.text("\n+++ After\n");
                    preview.text(field(value, "newText"));
                }
                "terminal" => {
                    preview.text("Terminal: ");
                    preview.text(field(value, "terminalId"));
                    preview.text(" (output not supplied)");
                }
                _ => preview.text("[Non-text content omitted]"),
            },
            _ => {}
        }
    }
    let mut preview = Preview::new(limit);
    content(value, &mut preview, 0);
    preview.finish()
}

pub enum RunInput {
    Stop,
    /// A message for the agent while it works: injected mid-turn where the harness supports it.
    /// The reply is `{"steered":true,"id","mode":"live"|"next-turn"}`; `steer` events follow its delivery.
    Steer {
        id: String,
        text: String,
        reply: tokio::sync::oneshot::Sender<Result<Value, String>>,
    },
    Approval {
        request_id: String,
        approved: bool,
    },
    UserInput {
        request_id: String,
        response: Value,
        reply: Option<tokio::sync::oneshot::Sender<Result<(), String>>>,
    },
}
pub type Sink = Arc<dyn Fn(AgentEvent) + Send + Sync>;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
    pub installed: bool,
    pub path: Option<String>,
    pub version: String,
    pub models: Vec<ModelInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub reasoning_efforts: Vec<ReasoningEffortInfo>,
    pub default_reasoning_effort: Option<String>,
    pub context_window: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningEffortInfo {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// Effort ids come from the agent's own catalog (`thought_level`), so the host
/// only enforces that a stored value stays bounded and printable. Whether the
/// provider can be started at all is decided when its process is launched.
pub fn validate_reasoning_effort(_provider: &str, effort: Option<&str>) -> Result<(), String> {
    let valid = match effort {
        None => true,
        Some(value) => {
            !value.is_empty()
                && value.len() <= 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        }
    };
    if valid {
        Ok(())
    } else {
        Err("Unsupported reasoning effort for this provider".into())
    }
}

impl ModelInfo {
    fn default_choice() -> Self {
        Self {
            id: "default".into(),
            name: "Default".into(),
            description: "Use the model configured in the CLI".into(),
            reasoning_efforts: Vec::new(),
            default_reasoning_effort: None,
            context_window: None,
        }
    }
}

pub(crate) fn executable(name: &str) -> Option<PathBuf> {
    if let Some(path) = crate::provider_settings::executable_override(name) {
        return Some(path);
    }
    let found = launch::find(executable_name(name));
    if found.is_some() {
        return found;
    }
    // Pantheon dev checkouts and per-user installs that launch::find misses.
    if name == "pantheon" {
        if let Ok(home) = std::env::var("HOME") {
            for candidate in [
                format!("{home}/projects/pantheon/target/release/pantheon"),
                format!("{home}/.local/bin/pantheon"),
            ] {
                let path = PathBuf::from(&candidate);
                if path.exists() {
                    return Some(path);
                }
            }
        }
    }
    None
}

fn executable_name(name: &str) -> &str {
    // One agent, one executable: the name the caller asked for is the name on disk.
    name
}

/// Any command-line tool (`gh`, `git`…) where an interactive shell would find it, even
/// when Pantheon was opened from Finder or the Dock with a minimal PATH.
pub(crate) fn tool_path(name: &str) -> Option<std::path::PathBuf> {
    launch::find(name)
}

fn version(path: &Path) -> Option<String> {
    let mut child = launch::std_command(path)
        .ok()?
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    #[cfg(windows)]
    let _job = windows_job::Job::attach(child.id());
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if child.try_wait().ok()?.is_some() {
            let output = child.wait_with_output().ok()?;
            return Some(
                String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .chars()
                    .take(150)
                    .collect(),
            );
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Blocking discovery; invoke on a worker thread, not the UI event loop.
pub fn discover() -> Vec<ProviderInfo> {
    PROVIDERS
        .into_iter()
        .map(|(id, name)| {
            let path = executable(id);
            ProviderInfo {
                id: id.into(),
                name: name.into(),
                installed: path.is_some(),
                version: path.as_deref().and_then(version).unwrap_or_default(),
                path: path.map(|p| p.to_string_lossy().into_owned()),
                models: vec![ModelInfo::default_choice()],
            }
        })
        .collect()
}

/// On-demand metadata only: no user prompt or inference. ACP-only servers may
/// create an empty session to advertise their scoped model options.
pub async fn list_models(provider: &str) -> Result<Vec<ModelInfo>, String> {
    list_model_catalog(provider).await
}

fn model_discovery_index(provider: &str) -> Option<usize> {
    PROVIDERS.iter().position(|(id, _)| *id == provider)
}

fn model_discovery_semaphores() -> &'static [tokio::sync::Semaphore; PROVIDERS.len()] {
    static DISCOVERY: OnceLock<[tokio::sync::Semaphore; PROVIDERS.len()]> = OnceLock::new();
    DISCOVERY.get_or_init(|| std::array::from_fn(|_| tokio::sync::Semaphore::new(1)))
}

pub(crate) async fn list_model_catalog(provider: &str) -> Result<Vec<ModelInfo>, String> {
    let index = model_discovery_index(provider).ok_or("Unknown provider")?;
    let semaphore = &model_discovery_semaphores()[index];
    let _permit = semaphore
        .acquire()
        .await
        .map_err(|_| "Model discovery is unavailable".to_owned())?;
    let path = executable(provider).ok_or("Pantheon CLI is not installed or is not on PATH")?;
    pantheon_models(&path).await
}

/// Models the Pantheon CLI can route to. The CLI owns its provider registry
/// (`agent provider catalog`), so ids are reported as `provider/model` — the
/// same shape `session/set_model` accepts.
async fn pantheon_models(path: &Path) -> Result<Vec<ModelInfo>, String> {
    let mut command = Command::new(path);
    command
        .args(["agent", "provider", "catalog"])
        .stdin(Stdio::null());
    let output = tokio::time::timeout(Duration::from_secs(20), command.output())
        .await
        .map_err(|_| "Listing Pantheon providers timed out".to_owned())?
        .map_err(|error| format!("Could not run the Pantheon CLI: {error}"))?;
    if !output.status.success() {
        return Err("The Pantheon CLI could not list its providers".into());
    }
    Ok(parse_pantheon_catalog(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// `  * Name (id): wire -- url` followed by `    models: a, b`. Ids are
/// reported as `provider/model`, matching `session/set_model`.
fn parse_pantheon_catalog(text: &str) -> Vec<ModelInfo> {
    let mut models = vec![ModelInfo::default_choice()];
    let mut provider = String::new();
    for line in text.lines() {
        let line = line.trim();
        if let Some(head) = line
            .strip_prefix("* ")
            .and_then(|rest| rest.split(": ").next())
        {
            if let Some((_, id)) = head.rsplit_once('(') {
                provider = id.trim_end_matches(')').trim().to_string();
            }
            continue;
        }
        let Some(list) = line.strip_prefix("models:") else {
            continue;
        };
        if provider.is_empty() {
            continue;
        }
        for model in list
            .split(',')
            .map(str::trim)
            .filter(|model| valid_model_id(model))
        {
            let id = format!("{provider}/{model}");
            if models.iter().any(|entry| entry.id == id) {
                continue;
            }
            models.push(ModelInfo {
                id,
                name: model.to_string(),
                description: format!("Provided by {provider}"),
                reasoning_efforts: Vec::new(),
                default_reasoning_effort: None,
                context_window: None,
            });
        }
    }
    models
}

fn valid_model_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 200 && !id.chars().any(char::is_control)
}

fn model_text(value: Option<&str>, fallback: &str, limit: usize) -> String {
    let text: String = value
        .unwrap_or_default()
        .chars()
        .filter(|c| !c.is_control())
        .take(limit)
        .collect();
    if text.trim().is_empty() {
        fallback.into()
    } else {
        text.trim().into()
    }
}

// SDK displayName can still be just "Sonnet". Format the provider's resolved
// wire ID instead; never assign a release version to an unresolved alias.
#[derive(Default)]
struct Step {
    events: Vec<AgentEvent>,
    write: Vec<Value>,
    done: bool,
    failure: Option<String>,
}

struct Protocol {
    config: RunConfig,
    remote_id: String,
    phase: u8,
    /// Steering messages this run has accepted but not yet delivered.
    acp_steers: acp::Steers,
    pending: HashMap<String, Value>,
    inputs: HashMap<String, Value>,
    mcp_server: Option<Value>,
    mcp_required: bool,
    mcp_tool_names: HashSet<String>,
    text_streams: text_stream::TextStreams,
    text_sequence: u64,
    message_id: String,
    resume_supported: bool,
    // ACP may publish commands before session/new responds. Keep at most one
    // small frame until the response confirms which session owns it.
    early_advertised_commands: Option<Vec<u8>>,
    tools: HashMap<String, ToolActivity>,
    file_write_tools: HashSet<String>,
    tool_order: VecDeque<String>,
    image_blocks: Vec<Value>,
    reasoning_values: Vec<String>,
    acp_config: Value,
    subagents: subagents::Subagents,
}

fn string(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}
fn short(value: &str) -> String {
    value.chars().take(4000).collect()
}

// Tool snapshots may contain megabytes of output. Only small scalar metadata
// crosses IPC; complete pending approval requests remain in the native adapter.
fn event_metadata(value: &Value) -> Option<Value> {
    let mut metadata = serde_json::Map::new();
    for key in [
        "type",
        "id",
        "status",
        "name",
        "toolCallId",
        "tool_use_id",
        "sessionUpdate",
        "method",
    ] {
        match value.get(key) {
            Some(Value::String(text)) => {
                metadata.insert(
                    key.into(),
                    json!(text.chars().take(128).collect::<String>()),
                );
            }
            Some(scalar @ (Value::Bool(_) | Value::Number(_))) => {
                metadata.insert(key.into(), scalar.clone());
            }
            _ => {}
        }
    }
    if metadata.is_empty() {
        None
    } else {
        Some(Value::Object(metadata))
    }
}

impl Protocol {
    fn emit_text(&mut self, update: Option<text_stream::Update>, step: &mut Step) {
        let Some(update) = update else {
            return;
        };
        if let Some((id, phase)) = update.boundary {
            let mut event = self.event("message_start", "", None);
            event.data = Some(json!({"messageId": id}));
            if let Some(phase) = phase {
                event.data.as_mut().unwrap()["phase"] = json!(phase);
            }
            step.events.push(event);
        }
        step.events.push(self.event("text", update.text, None));
    }
    fn text_delta(&mut self, id: &str, phase: Option<&str>, text: &str, step: &mut Step) {
        let update = self.text_streams.delta(id, phase, text);
        self.emit_text(update, step);
    }
    fn next_text_id(&mut self) -> String {
        self.text_sequence += 1;
        format!("{}-message-{}", self.config.provider, self.text_sequence)
    }
    fn new(config: RunConfig) -> Self {
        Self {
            remote_id: config.resume_id.clone().unwrap_or_default(),
            config,
            phase: 0,
            acp_steers: Default::default(),
            pending: HashMap::new(),
            inputs: HashMap::new(),
            mcp_server: None,
            mcp_required: true,
            mcp_tool_names: HashSet::new(),
            text_streams: text_stream::TextStreams::default(),
            text_sequence: 0,
            message_id: String::new(),
            resume_supported: false,
            early_advertised_commands: None,
            tools: HashMap::new(),
            file_write_tools: HashSet::new(),
            tool_order: VecDeque::new(),
            image_blocks: Vec::new(),
            reasoning_values: Vec::new(),
            acp_config: Value::Null,
            subagents: subagents::Subagents::default(),
        }
    }
    fn usage(&self, used: u64, window: Option<u64>, step: &mut Step) {
        // Match the storage/UI limit and never turn a negative/malformed metric
        // into a fabricated zero or an unbounded percentage.
        if used > 1_000_000_000 || window.is_some_and(|value| value == 0 || value > 1_000_000_000) {
            return;
        }
        let mut event = self.event("usage", "", None);
        event.data = Some(json!({"usedTokens":used,"contextWindow":window}));
        step.events.push(event);
    }
    fn config_options(&mut self, result: &Value) {
        if let Some(options) = result["configOptions"].as_array() {
            self.reasoning_values.clear();
            if let Some(thinking) = options
                .iter()
                .take(32)
                .find(|option| option["id"] == "thinking")
            {
                self.reasoning_values = thinking["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(8)
                    .filter_map(|option| option["value"].as_str())
                    .filter(|value| ["on", "off"].contains(value))
                    .map(str::to_owned)
                    .collect();
            }
        }
    }
    fn prompt_content(&mut self) -> Vec<Value> {
        // Move encoded screenshots into the outgoing frame so the protocol does
        // not retain a second copy for the rest of a potentially long turn.
        let mut content = std::mem::take(&mut self.image_blocks);
        let text = std::mem::take(&mut self.config.prompt);
        if !text.trim().is_empty() {
            content.push(json!({"type":"text","text":text}));
        }
        if self.config.provider == "pantheon" {
            if let Some((name, path)) = self.config.skill.take() {
                content.push(json!({"type":"skill","name":name,"path":path}));
            }
        }
        content
    }
    fn event(&self, kind: &str, text: impl Into<String>, data: Option<&Value>) -> AgentEvent {
        let text = text.into();
        AgentEvent {
            session_id: self.config.session_id.clone(),
            kind: kind.into(),
            text: if kind == "text" || kind == "session" {
                text
            } else {
                short(&text)
            },
            request_id: None,
            data: data.and_then(event_metadata),
            tool: None,
        }
    }
    fn tool_update(
        &mut self,
        id: &str,
        name: Option<&str>,
        status: Option<&str>,
        input: Option<String>,
        output: Option<String>,
        step: &mut Step,
    ) {
        if id.is_empty() {
            return;
        }
        let id = limited(id, 256);
        if !self.tools.contains_key(&id) {
            if self.tools.len() >= TOOL_CACHE_LIMIT {
                if let Some(oldest) = self.tool_order.pop_front() {
                    self.tools.remove(&oldest);
                    self.file_write_tools.remove(&oldest);
                }
            }
            self.tool_order.push_back(id.clone());
        }
        let previous = self.tools.get(&id);
        // Claude may replay its complete tool_use after the corresponding result.
        if previous.is_some_and(|tool| tool.status != "running") && status == Some("running") {
            return;
        }
        let mut tool = previous.cloned().unwrap_or_else(|| ToolActivity {
            id: id.clone(),
            name: "Tool".into(),
            status: "running".into(),
            input: String::new(),
            output: String::new(),
            write_paths: None,
        });
        if let Some(name) = name.filter(|name| !name.is_empty()) {
            tool.name = limited(name, 256);
        }
        if let Some(status) = status {
            tool.status = status.into();
        }
        if let Some(input) = input {
            tool.input = limited(&input, TOOL_INPUT_LIMIT);
        }
        if let Some(output) = output {
            tool.output = limited(&output, TOOL_OUTPUT_LIMIT);
        }
        if let Some(paths) = tool_write_paths(&tool.name, &tool.input) {
            tool.write_paths = Some(paths);
        }
        if previous == Some(&tool) {
            return;
        }
        self.tools.insert(id, tool.clone());
        // The authenticated bridge emits these calls itself. Provider tool
        // frames are retained for joining sparse results, but not shown twice.
        if self.mcp_tool_names.contains(&tool.name) {
            return;
        }
        let mut event = self.event("tool", format!("{} · {}", tool.name, tool.status), None);
        event.tool = Some(tool);
        step.events.push(event);
    }
    fn tool_paths(&mut self, id: &str, paths: Option<Vec<String>>, step: &mut Step) {
        let Some(paths) = paths else {
            return;
        };
        let mut bounded = Vec::new();
        let mut bytes = 0usize;
        for path in paths {
            bytes = bytes.saturating_add(path.len());
            if bounded.len() >= 200 || bytes > 16 * 1024 {
                // An invalid sentinel marks partial provenance without inventing
                // a file name or retaining an unbounded provider payload.
                bounded.push(String::new());
                break;
            }
            if !bounded.contains(&path) {
                bounded.push(path);
            }
        }
        let paths = bounded;
        let Some(tool) = self.tools.get_mut(id) else {
            return;
        };
        let changed = tool.write_paths.as_ref() != Some(&paths);
        tool.write_paths = Some(paths);
        let tool = tool.clone();
        if let Some(event) = step
            .events
            .iter_mut()
            .rev()
            .find(|e| e.tool.as_ref().is_some_and(|t| t.id == id))
        {
            event.tool = Some(tool);
        } else if changed {
            let mut event = self.event("tool", format!("{} · {}", tool.name, tool.status), None);
            event.tool = Some(tool);
            step.events.push(event);
        }
    }
    fn tool_call(&mut self, update: &Value, step: &mut Step) {
        let write_kind = matches!(field(update, "kind"), "edit" | "delete" | "move");
        if write_kind {
            self.file_write_tools
                .insert(limited(field(update, "toolCallId"), 256));
        }
        let writes = write_kind || self.file_write_tools.contains(field(update, "toolCallId"));
        let status = match field(update, "status") {
            "completed" => Some("completed"),
            "failed" => Some("failed"),
            "pending" | "in_progress" => Some("running"),
            _ => None,
        };
        let finished = matches!(status, Some("completed" | "failed"))
            || (status.is_none()
                && self
                    .tools
                    .get(field(update, "toolCallId"))
                    .is_some_and(|tool| tool.status != "running"));
        // The agent sends cumulative argument previews before execution and REPLACES
        // content with the result on completion; missing fields are unchanged.
        let input = if let Some(input) = update.get("rawInput").filter(|value| !value.is_null()) {
            Some(preview_json(input, TOOL_INPUT_LIMIT))
        } else if !finished && update.get("content").is_some() {
            Some(content_preview(&update["content"], TOOL_INPUT_LIMIT))
        } else {
            None
        };
        let output = if finished && update.get("content").is_some() {
            Some(content_preview(&update["content"], TOOL_OUTPUT_LIMIT))
        } else {
            update
                .get("rawOutput")
                .filter(|value| !value.is_null())
                .map(|output| output_json(output, TOOL_OUTPUT_LIMIT))
        };
        // ACP argument deltas are cumulative. Retain the latest bounded input
        // locally, then publish it with the complete call or terminal result.
        let buffer_only = self.tools.contains_key(field(update, "toolCallId"))
            && status == Some("running")
            && update.get("content").is_some()
            && update["title"].is_null()
            && update["rawInput"].is_null()
            && update["rawOutput"].is_null();
        let mut buffered = Step::default();
        let target = if buffer_only { &mut buffered } else { step };
        self.tool_update(
            field(update, "toolCallId"),
            update["title"].as_str(),
            status,
            input,
            output,
            target,
        );
        self.file_write_tools
            .retain(|id| self.tools.contains_key(id));
        let paths = if writes {
            let mut paths: Vec<String> = update["locations"]
                .as_array()
                .into_iter()
                .flatten()
                .take(201)
                .filter_map(|location| location["path"].as_str())
                .filter(|p| p.len() <= 4096)
                .map(str::to_owned)
                .collect();
            for key in ["file_path", "path", "notebook_path", "old_path", "new_path"] {
                if let Some(path) = update["rawInput"][key].as_str().filter(|p| p.len() <= 4096) {
                    paths.push(path.into());
                }
            }
            paths.sort();
            paths.dedup();
            if paths.is_empty()
                && self
                    .tools
                    .get(field(update, "toolCallId"))
                    .is_some_and(|tool| tool.write_paths.as_ref().is_some_and(|p| !p.is_empty()))
            {
                None
            } else {
                Some(paths)
            }
        } else if update["kind"] == "execute" {
            Some(vec![])
        } else {
            None
        };
        self.tool_paths(field(update, "toolCallId"), paths, target);
    }
    fn initial(&self) -> Value {
        // Pantheon speaks ACP; `serve acp` is the only adapter this app launches.
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{},"clientInfo":{"name":"pantheon","version":"0.1.0"}}})
    }
    fn approval(&mut self, value: Value, text: String, step: &mut Step) -> Result<(), String> {
        if self.pending.len() + self.inputs.len() >= 64 {
            return Err("Too many simultaneous approval requests".into());
        }
        let request_id = uuid::Uuid::new_v4().to_string();
        let mut event = self.event("approval", short(&text), Some(&value));
        event.request_id = Some(request_id.clone());
        self.pending.insert(request_id, value);
        step.events.push(event);
        Ok(())
    }
    fn cancel(&self) -> Value {
        json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":self.remote_id}})
    }
    fn receive(&mut self, value: Value) -> Result<Step, String> {
        let mut result = self.receive_inner(value);
        if let Ok(step) = &mut result {
            let mut subagents = std::mem::take(&mut self.subagents);
            subagents.observe_tools(&step.events, &self.config);
            step.events.extend(subagents.flush(self));
            self.subagents = subagents;
        }
        result
    }
    fn receive_inner(&mut self, value: Value) -> Result<Step, String> {
        // Pantheon's ask-user rides extension notifications (plan doc §12):
        // `_pantheon/input_request` carries the question + options for the
        // renderer's question form; the answer comes back as the next prompt
        // and `_pantheon/input_resolved` retires the form.
        if self.config.provider == "pantheon" && value["method"] == "_pantheon/input_request" {
            let params = &value["params"];
            let request_id = field(params, "requestId").to_string();
            let question = field(&params["request"]["questions"][0], "question");
            let mut event = self.event("input_request", question, Some(&params["request"]));
            event.request_id = Some(request_id);
            return Ok(Step {
                events: vec![event],
                ..Step::default()
            });
        }
        if self.config.provider == "pantheon" && value["method"] == "_pantheon/input_resolved" {
            let mut event = self.event("input_cancelled", "", None);
            event.request_id = Some(field(&value["params"], "requestId").to_string());
            return Ok(Step {
                events: vec![event],
                ..Step::default()
            });
        }
        self.acp(value)
    }
    fn commands_changed(&self, value: &Value, step: &mut Step) {
        if crate::slash_commands::observe_runtime(&self.config.provider, &self.config.cwd, value) {
            step.events.push(self.event("commands_changed", "", None));
        }
    }
    fn stream(&mut self, value: Value) -> Result<Step, String> {
        let mut step = Step::default();
        if let Some(error) = value.get("error") {
            // The agent reports model-level image capability failures in ACP error
            // data. Keep that useful reason instead of only "Internal error".
            if !self.config.images.is_empty() {
                if let Some(detail) = error["data"]["error"].as_str() {
                    return Err(format!(
                        "Pantheon could not process the screenshot: {}",
                        short(detail)
                    ));
                }
            }
            return Err(error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or(
                    "Pantheon request failed. Run `pantheon login` if authentication is required.",
                )
                .into());
        }
        if let Some(id) = value["id"]
            .as_u64()
            .filter(|_| value.get("result").is_some())
        {
            match id {
                1 => {
                    if value["result"]["protocolVersion"] != 1 {
                        return Err("This Pantheon build speaks an ACP protocol version this app does not. Update one of them.".into());
                    }
                    if !self.image_blocks.is_empty()
                        && value["result"]["agentCapabilities"]["promptCapabilities"]["image"]
                            != true
                    {
                        return Err("This Pantheon build does not accept screenshots over ACP. Update it, or remove the screenshots.".into());
                    }
                    self.resume_supported = value["result"]["agentCapabilities"]
                        ["sessionCapabilities"]
                        .get("resume")
                        .is_some();
                    if self.mcp_server.is_some()
                        && value["result"]["agentCapabilities"]["mcpCapabilities"]["http"] != true
                    {
                        if self.mcp_required {
                            return Err("This Pantheon build does not accept HTTP MCP servers over ACP, which team tools need.".into());
                        }
                        self.mcp_server = None;
                    }
                    let mut params = json!({"cwd":self.config.cwd,"mcpServers":self.mcp_server.iter().collect::<Vec<_>>()});
                    let method = if self.remote_id.is_empty() {
                        "session/new"
                    } else {
                        params["sessionId"] = json!(self.remote_id);
                        if self.resume_supported {
                            "session/resume"
                        } else {
                            "session/load"
                        }
                    };
                    step.write
                        .push(json!({"jsonrpc":"2.0","id":2,"method":method,"params":params}));
                }
                2 => {
                    self.config_options(&value["result"]);
                    let returned = string(&value["result"], "sessionId");
                    if !returned.is_empty() {
                        self.remote_id = returned;
                    }
                    if self.remote_id.is_empty() {
                        return Err(
                            "Pantheon did not return a session ID. Update it, or start a new task."
                                .into(),
                        );
                    }
                    if let Some(objective) = self.config.goal_objective.clone() {
                        step.write.push(json!({"jsonrpc":"2.0","id":7,"method":"_pantheon/goal","params":{"sessionId":self.remote_id,"action":"set","objective":objective}}));
                    }
                    if let Some(bytes) = self.early_advertised_commands.take() {
                        if let Ok(commands) = serde_json::from_slice::<Value>(&bytes) {
                            if commands["params"]["sessionId"] == self.remote_id {
                                self.commands_changed(&commands, &mut step);
                            }
                        }
                    }
                    step.events
                        .push(self.event("session", self.remote_id.clone(), None));
                    let modes = value["result"]["modes"]["availableModes"].as_array();
                    // The server names its own modes; ask for the first id it
                    // advertises that matches the mode the user chose.
                    // Pantheon advertises `build` and `plan`; anything that is
                    // not `plan` is a build session, so the non-plan modes must
                    // fall back to `build` before they reject the launch.
                    let wanted: &[&str] = match self.config.mode.as_str() {
                        "plan" => &["plan", "read-only", "readonly"],
                        "auto" => &["build", "auto", "accept-edits", "acceptEdits", "yolo"],
                        "full" => &[
                            "build",
                            "full",
                            "bypass",
                            "bypass-permissions",
                            "unrestricted",
                        ],
                        _ => &["build", "default", "ask"],
                    };
                    let chosen = modes.and_then(|list| {
                        wanted
                            .iter()
                            .find_map(|want| list.iter().find(|mode| string(mode, "id") == *want))
                    });
                    if let Some(mode) = chosen {
                        step.write.push(json!({"jsonrpc":"2.0","id":4,"method":"session/set_mode","params":{"sessionId":self.remote_id,"modeId":mode["id"]}}));
                        return Ok(step);
                    }
                    return Err(format!(
                        "Pantheon does not advertise the {} access mode. Update Pantheon, or choose another mode.",
                        self.config.mode
                    ));
                }
                4 => self.stream_model(&mut step)?,
                5 => {
                    self.config_options(&value["result"]);
                    self.stream_effort(&mut step)?;
                }
                6 => self.prompt_turn(&mut step),
                3 => step.done = true,
                _ => {}
            }
        }
        let method = string(&value, "method");
        let params = &value["params"];
        match method.as_str() {
            "session/update"=>{
                if self.remote_id.is_empty() && self.phase == 0
                    && params["update"]["sessionUpdate"] == "available_commands_update"
                    && params["sessionId"].as_str().is_some_and(|id| !id.is_empty() && id.len() <= 256)
                {
                    let mut preview = Preview::new(128 * 1024);
                    if value.serialize(&mut serde_json::Serializer::new(&mut preview)).is_ok() {
                        self.early_advertised_commands = Some(preview.bytes);
                    }
                    return Ok(step);
                }
                if params["sessionId"].as_str().is_some_and(|id| id != self.remote_id) {return Ok(step);}
                let update=&params["update"];
                if update["sessionUpdate"] == "available_commands_update" { self.commands_changed(&value, &mut step); }
                if update["sessionUpdate"] == "config_option_update" { self.config_options(update); }
                // session/load can replay history before its response; UI already owns that history.
                if self.phase==0{return Ok(step);}
                match string(update,"sessionUpdate").as_str() {
                    "usage_update"=>{if let Some(used)=update["used"].as_u64() {self.usage(used,update["size"].as_u64(),&mut step);}},
                    "agent_message_chunk"=>{
                        let content=&update["content"];
                        if content["type"]=="text" || content["type"].is_null() {
                            let text=field(content,"text");
                            if !text.is_empty() {
                                if let Some(id)=update["messageId"].as_str().filter(|id| !id.is_empty()) {
                                    self.message_id=limited(id,256);
                                } else if self.message_id.is_empty() {
                                    self.message_id=self.next_text_id();
                                }
                                let id=self.message_id.clone();
                                self.text_delta(&id,None,text,&mut step);
                            }
                        }
                    },
                    "agent_thought_chunk"=>self.message_id.clear(),
                    "tool_call"|"tool_call_update"=>{
                        // Late snapshots of a known tool must not split a later
                        // assistant response. Only a new call closes the segment.
                        let id=field(update,"toolCallId");
                        if !id.is_empty() && !self.tools.contains_key(id) {self.message_id.clear();}
                        self.tool_call(update,&mut step);
                    },
                    "plan"=>step.events.push(self.event("status","Plan updated",Some(update))),
                    _=>{},
                }
            },
            "session/request_permission"=>{
                let options=params["options"].as_array();
                let is_question=options.is_some_and(|options|options.iter().filter(|o|o["kind"]=="allow_once").count()>1 || options.iter().any(|o| {
                    let id=string(o,"optionId");
                    id.strip_prefix('q').and_then(|s|s.split_once("_opt_")).is_some_and(|(number,_)|number.parse::<usize>().is_ok())
                }));
                if is_question {
                    step.events.push(self.event("status","The agent requested structured input. Answer in chat when the turn finishes.",None));
                    step.write.push(json!({"jsonrpc":"2.0","id":value["id"],"result":{"outcome":{"outcome":"cancelled"}}}));
                    return Ok(step);
                }
                let label=string(&params["toolCall"],"title");self.approval(value,if label.is_empty(){"Allow this agent action?".into()}else{label},&mut step)?;
            },
            _ if value.get("id").is_some() && !method.is_empty()=>step.write.push(json!({"jsonrpc":"2.0","id":value["id"],"error":{"code":-32601,"message":"Client capability not supported"}})),
            _=>{},
        }
        Ok(step)
    }
    fn stream_model(&mut self, step: &mut Step) -> Result<(), String> {
        if !self.config.model.is_empty() && self.config.model != "default" {
            self.reasoning_values.clear();
            step.write.push(json!({"jsonrpc":"2.0","id":5,"method":"session/set_model","params":{"sessionId":self.remote_id,"modelId":self.config.model}}));
        } else {
            self.stream_effort(step)?;
        }
        Ok(())
    }
    fn stream_effort(&mut self, step: &mut Step) -> Result<(), String> {
        if let Some(effort) = &self.config.reasoning_effort {
            if !self.reasoning_values.contains(effort) {
                return Err("This Pantheon model does not advertise the selected reasoning effort. Choose another effort, or use the model default.".into());
            }
            step.write.push(json!({"jsonrpc":"2.0","id":6,"method":"session/set_config_option","params":{"sessionId":self.remote_id,"configId":"thinking","value":effort}}));
        } else {
            self.prompt_turn(step);
        }
        Ok(())
    }
    fn prompt_turn(&mut self, step: &mut Step) {
        let mut request = json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":self.remote_id}});
        request["params"]["prompt"] = Value::Array(self.prompt_content());
        step.write.push(request);
        self.phase = 1;
    }
}

async fn write_json(stdin: &mut ChildStdin, value: &Value) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    tokio::time::timeout(Duration::from_secs(5), async {
        stdin.write_all(&bytes).await?;
        stdin.flush().await
    })
    .await
    .map_err(|_| "Provider stopped reading input; send timed out".to_owned())?
    .map_err(|e| format!("Could not send to provider: {e}"))
}

struct StderrReader(tokio::task::JoinHandle<String>);
impl Drop for StderrReader {
    fn drop(&mut self) {
        // A child can inherit stderr and outlive the CLI. A timeout or aborted
        // run must not leave a detached task holding that pipe indefinitely.
        self.0.abort();
    }
}

async fn read_stderr(mut stderr: impl tokio::io::AsyncRead + Unpin) -> String {
    let mut tail = Vec::new();
    let mut buf = [0u8; 2048];
    while let Ok(n) = stderr.read(&mut buf).await {
        if n == 0 {
            break;
        }
        tail.extend_from_slice(&buf[..n]);
        if tail.len() > STDERR_LIMIT {
            tail.drain(..tail.len() - STDERR_LIMIT);
        }
    }
    String::from_utf8_lossy(&tail).trim().to_owned()
}

fn process_registry() -> &'static Mutex<HashSet<u32>> {
    static GROUPS: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();
    GROUPS.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(crate) fn kill_group(pid: u32) {
    if pid <= 1 {
        return;
    }
    #[cfg(unix)]
    unsafe {
        if libc::kill(pid as i32, 0) == 0 {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    {
        if windows_job::terminate(pid) {
            return;
        }
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .creation_flags(0x08000000)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Call immediately before forced application exit, which skips Rust destructors.
pub fn shutdown_all() {
    let owned: Vec<_> = process_registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .drain()
        .collect();
    for pid in owned {
        kill_group(pid);
    }
}

pub(crate) struct ProcessGroup(Option<u32>);
impl ProcessGroup {
    pub(crate) fn register(pid: Option<u32>) -> Self {
        if let Some(pid) = pid {
            #[cfg(windows)]
            windows_job::register(pid);
            process_registry()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(pid);
        }
        Self(pid)
    }
}
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            if process_registry()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&pid)
            {
                kill_group(pid);
            }
        }
    }
}

async fn finish_child(child: &mut Child, stdin: ChildStdin, group: &mut ProcessGroup) {
    drop(stdin);
    if tokio::time::timeout(Duration::from_millis(1000), child.wait())
        .await
        .is_err()
    {
        #[cfg(unix)]
        if let Some(pid) = group.0.filter(|pid| *pid > 1) {
            unsafe {
                if libc::kill(pid as i32, 0) == 0 {
                    libc::kill(-(pid as i32), libc::SIGTERM);
                }
            }
        }
        if tokio::time::timeout(Duration::from_millis(500), child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
        }
    }
    // Also clean up provider-created background processes, including orphaned children.
    let cleanup = ProcessGroup(group.0.take());
    drop(cleanup);
}

// Protocol fixture entry point.
#[cfg(test)]
#[allow(dead_code)]
pub async fn run(
    config: RunConfig,
    sink: Sink,
    input: UnboundedReceiver<RunInput>,
) -> Result<(), String> {
    run_with_tools(config, sink, input, None).await
}

pub async fn run_with_tools(
    config: RunConfig,
    sink: Sink,
    input: UnboundedReceiver<RunInput>,
    host: Option<ToolHost>,
) -> Result<(), String> {
    let identity = Protocol::new(config.clone());
    sink(identity.event("status", "Starting agent", None));
    let result = run_inner(config, sink.clone(), input, host).await;
    if let Err(error) = &result {
        sink(identity.event("error", error.clone(), None));
    }
    sink(identity.event(
        "done",
        if result.is_ok() { "Finished" } else { "Failed" },
        None,
    ));
    result
}

/// Credential paths the Claude-family CLI must never read or edit.
///
/// The desktop runs shell and file tools inside the provider harness, not in
/// Pantheon's own sandbox, so `pantheon_exec`'s credential masks (the TUI's
/// `hidden_paths`) do not apply here. Claude Code has an enforceable equivalent:
fn configure_command(command: &mut Command, config: &RunConfig) {
    // Pantheon speaks standard ACP on `serve acp` (stdio JSON-RPC). The task's
    // mode, model and effort travel over ACP (`session/set_mode`,
    // `session/set_model`, `session/set_config_option`), never argv, so a resumed
    // session cannot keep a stale policy.
    let _ = config;
    command.args(["serve", "acp"]);
}

async fn run_inner(
    mut config: RunConfig,
    sink: Sink,
    input: UnboundedReceiver<RunInput>,
    host: Option<ToolHost>,
) -> Result<(), String> {
    if !HARNESSES.contains(&config.provider.as_str()) {
        return Err("Unknown provider".into());
    }
    if !crate::storage::valid_permission_mode(&config.mode) {
        return Err("Unknown permission mode".into());
    }
    review_supported(&config.provider, &config.mode)?;
    validate_reasoning_effort(&config.provider, config.reasoning_effort.as_deref())?;
    // A cloud task runs the same pipeline through a bridge to its machine; local MCP host
    // tools stay on this computer (docs/cloud-agents.md, step 5).
    if let Some(command) = crate::cloud_agents::transport::command(&config, |command| {
        configure_command(command, &config)
    })
    .await?
    {
        return run_process_with_tools(config, sink, input, command, Vec::new(), None).await;
    }
    let connection = crate::connections::for_task(&config.session_id, &config.provider)
        .map(|connection| crate::connections::launch(&connection))
        .transpose()?;
    let path = connection
        .as_ref()
        .and_then(|connection| connection.executable.clone())
        .or_else(|| executable(&config.provider))
        .ok_or_else(|| format!("{} CLI is not installed or is not on PATH", config.provider))?;
    config.cwd = external_path(&config.cwd);
    let image_blocks = attachments::load(&config.provider, &config.images).await?;
    let mut command = command(&path)?;
    configure_command(&mut command, &config);
    // A named connection's arguments and environment win over the global setup.
    if let Some(connection) = &connection {
        connection.apply(&mut command);
    }
    run_process_with_tools(config, sink, input, command, image_blocks, host)
        .await
        .map_err(|error| {
            connection
                .as_ref()
                .map_or(error.clone(), |connection| connection.redact(&error))
        })
}

fn optional_metadata_tool(name: &str) -> bool {
    name == crate::session_titles::TOOL || name == crate::turn_changes::TRACK_TOOL
}
// Preview tools are an optional transport capability, but retain their normal
// approval requirements and transcript activity. They are not metadata tools.
fn optional_transport_tool(name: &str) -> bool {
    optional_metadata_tool(name) || matches!(name, "pantheon_run_project" | "pantheon_preview_file")
}
fn per_run_mcp_tool(name: &str) -> bool {
    name.starts_with("pantheon_jev_") || optional_transport_tool(name)
}

async fn run_process_with_tools(
    config: RunConfig,
    sink: Sink,
    mut input: UnboundedReceiver<RunInput>,
    mut command: Command,
    image_blocks: Vec<Value>,
    host: Option<ToolHost>,
) -> Result<(), String> {
    let mcp_tool_names: HashSet<String> = {
        host.as_ref()
            .into_iter()
            .flat_map(|host| host.tools.iter())
            .filter_map(|spec| spec["name"].as_str())
            .filter(|name| per_run_mcp_tool(name))
            .flat_map(|name| [name.into(), format!("mcp__pantheon_architect__{name}")])
            .collect()
    };
    let mcp = match host {
        Some(host) => {
            mcp_host::Server::start_for_run(
                host,
                config.session_id.clone(),
                sink.clone(),
                &config.mode,
            )
            .await?
        }
        None => None,
    };
    let (mut child, mut group) = {
        command
            .current_dir(&config.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        let child = command
            .spawn()
            .map_err(|e| format!("Could not start {}: {e}", config.provider))?;
        let group = ProcessGroup::register(child.id());
        (child, group)
    };
    let mut stdin = child.stdin.take().ok_or("Provider stdin unavailable")?;
    let mut stdout = child.stdout.take().ok_or("Provider stdout unavailable")?;
    let stderr = child.stderr.take().ok_or("Provider stderr unavailable")?;
    let mut stderr_task = StderrReader(tokio::spawn(read_stderr(stderr)));
    let mut protocol = Protocol::new(config);
    protocol.image_blocks = image_blocks;
    protocol.mcp_server = mcp.as_ref().map(mcp_host::Server::acp_config);
    protocol.mcp_required = mcp.as_ref().is_some_and(mcp_host::Server::required);
    // An optional bridge can fail to bind. In that case no local MCP tool is
    // registered, so a same-named user server must retain normal approvals.
    protocol.mcp_tool_names = if mcp.is_some() {
        mcp_tool_names
    } else {
        HashSet::new()
    };
    let mut bytes = Vec::new();
    let mut buf = [0u8; 8192];
    let mut stopped = false;
    let mut input_open = true;
    let mut deadline = tokio::time::Instant::now() + Duration::from_secs(90);
    let result=async {
        write_json(&mut stdin,&protocol.initial()).await?;
        loop {
            tokio::select! {
                action=input.recv(),if input_open && !stopped=>{
                    match action {
                        Some(RunInput::Steer{id,text,reply})=>{let step=protocol.steer(id,text,reply);apply_step(&mut protocol,step,&sink,&mut stdin).await?;},
                        Some(RunInput::Approval{request_id,approved})=>{if let Some(server) = &mcp { server.approve(&request_id,approved); }},
                        Some(RunInput::UserInput{request_id,response,reply})=>{
                            match protocol.user_input(&request_id,response) {
                                Ok(step)=>{let result=apply_step(&mut protocol,step,&sink,&mut stdin).await.map(|_|());if let Some(reply)=reply {let _=reply.send(result.clone());}result?;},
                                Err(error)=>{if let Some(reply)=reply {let _=reply.send(Err(error));}},
                            }
                        },
                        Some(RunInput::Stop)|None=>{
                            if let Some(server) = &mcp { server.stop(); }
                            input_open=false;stopped=true;sink(protocol.event("status","Stopping agent",None));
                                    let step=protocol.cancel_requests();
                            let _=tokio::time::timeout(Duration::from_millis(500),apply_step(&mut protocol,step,&sink,&mut stdin)).await;
                            let _=tokio::time::timeout(Duration::from_millis(500),write_json(&mut stdin,&protocol.cancel())).await;deadline=tokio::time::Instant::now()+Duration::from_secs(3);
                        },
                    }
                },
                                _=tokio::time::sleep_until(deadline),if stopped || protocol.phase==0=>{
                    if stopped{break Ok(());}break Err("Agent initialization timed out. Check the CLI's login and configuration in a terminal.".into());
                },
                read=stdout.read(&mut buf)=>{
                    let count=read.map_err(|e|format!("Provider stream failed: {e}"))?;
                    if count==0 {if stopped{break Ok(());}break Err("Agent exited before completing its response".into());}
                    // Retained bytes contain no newline: only scan this read.
                    // Rescanning the entire partial frame makes large tool
                    // results quadratic and can starve other concurrent runs.
                    let previous_len=bytes.len();
                    bytes.extend_from_slice(&buf[..count]);
                    let mut consumed=0;
                    for end in bytes.iter().enumerate().skip(previous_len).filter_map(|(i,b)|if *b==b'\n'{Some(i)}else{None}).collect::<Vec<_>>() {
                        let line=&bytes[consumed..end];consumed=end+1;
                        if line.len()>MAX_FRAME{return Err("Provider sent an oversized JSON message".into());}
                        if line.iter().all(u8::is_ascii_whitespace){continue;}
                        let value:Value=serde_json::from_slice(line).map_err(|e|format!("Invalid provider JSON: {e}"))?;
                        if stopped {
                            continue;
                        }
                        let step=protocol.receive(value)?;
                        if apply_step(&mut protocol,step,&sink,&mut stdin).await? {return Ok(());}
                    }
                    bytes.drain(..consumed);
                    if bytes.len()>MAX_FRAME{return Err("Provider output exceeded the message size limit".into());}
                },
            }
        }
    }.await;
    if let Some(server) = &mcp {
        server.stop();
    }
    for event in protocol.steers_ended(result.is_ok() && !stopped) {
        sink(event);
    }
    for event in protocol.cancel_requests().events {
        sink(event);
    }
    finish_child(&mut child, stdin, &mut group).await;
    let diagnostic = tokio::time::timeout(Duration::from_millis(500), &mut stderr_task.0)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    let diagnostic = mcp
        .as_ref()
        .map(|server| server.redact(&diagnostic))
        .unwrap_or(diagnostic);
    result.map_err(|error: String| {
        let error = mcp
            .as_ref()
            .map(|server| server.redact(&error))
            .unwrap_or(error);
        diagnostics::failure(&error, &diagnostic)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn config(provider: &str) -> RunConfig {
        RunConfig {
            session_id: "local".into(),
            provider: provider.into(),
            model: "default".into(),
            reasoning_effort: None,
            mode: "ask".into(),
            cwd: PathBuf::from("/tmp"),
            resume_id: None,
            prompt: "Explain this project".into(),
            instructions: String::new(),
            skill: None,
            images: Vec::new(),
            goal_objective: None,
        }
    }

    #[test]
    fn preview_tools_do_not_block_normal_acp_turns_when_http_mcp_is_unavailable() {
        let mut p = Protocol::new(config("pantheon"));
        p.mcp_server = Some(json!({"type":"http","name":"pantheon_architect"}));
        p.mcp_required = ["pantheon_run_project", "pantheon_preview_file"]
            .iter()
            .any(|name| !optional_transport_tool(name));
        let step = p.receive(json!({"id":1,"result":{"protocolVersion":1,"agentCapabilities":{"mcpCapabilities":{"http":false}},"authMethods":[{"id":"cached_token","name":"cached_token"}],"_meta":{"defaultAuthMethodId":"cached_token"}}})).unwrap();
        assert_eq!(step.write[0]["method"], "session/new");
        assert_eq!(step.write[0]["params"]["mcpServers"], json!([]));
    }

    #[test]
    fn review_mode_is_refused_and_the_policy_modes_are_accepted() {
        assert!(review_supported("pantheon", "review")
            .unwrap_err()
            .contains("no agent-reviewed approvals"));
        for mode in ["ask", "auto", "plan", "full"] {
            assert!(review_supported("pantheon", mode).is_ok(), "{mode}");
        }
    }

    /// Audit DESKTOP-1: the desktop's harness runs outside `pantheon_exec`, so
    /// the TUI's credential masks do not apply. Every Claude-family launch must
    /// carry deny rules for the credential paths, in every mode, and the payload
    /// must be valid JSON in the shape Claude Code documents.

    #[test]
    fn screenshot_only_prompts_do_not_include_invalid_empty_text_blocks() {
        let mut cfg = config("pantheon");
        cfg.prompt.clear();
        let mut p = Protocol::new(cfg);
        p.image_blocks =
            vec![json!({"type":"image","mimeType":"image/png","data":"PRIVATE_IMAGE"})];
        let step = p.receive(json!({"id":4,"result":{}})).unwrap();
        let content = &step.write[0]["params"]["prompt"];
        assert_eq!(content.as_array().unwrap().len(), 1);
        assert_ne!(content[0]["type"], "text");
    }

    #[tokio::test]
    async fn model_listing_rejects_unknown_provider_without_starting_a_process() {
        assert_eq!(list_models("shell").await.unwrap_err(), "Unknown provider");
    }

    #[test]
    fn every_registered_provider_has_a_model_discovery_slot() {
        assert_eq!(model_discovery_semaphores().len(), PROVIDERS.len());
        for (index, (id, _)) in PROVIDERS.iter().enumerate() {
            assert_eq!(model_discovery_index(id), Some(index));
            assert!(index < model_discovery_semaphores().len());
        }
        assert!(PROVIDERS.iter().any(|(id, _)| *id == "pantheon"));
        assert_eq!(model_discovery_index("not-a-provider"), None);
    }

    #[test]
    fn every_advertised_provider_passes_the_launch_whitelist() {
        for (id, _) in PROVIDERS {
            assert!(
                HARNESSES.contains(&id),
                "{id} is advertised but run_inner refuses to launch it"
            );
        }
        assert!(HARNESSES.contains(&"pantheon"));
    }

    #[test]
    fn pantheon_catalog_reports_provider_model_ids_without_duplicates() {
        let models = parse_pantheon_catalog(
            r#"
    cataloged providers:
      * OpenAI (openai): OpenAI -- https://api.openai.com/v1
        models: gpt-4o, gpt-4o-mini
      * (Google Vertex AI only) (vertex): OpenAI -- https://{location}-aiplatform.googleapis.com/v1
        models:
        tag: template
      * Zhipu AI / GLM (zhipu): OpenAI -- https://open.bigmodel.cn/api/paas/v4
        models: glm-4-flash, glm-5.3-flash, glm-4-flash
    "#,
        );
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "default",
                "openai/gpt-4o",
                "openai/gpt-4o-mini",
                "zhipu/glm-4-flash",
                "zhipu/glm-5.3-flash"
            ]
        );
        assert_eq!(models[1].name, "gpt-4o");
        assert_eq!(models[1].description, "Provided by openai");
    }

    /// Read-only registry query; no model inference.
    #[tokio::test]
    #[ignore]
    async fn installed_pantheon_catalog() {
        let path = executable("pantheon").expect("Build the Pantheon CLI to check its catalog");
        let models = pantheon_models(&path).await.unwrap();
        assert!(models.len() > 1);
        for model in models {
            println!("{}: {}", model.id, model.name);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn forced_shutdown_terminates_owned_provider_group() {
        let _serial = concurrent_process_tests::process_test_lock().lock().await;
        let mut child = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let group = ProcessGroup::register(child.id());
        shutdown_all();
        let status = tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(!status.success());
        assert!(!process_registry()
            .lock()
            .unwrap()
            .contains(&group.0.unwrap()));
        drop(group);
    }

    /// Explicitly opt in: this runs two small billable/subscription requests.
    /// PANTHEON_SMOKE_PROVIDER=codex cargo test smoke_provider_from_env -- --ignored --nocapture
    #[tokio::test]
    #[ignore]
    async fn smoke_provider_from_env() {
        use std::sync::Mutex;
        let provider = std::env::var("PANTHEON_SMOKE_PROVIDER")
            .expect("Set PANTHEON_SMOKE_PROVIDER to claude, codex, kimi, muse or pantheon");
        let model = std::env::var("PANTHEON_SMOKE_MODEL").unwrap_or_else(|_| "default".into());
        assert!(["pantheon", "pantheon", "pantheon", "pantheon", "pantheon"]
            .contains(&provider.as_str()));
        let root =
            std::env::temp_dir().join(format!("pantheon-provider-smoke-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        assert!(std::process::Command::new("git")
            .arg("init")
            .arg("--quiet")
            .arg(&root)
            .status()
            .unwrap()
            .success());
        let mut resume = None;
        for turn in 0..2 {
            let observed = Arc::new(Mutex::new((
                String::new(),
                None::<String>,
                Vec::<String>::new(),
            )));
            let captured = observed.clone();
            let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
            let approval_sender = sender.clone();
            let sink: Sink = Arc::new(move |event| {
                let mut data = captured.lock().unwrap();
                data.2.push(event.kind.clone());
                if event.kind == "text" {
                    data.0.push_str(&event.text);
                }
                if event.kind == "session" {
                    data.1 = Some(event.text);
                }
                if event.kind == "approval" {
                    if let Some(request_id) = event.request_id {
                        let _ = approval_sender.send(RunInput::Approval {
                            request_id,
                            approved: false,
                        });
                    }
                }
            });
            let prompt = if turn == 0 {
                "Reply with exactly PANTHEON_OK. Do not use tools or modify files."
            } else {
                "What exact token did you reply in your preceding answer? Reply only with that token. Do not use tools or modify files."
            };
            let cfg = RunConfig {
                session_id: format!("smoke-{provider}"),
                provider: provider.clone(),
                model: model.clone(),
                reasoning_effort: None,
                mode: "ask".into(),
                cwd: root.clone(),
                resume_id: resume.clone(),
                prompt: prompt.into(),
                instructions: String::new(),
                skill: None,
                images: Vec::new(),
                goal_objective: None,
            };
            let mut task = tokio::spawn(run(cfg, sink, receiver));
            let outcome = match tokio::time::timeout(Duration::from_secs(90), &mut task).await {
                Ok(result) => result.expect("Provider task panicked"),
                Err(_) => {
                    let _ = sender.send(RunInput::Stop);
                    let _ = tokio::time::timeout(Duration::from_secs(10), &mut task).await;
                    task.abort();
                    panic!("Provider smoke timed out");
                }
            };
            outcome.unwrap_or_else(|e| panic!("{provider} failed: {e}"));
            let data = observed.lock().unwrap();
            assert!(
                data.0.contains("PANTHEON_OK"),
                "Expected smoke response, received {} characters",
                data.0.chars().count()
            );
            let remote = data.1.clone().expect("No provider session ID was emitted");
            if let Some(previous) = resume.as_ref() {
                assert_eq!(
                    previous, &remote,
                    "Resume created a different provider session"
                );
            }
            resume = Some(remote);
            println!(
                "{provider} turn {}: PANTHEON_OK; {} events; resume ID present",
                turn + 1,
                data.2.len()
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
