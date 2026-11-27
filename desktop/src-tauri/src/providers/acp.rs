//! ACP v1 transport for Pantheon.
//!
//! `pantheon serve acp` is the only agent this desktop launches. The transport is
//! the protocol's own: `initialize`, `session/new` or `session/resume`, mode,
//! model and reasoning selection, `session/prompt`, streaming updates, and
//! `session/request_permission` for the approval bar.
use super::*;
use crate::providers::pipe::Pipe;

const DISCOVERY_STARTUP: Duration = Duration::from_secs(60);
const COMMAND_GRACE: Duration = Duration::from_secs(1);
pub(crate) const ACP_STARTUP_TIMEOUT: &str =
    "Pantheon's ACP server did not start within 60 seconds. Run `pantheon doctor` in a terminal, then refresh.";

fn rpc_error(frame: &Value, stage: &str) -> String {
    let message = model_text(
        frame["error"]["message"].as_str(),
        "ACP request failed",
        1000,
    );
    let data = &frame["error"]["data"];
    let detail = data
        .as_str()
        .or_else(|| data["message"].as_str())
        .or_else(|| data["details"].as_str())
        .unwrap_or("");
    super::diagnostics::failure(&format!("ACP {stage}: {message}"), detail)
}

fn transport_error(pipe: &Pipe, stage: &str) -> String {
    if pipe.timed_out() {
        ACP_STARTUP_TIMEOUT.into()
    } else {
        format!("Pantheon's ACP server ended during {stage}. Check `pantheon doctor`, then retry.")
    }
}

fn options<'a>(config: &'a Value, category: &str) -> Option<&'a Value> {
    config["configOptions"]
        .as_array()?
        .iter()
        .find(|option| option["category"] == category)
}

fn choices(option: &Value) -> Vec<&Value> {
    option["options"]
        .as_array()
        .map(|values| values.iter().collect())
        .unwrap_or_default()
}

fn choice_request(session: &str, id: u64, option: &Value, value: &str) -> Result<Value, String> {
    let advertised = choices(option)
        .iter()
        .any(|choice| choice["value"] == value);
    if !advertised {
        return Err(format!(
            "Pantheon does not advertise {} here. Choose another value.",
            category_label(option)
        ));
    }
    Ok(
        json!({"jsonrpc":"2.0","id":id,"method":"session/set_config_option","params":{"sessionId":session,"configId":option["id"],"value":value}}),
    )
}

fn category_label(option: &Value) -> &'static str {
    match option["category"].as_str() {
        Some("mode") => "this task mode",
        Some("model") => "this model",
        Some("thought_level") | Some("effort") => "this reasoning level",
        _ => "this setting",
    }
}

impl Protocol {
    pub(super) fn acp(&mut self, value: Value) -> Result<Step, String> {
        let mut step = Step::default();
        if self.acp_live_answered(&value, &mut step) {
            return Ok(step);
        }
        if value.get("error").is_some() {
            if value["error"]["code"] == -32000 {
                return Err("Pantheon's provider is not signed in. Run `pantheon login` in a terminal, then retry.".into());
            }
            let stage = match value["id"].as_u64() {
                Some(1) => "initialize",
                Some(2) => "session creation or resume",
                Some(3) => "session/prompt",
                Some(4) => "mode selection",
                Some(5) => "model selection",
                Some(6) => "reasoning selection",
                _ => "request",
            };
            return Err(rpc_error(&value, stage));
        }
        if let Some(id) = value["id"]
            .as_u64()
            .filter(|_| value.get("result").is_some())
        {
            let result = &value["result"];
            match id {
                1 => {
                    if result["protocolVersion"] != 1 {
                        return Err("Pantheon's ACP server does not speak protocol v1. Run `pantheon update`.".into());
                    }
                    let capabilities = &result["agentCapabilities"];
                    if !self.image_blocks.is_empty()
                        && capabilities["promptCapabilities"]["image"] != true
                    {
                        return Err("This Pantheon build does not accept images. Remove them and describe what they show.".into());
                    }
                    if self.mcp_server.is_some() && capabilities["mcpCapabilities"]["http"] != true
                    {
                        if self.mcp_required {
                            return Err("This Pantheon build does not advertise HTTP MCP support, which its own tools need. Run `pantheon update`.".into());
                        }
                        // Title suggestions are optional metadata; the local
                        // fallback covers an older build.
                        self.mcp_server = None;
                    }
                    let mut params = json!({"cwd":launch::external_path(&self.config.cwd),"mcpServers":self.mcp_server.iter().collect::<Vec<_>>()});
                    let method = if self.remote_id.is_empty() {
                        "session/new"
                    } else {
                        params["sessionId"] = json!(self.remote_id);
                        if capabilities["sessionCapabilities"].get("resume").is_some() {
                            "session/resume"
                        } else if capabilities["loadSession"] == true {
                            "session/load"
                        } else {
                            return Err("This Pantheon build cannot resume saved tasks. Update it or start a new task.".into());
                        }
                    };
                    step.write
                        .push(json!({"jsonrpc":"2.0","id":2,"method":method,"params":params}));
                }
                2 => {
                    let returned = field(result, "sessionId");
                    if !returned.is_empty() && returned.len() <= 256 {
                        self.remote_id = returned.into();
                    }
                    if self.remote_id.is_empty() {
                        return Err("Pantheon's ACP server did not return a session ID".into());
                    }
                    self.acp_config = result.clone();
                    if let Some(bytes) = self.early_advertised_commands.take() {
                        if let Ok(frame) = serde_json::from_slice::<Value>(&bytes) {
                            if frame["params"]["sessionId"] == self.remote_id {
                                self.commands_changed(&frame, &mut step);
                            }
                        }
                    }
                    step.events
                        .push(self.event("session", self.remote_id.clone(), None));
                    self.acp_mode(&mut step)?;
                }
                4 => {
                    self.acp_update_config(result);
                    self.acp_model(&mut step)?;
                }
                5 => {
                    self.acp_update_config(result);
                    self.acp_effort(&mut step)?;
                }
                6 => self.prompt_turn(&mut step),
                3 => self.acp_turn_end(result, &mut step),
                _ => {}
            }
            self.acp_startup_status(&mut step);
            return Ok(step);
        }
        if value["method"] == "session/update"
            && value["params"]["sessionId"] == self.remote_id
            && value["params"]["update"]["sessionUpdate"] == "config_option_update"
        {
            self.acp_update_config(&value["params"]["update"]);
        }
        if value["method"] == "session/request_permission" {
            if value["params"]["sessionId"]
                .as_str()
                .is_some_and(|id| id != self.remote_id)
            {
                step.write.push(json!({"jsonrpc":"2.0","id":value["id"],"result":{"outcome":{"outcome":"cancelled"}}}));
                return Ok(step);
            }
            // Pantheon's own policy is the gate: its permission requests reach the
            // approval bar unless the selected mode says otherwise. Plan mode runs
            // read-only inside the agent, so it does not need to answer here; Full
            // access answers single-grant requests and never a structured question.
            let one_allow = value["params"]["options"].as_array().is_some_and(|values| {
                values
                    .iter()
                    .filter(|entry| entry["kind"] == "allow_once")
                    .count()
                    == 1
            });
            let structured_question =
                value["params"]["options"]
                    .as_array()
                    .is_some_and(|options| {
                        options.iter().any(|option| {
                            field(option, "optionId")
                                .strip_prefix('q')
                                .and_then(|id| id.split_once("_opt_"))
                                .is_some_and(|(number, _)| number.parse::<usize>().is_ok())
                        })
                    });
            let edit = value["params"]["toolCall"]["kind"] == "edit";
            let auto = self.config.mode == "full" || (self.config.mode == "auto" && edit);
            if auto && one_allow && !structured_question {
                let selected = value["params"]["options"]
                    .as_array()
                    .and_then(|values| values.iter().find(|entry| entry["kind"] == "allow_once"))
                    .cloned();
                if let Some(selected) = selected {
                    step.write.push(json!({"jsonrpc":"2.0","id":value["id"],"result":{"outcome":{"outcome":"selected","optionId":selected["optionId"]}}}));
                    return Ok(step);
                }
            }
        }
        let mut inner = self.stream(value)?;
        if step.events.is_empty() && step.write.is_empty() {
            return Ok(inner);
        }
        step.events.append(&mut inner.events);
        step.write.append(&mut inner.write);
        step.done |= inner.done;
        step.failure = step.failure.or(inner.failure);
        Ok(step)
    }

    fn acp_update_config(&mut self, result: &Value) {
        if result["configOptions"].is_array() {
            self.acp_config["configOptions"] = result["configOptions"].clone();
        }
    }

    fn acp_startup_status(&self, step: &mut Step) {
        let label = step
            .write
            .iter()
            .find_map(|request| match field(request, "method") {
                "session/new" => Some("Opening session"),
                "session/load" | "session/resume" => Some("Restoring session"),
                "session/set_mode" => Some("Applying task mode"),
                "session/set_model" => Some("Applying model settings"),
                "session/set_config_option" => Some(match request["id"].as_u64() {
                    Some(4) => "Applying task mode",
                    Some(6) => "Applying reasoning settings",
                    _ => "Applying model settings",
                }),
                "session/prompt" => Some("Waiting for response"),
                _ => None,
            });
        if let Some(label) = label {
            step.events.push(self.event("status", label, None));
        }
    }

    /// Build keeps Pantheon's normal capability gate (requests surface in the
    /// approval bar); Plan is its host-enforced read-only posture.
    fn acp_mode(&mut self, step: &mut Step) -> Result<(), String> {
        let wanted = if self.config.mode == "plan" {
            "plan"
        } else {
            "build"
        };
        if let Some(option) = options(&self.acp_config, "mode") {
            if choices(option)
                .iter()
                .any(|entry| field(entry, "value") == wanted)
            {
                step.write
                    .push(choice_request(&self.remote_id, 4, option, wanted)?);
                return Ok(());
            }
            return Err("Pantheon does not advertise the selected task mode. Run `pantheon update`, then refresh.".into());
        }
        if let Some(modes) = self.acp_config["modes"]["availableModes"].as_array() {
            if let Some(mode) = modes.iter().find(|mode| mode["id"] == wanted) {
                step.write.push(json!({"jsonrpc":"2.0","id":4,"method":"session/set_mode","params":{"sessionId":self.remote_id,"modeId":mode["id"]}}));
                return Ok(());
            }
            return Err("Pantheon does not advertise the selected task mode. Run `pantheon update`, then refresh.".into());
        }
        // No mode surface at all: the agent has one posture, so nothing to select.
        self.acp_model(step)
    }

    fn acp_model(&mut self, step: &mut Step) -> Result<(), String> {
        if self.config.model.is_empty() || self.config.model == "default" {
            return self.acp_effort(step);
        }
        if let Some(option) = options(&self.acp_config, "model") {
            step.write.push(choice_request(
                &self.remote_id,
                5,
                option,
                &self.config.model,
            )?);
        } else {
            let advertised = self.acp_config["models"]["availableModels"]
                .as_array()
                .is_some_and(|models| {
                    models
                        .iter()
                        .any(|model| model["modelId"] == self.config.model)
                });
            if !advertised {
                return Err(
                    "Pantheon does not advertise the selected model. Refresh the model list."
                        .into(),
                );
            }
            step.write.push(json!({"jsonrpc":"2.0","id":5,"method":"session/set_model","params":{"sessionId":self.remote_id,"modelId":self.config.model}}));
        }
        Ok(())
    }

    fn acp_effort(&mut self, step: &mut Step) -> Result<(), String> {
        if let Some(effort) = &self.config.reasoning_effort {
            let option = options(&self.acp_config, "thought_level")
                .or_else(|| options(&self.acp_config, "effort"));
            match option {
                Some(option) => step.write.push(choice_request(&self.remote_id, 6, option, effort)?),
                None => return Err("This model does not advertise an adjustable reasoning effort. Choose a different effort.".into()),
            }
        } else {
            self.prompt_turn(step);
        }
        Ok(())
    }
}

/// Send `initialize` alone and return its result; the process is then stopped.
pub(crate) async fn inspect_with(
    command: Command,
    cwd: &Path,
    wait_commands: bool,
) -> Result<(Value, Option<Value>), String> {
    inspect_state(
        command,
        cwd,
        wait_commands,
        DISCOVERY_STARTUP,
        COMMAND_GRACE,
    )
    .await
    .map_err(|failure| failure.message)
}

#[derive(Debug)]
struct DiscoveryFailure {
    acp_ready: bool,
    authentication_required: bool,
    message: String,
}

async fn inspect_state(
    command: Command,
    cwd: &Path,
    wait_commands: bool,
    startup_timeout: Duration,
    command_grace: Duration,
) -> Result<(Value, Option<Value>), DiscoveryFailure> {
    let mut pipe = Pipe::spawn(command, cwd, startup_timeout).map_err(|_| DiscoveryFailure {
        acp_ready: false,
        authentication_required: false,
        message: "Could not start Pantheon's ACP server. Choose its executable in Settings.".into(),
    })?;
    let mut acp_ready = false;
    let mut authentication_required = false;
    let mut stage = "initialize";
    let result: Result<(Value, Option<Value>), String> = async {
        pipe.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{},"clientInfo":{"name":"pantheon","version":"0.1.0"}}})).await.map_err(|_| transport_error(&pipe, stage))?;
        let mut metadata = None;
        let mut commands = None;
        let session = json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":launch::external_path(cwd),"mcpServers":[]}});
        loop {
            let frame = match pipe.read().await {
                Ok(frame) => frame,
                // A command notification is optional. Once session creation
                // succeeded, neither silence nor exit can erase its models.
                Err(()) if metadata.is_some() => return Ok((metadata.unwrap(), commands)),
                Err(()) => return Err(transport_error(&pipe, stage)),
            };
            if frame.get("method").is_some() && frame.get("id").is_some() {
                pipe.send(&json!({"jsonrpc":"2.0","id":frame["id"],"error":{"code":-32601,"message":"Metadata discovery does not execute requests"}})).await.map_err(|_| transport_error(&pipe, stage))?;
            } else if frame["id"] == 1 || frame["id"] == 2 {
                if frame.get("error").is_some() {
                    authentication_required = frame["error"]["code"] == -32000;
                    let error = rpc_error(&frame, stage);
                    return Err(if authentication_required {
                        format!("Pantheon's provider is not signed in. Run `pantheon login`, then check again.\n{error}")
                    } else {
                        error
                    });
                }
                if frame["id"] == 1 {
                    if frame["result"]["protocolVersion"] != 1 {
                        return Err("Pantheon's ACP server does not speak protocol v1.".into());
                    }
                    acp_ready = true;
                    stage = "session/new";
                    pipe.send(&session).await.map_err(|_| transport_error(&pipe, stage))?;
                } else {
                    if !frame["result"]["sessionId"].as_str().is_some_and(|id| !id.is_empty() && id.len() <= 256) {
                        return Err("ACP session/new did not return a valid session ID.".into());
                    }
                    metadata = Some(frame["result"].clone());
                    pipe.set_deadline_after(command_grace);
                }
            } else if frame["method"] == "session/update"
                && frame["params"]["update"]["sessionUpdate"] == "available_commands_update"
            {
                commands = Some(frame);
            }
            if let Some(metadata) = &metadata {
                if commands.as_ref().is_some_and(|frame| frame["params"]["sessionId"] != metadata["sessionId"]) {
                    commands = None;
                }
                if !wait_commands || commands.is_some() {
                    return Ok((metadata.clone(), commands));
                }
            }
        }
    }
    .await;
    pipe.finish().await;
    result.map_err(|error| DiscoveryFailure {
        acp_ready,
        authentication_required,
        message: super::diagnostics::failure(&error, pipe.diagnostic()),
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionStatus {
    installed: bool,
    acp_ready: bool,
    session_ready: bool,
    state: &'static str,
    message: String,
}

fn connection_status(result: Result<(Value, Option<Value>), DiscoveryFailure>) -> ConnectionStatus {
    match result {
        Ok(_) => ConnectionStatus {
            installed: true,
            acp_ready: true,
            session_ready: true,
            state: "ready",
            message: "Pantheon's ACP server initialized and opened a session. No prompt was sent; model access is checked when you send.".into(),
        },
        Err(failure) => ConnectionStatus {
            installed: true,
            acp_ready: failure.acp_ready,
            session_ready: false,
            state: if failure.authentication_required {
                "authenticationRequired"
            } else if failure.acp_ready {
                "sessionFailed"
            } else {
                "startupFailed"
            },
            message: failure.message,
        },
    }
}

/// Explicit Settings check. Never sends a prompt or accepts an agent request.
#[tauri::command]
pub async fn provider_check_connection(provider: String) -> Result<ConnectionStatus, String> {
    if provider != "pantheon" {
        return Err("Pantheon is the only supported provider.".into());
    }
    let Some(path) = super::executable(&provider).filter(|path| path.is_file()) else {
        return Ok(ConnectionStatus {
            installed: false,
            acp_ready: false,
            session_ready: false,
            state: "missing",
            message: "Select the installed Pantheon executable in Settings.".into(),
        });
    };
    let command = match super::command(&path) {
        Ok(command) => command,
        Err(message) => {
            return Ok(connection_status(Err(DiscoveryFailure {
                acp_ready: false,
                authentication_required: false,
                message,
            })))
        }
    };
    let mut command = command;
    command.args(["serve", "acp"]);
    let result = inspect_state(
        command,
        &std::env::temp_dir(),
        false,
        DISCOVERY_STARTUP,
        COMMAND_GRACE,
    )
    .await;
    Ok(connection_status(result))
}

const MAX_STEER_BYTES: usize = 128 * 1024;
const MAX_STEERS: usize = 16;
/// Request ids for live steering prompts, clear of startup (1-7), cancel (900) and goals (10000+).
const STEER_IDS: std::ops::Range<u64> = 2000..9000;

/// Steering for ACP runs. A second `session/prompt` can fold into the running
/// A steer sent into an open turn is `live`; one that arrives before or between
/// turns is `held` and goes out as the next `session/prompt` on the same session.
#[derive(Default)]
pub(super) struct Steers {
    pub(super) held: Vec<(String, String)>,
    pub(super) live: Vec<(u64, String, String)>,
    next: u64,
    /// The opening prompt answered while live steers were still outstanding.
    pub(super) turn_over: bool,
}

impl Protocol {
    pub(super) fn steer_event(&self, id: &str, text: &str, status: &str, mode: &str) -> AgentEvent {
        // `event()` keeps only generic metadata keys; a steer needs its mode too.
        let mut event = self.event("steer", text, None);
        event.data = Some(json!({"id":id,"status":status,"mode":mode}));
        event
    }

    pub(super) fn acp_steer(
        &mut self,
        id: String,
        text: String,
        reply: super::goals::RpcReply,
    ) -> Step {
        let mut step = Step::default();
        let text = text.trim().to_owned();
        if text.is_empty() || text.len() > MAX_STEER_BYTES {
            let _ = reply.send(Err("Write a steering message under 128 KiB.".into()));
            return step;
        }
        let steers = &mut self.acp_steers;
        if steers.held.len() + steers.live.len() >= MAX_STEERS {
            let _ = reply.send(Err(
                "Too many steering messages are waiting; the message stays queued.".into(),
            ));
            return step;
        }
        // A steer rides the open turn; before or between turns it becomes the next one.
        let live = self.phase == 1 && !steers.turn_over && !self.remote_id.is_empty();
        let mode = if live { "live" } else { "next-turn" };
        step.events
            .push(self.steer_event(&id, &text, "pending", mode));
        if live {
            let steers = &mut self.acp_steers;
            let request = STEER_IDS.start + steers.next % (STEER_IDS.end - STEER_IDS.start);
            steers.next += 1;
            step.write.push(json!({"jsonrpc":"2.0","id":request,"method":"session/prompt","params":{"sessionId":self.remote_id,"prompt":[{"type":"text","text":text}]}}));
            step.events
                .push(self.steer_event(&id, &text, "delivered", mode));
            self.acp_steers.live.push((request, id.clone(), text));
        } else {
            self.acp_steers.held.push((id.clone(), text));
        }
        let _ = reply.send(Ok(json!({"steered":true,"id":id,"mode":mode})));
        step
    }

    /// Steers that will never reach the agent (stop, error, process exit).
    pub(super) fn acp_steers_failed(&mut self) -> Step {
        let mut step = Step::default();
        for (id, text) in std::mem::take(&mut self.acp_steers.held) {
            step.events
                .push(self.steer_event(&id, &text, "failed", "next-turn"));
        }
        self.acp_steers.live.clear();
        step
    }

    /// The running `session/prompt` answered: send held steers as the next turn, or finish.
    fn acp_turn_end(&mut self, result: &Value, step: &mut Step) {
        if result["stopReason"] == "cancelled" {
            let mut failed = self.acp_steers_failed();
            step.events.append(&mut failed.events);
            step.done = true;
            return;
        }
        // A non-end_turn stop is a state the user must see (turn-terminal
        // transitions flush immediately): budget stops and refusals otherwise
        // render as a silent turn end.
        if !self.acp_steers.turn_over {
            if let Some(text) = acp_stop_notice(result["stopReason"].as_str().unwrap_or_default()) {
                step.events.push(self.event("status", text, None));
            }
        }
        if !self.acp_steers.live.is_empty() {
            self.acp_steers.turn_over = true;
            return;
        }
        self.acp_steers.turn_over = false;
        let held = std::mem::take(&mut self.acp_steers.held);
        if held.is_empty() {
            step.done = true;
            return;
        }
        self.config.prompt = held
            .iter()
            .map(|(_, text)| text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        self.prompt_turn(step);
        for (id, text) in held {
            step.events
                .push(self.steer_event(&id, &text, "delivered", "next-turn"));
        }
    }

    /// A live steer request answered. Returns false when the id is not one.
    fn acp_live_answered(&mut self, value: &Value, step: &mut Step) -> bool {
        let Some(index) = value["id"].as_u64().and_then(|id| {
            self.acp_steers
                .live
                .iter()
                .position(|(request, ..)| *request == id)
        }) else {
            return false;
        };
        let (_, id, text) = self.acp_steers.live.remove(index);
        if value.get("error").is_some() {
            // Refused mid-turn: carry it into the next turn instead of failing the run.
            self.acp_steers.held.push((id, text));
        }
        if self.acp_steers.turn_over && self.acp_steers.live.is_empty() {
            self.acp_turn_end(&value["result"], step);
        }
        true
    }
}

/// Human notice for a non-end_turn ACP stop reason; `None` for normal ends.
fn acp_stop_notice(reason: &str) -> Option<String> {
    match reason {
        "" | "end_turn" | "cancelled" => None,
        "max_tokens" => Some("Paused: the model hit its output limit for this turn.".into()),
        "max_turn_requests" => {
            Some("Paused: the run reached its request limit for this turn.".into())
        }
        "refusal" => Some("The agent declined to continue (refusal).".into()),
        other => Some(format!("The agent stopped ({other}).")),
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::config;
    use super::*;

    fn capabilities() -> Value {
        json!({"protocolVersion":1,"agentCapabilities":{"promptCapabilities":{"image":true},"mcpCapabilities":{"http":true},"loadSession":true}})
    }

    fn config_options(mode: bool, model: bool, effort: bool) -> Value {
        let mut options = vec![];
        if mode {
            options.push(json!({"id":"task-mode","category":"mode","options":[{"value":"build","name":"Build"},{"value":"plan","name":"Plan"}]}));
        }
        if model {
            options.push(json!({"id":"model","category":"model","options":[{"value":"opus","name":"Opus"},{"value":"sonnet","name":"Sonnet"}]}));
        }
        if effort {
            options.push(json!({"id":"thought","category":"thought_level","options":[{"value":"low","name":"Low"},{"value":"high","name":"High"}]}));
        }
        json!(options)
    }

    fn session(options: Value) -> Value {
        json!({"jsonrpc":"2.0","id":2,"result":{"sessionId":"s1","configOptions":options}})
    }

    fn started(provider: &str, mode: &str) -> Protocol {
        let mut chosen = config(provider);
        chosen.mode = mode.into();
        let mut p = Protocol::new(chosen);
        p.receive(json!({"jsonrpc":"2.0","id":1,"result":capabilities()}))
            .unwrap();
        p
    }

    #[test]
    fn acp_errors_preserve_structured_cause_without_serializing_opaque_data() {
        let mut p = Protocol::new(config("pantheon"));
        let error = p.receive(json!({"id":2,"error":{"code":-32603,"message":"Internal error","data":{"message":"ValueError: invalid working directory","apiKey":"fixture-secret","stack":"private opaque metadata"}}})).err().unwrap();
        assert!(error.contains("session creation or resume"));
        assert!(error.contains("ValueError: invalid working directory"));
        assert!(!error.contains("fixture-secret"));
        assert!(!error.contains("private opaque metadata"));
    }

    #[test]
    fn initialize_opens_a_session_and_selects_the_advertised_mode() {
        let mut p = started("pantheon", "plan");
        let step = p
            .receive(session(config_options(true, false, false)))
            .unwrap();
        assert_eq!(step.write.len(), 1);
        assert_eq!(step.write[0]["method"], "session/set_config_option");
        assert_eq!(step.write[0]["params"]["value"], "plan");
        let session = step
            .events
            .iter()
            .find(|event| event.kind == "session")
            .unwrap();
        assert_eq!(session.text, "s1");
    }

    #[test]
    fn a_resumed_session_uses_the_advertised_resume_call() {
        let mut resumed = config("pantheon");
        resumed.resume_id = Some("saved".into());
        let mut p = Protocol::new(resumed);
        let mut caps = capabilities();
        caps["agentCapabilities"]["sessionCapabilities"] = json!({"resume":{}});
        let step = p
            .receive(json!({"jsonrpc":"2.0","id":1,"result":caps}))
            .unwrap();
        assert_eq!(step.write[0]["method"], "session/resume");
        assert_eq!(step.write[0]["params"]["sessionId"], "saved");
        // Without the capability the protocol's loadSession call is the only resumable path.
        let mut without_resume = config("pantheon");
        without_resume.resume_id = Some("saved".into());
        let mut p = Protocol::new(without_resume);
        let step = p
            .receive(json!({"jsonrpc":"2.0","id":1,"result":capabilities()}))
            .unwrap();
        assert_eq!(step.write[0]["method"], "session/load");
    }

    #[test]
    fn a_server_that_cannot_resume_refuses_instead_of_starting_a_fresh_session() {
        let mut resumed = config("pantheon");
        resumed.resume_id = Some("saved".into());
        let mut p = Protocol::new(resumed);
        let error = p
            .receive(json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}))
            .err()
            .unwrap();
        assert!(error.contains("cannot resume saved tasks"), "{error}");
    }

    #[test]
    fn a_protocol_version_other_than_one_is_refused() {
        let mut p = Protocol::new(config("pantheon"));
        let error = p
            .receive(json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":2}}))
            .err()
            .unwrap();
        assert!(error.contains("protocol v1"), "{error}");
    }

    #[test]
    fn a_mode_the_agent_does_not_advertise_is_refused_instead_of_widening_access() {
        let mut p = started("pantheon", "plan");
        let error = p
            .receive(session(config_options(false, false, false)))
            .err();
        assert!(error.is_none(), "no mode surface means nothing to select");
        let mut p = started("pantheon", "plan");
        let error = p
            .receive(session(json!([{"id":"task-mode","category":"mode","options":[{"value":"build","name":"Build"}]}])))
            .err()
            .unwrap();
        assert!(
            error.contains("does not advertise the selected task mode"),
            "{error}"
        );
    }

    #[test]
    fn model_and_effort_follow_the_mode_and_reach_the_prompt() {
        let mut selected = config("pantheon");
        selected.model = "sonnet".into();
        selected.reasoning_effort = Some("high".into());
        let mut p = Protocol::new(selected);
        p.receive(json!({"jsonrpc":"2.0","id":1,"result":capabilities()}))
            .unwrap();
        // One setting per round trip: mode, then model, then reasoning, then the prompt.
        let step = p
            .receive(session(config_options(true, true, true)))
            .unwrap();
        assert_eq!(step.write.len(), 1);
        assert_eq!(step.write[0]["params"]["value"], "build");
        let step = p
            .receive(json!({"jsonrpc":"2.0","id":4,"result":{}}))
            .unwrap();
        assert_eq!(step.write[0]["params"]["value"], "sonnet");
        let step = p
            .receive(json!({"jsonrpc":"2.0","id":5,"result":{}}))
            .unwrap();
        assert_eq!(step.write[0]["params"]["value"], "high");
        let step = p
            .receive(json!({"jsonrpc":"2.0","id":6,"result":{}}))
            .unwrap();
        assert_eq!(step.write[0]["method"], "session/prompt");
        assert_eq!(
            step.write[0]["params"]["prompt"][0]["text"],
            "Explain this project"
        );
    }

    #[test]
    fn an_unadvertised_model_is_refused_rather_than_silently_defaulted() {
        let mut selected = config("pantheon");
        selected.model = "kimi-k9".into();
        let mut p = Protocol::new(selected);
        p.receive(json!({"jsonrpc":"2.0","id":1,"result":capabilities()}))
            .unwrap();
        let error = p
            .receive(session(json!([{"id":"model","category":"model","options":[{"value":"opus","name":"Opus"}]}])))
            .err()
            .unwrap();
        assert!(error.contains("does not advertise this model"), "{error}");
    }

    fn permission(options: Value) -> Value {
        json!({"jsonrpc":"2.0","id":71,"method":"session/request_permission","params":{"sessionId":"s1","toolCall":{"toolCallId":"t1","kind":"edit","title":"Write a file"},"options":options}})
    }

    fn ready(mode: &str) -> Protocol {
        let mut p = started("pantheon", mode);
        p.receive(session(config_options(true, false, false)))
            .unwrap();
        p.receive(json!({"jsonrpc":"2.0","id":4,"result":{}}))
            .unwrap();
        p
    }

    #[test]
    fn full_access_answers_a_single_grant_and_build_waits_for_the_user() {
        let options = json!([{"optionId":"allow","kind":"allow_once","name":"Allow"},{"optionId":"deny","kind":"reject_once","name":"Reject"}]);
        let mut full = ready("full");
        let step = full.receive(permission(options.clone())).unwrap();
        assert_eq!(step.write[0]["result"]["outcome"]["optionId"], "allow");
        let mut build = ready("build");
        let step = build.receive(permission(options)).unwrap();
        assert!(
            step.write.is_empty(),
            "Build must surface the request in the approval bar"
        );
        assert_eq!(step.events[0].kind, "approval");
    }

    #[test]
    fn a_structured_question_is_never_auto_answered() {
        let options = json!([{"optionId":"q0_opt_0","kind":"allow_once","name":"Yes"}]);
        let mut full = ready("full");
        let step = full.receive(permission(options)).unwrap();
        assert_eq!(
            step.write[0]["result"]["outcome"]["outcome"], "cancelled",
            "an agent question is for the user, whatever the mode"
        );
        assert!(step
            .events
            .iter()
            .any(|event| event.text.contains("structured input")));
    }

    #[test]
    fn a_permission_for_another_session_is_cancelled() {
        let mut p = ready("full");
        let mut request =
            permission(json!([{"optionId":"allow","kind":"allow_once","name":"Allow"}]));
        request["params"]["sessionId"] = json!("other");
        let step = p.receive(request).unwrap();
        assert_eq!(step.write[0]["result"]["outcome"]["outcome"], "cancelled");
    }

    #[cfg(unix)]
    fn fixture(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn metadata_survives_missing_commands_after_slow_startup() {
        assert!(DISCOVERY_STARTUP >= Duration::from_secs(60));
        let command = fixture(
            r#"
            sleep 0.1
            read -r initialize
            printf '%s
' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}'
            read -r session
            printf '%s
' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"fixture","configOptions":[{"id":"model","category":"model","options":[{"value":"m","name":"M"}]}]}}'
            read -r forbidden
        "#,
        );
        let start = std::time::Instant::now();
        let (metadata, commands) = inspect_state(
            command,
            &std::env::temp_dir(),
            true,
            Duration::from_secs(2),
            Duration::from_millis(20),
        )
        .await
        .unwrap();
        assert_eq!(metadata["sessionId"], "fixture");
        assert!(commands.is_none());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn readiness_distinguishes_startup_authentication_and_session_failures() {
        for (reply, state, acp_ready) in [
            (
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":2}}"#,
                "startupFailed",
                false,
            ),
            (
                r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32000,"message":"Login required"}}"#,
                "authenticationRequired",
                true,
            ),
            (
                r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32603,"message":"Invalid working directory"}}"#,
                "sessionFailed",
                true,
            ),
            (
                r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"ready"}}"#,
                "ready",
                true,
            ),
        ] {
            let startup = if acp_ready {
                "read -r init; printf '%s\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":1}}';"
            } else {
                ""
            };
            let command = fixture(&format!(
                "{startup} read -r request; printf '%s\n' '{reply}'; read -r forbidden"
            ));
            let status = connection_status(
                inspect_state(
                    command,
                    &std::env::temp_dir(),
                    false,
                    Duration::from_secs(2),
                    Duration::from_millis(20),
                )
                .await,
            );
            assert_eq!(status.state, state);
            assert!(status.installed);
            assert_eq!(status.acp_ready, acp_ready);
            assert_eq!(status.session_ready, state == "ready");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn readiness_reports_timeout_and_stderr_cause_without_credentials() {
        let failure = inspect_state(
            fixture("read -r init; sleep 2"),
            &std::env::temp_dir(),
            false,
            Duration::from_millis(20),
            COMMAND_GRACE,
        )
        .await
        .unwrap_err();
        assert_eq!(failure.message, ACP_STARTUP_TIMEOUT);
        assert!(!failure.acp_ready);
        let failure = inspect_state(fixture("printf '%s\n' 'FutureWarning: Python is deprecated' 'Authorization: Bearer fixture-secret' 'ValueError: invalid cwd' >&2; exit 1"), &std::env::temp_dir(), false, Duration::from_secs(2), COMMAND_GRACE).await.unwrap_err();
        assert!(failure.message.contains("ValueError: invalid cwd"));
        assert!(!failure.message.contains("fixture-secret"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn readiness_declines_agent_requests_without_sending_a_prompt() {
        let command = fixture(
            r#"
            read -r request
            case "$request" in *'"method":"initialize"'*) ;; *) exit 1;; esac
            printf '%s
' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1}}'
            read -r request
            case "$request" in *'"method":"session/new"'*) ;; *) exit 1;; esac
            printf '%s
' '{"jsonrpc":"2.0","id":90,"method":"fs/write_text_file","params":{"path":"unused","content":"must not write"}}'
            read -r request
            case "$request" in *'"code":-32601'*) ;; *) exit 1;; esac
            printf '%s
' '{"jsonrpc":"2.0","id":2,"result":{"sessionId":"ready"}}'
            read -r forbidden
            case "$forbidden" in *'"method":"session/prompt"'*) exit 1;; esac
        "#,
        );
        let (metadata, _) = inspect_with(command, &std::env::temp_dir(), false)
            .await
            .unwrap();
        assert_eq!(metadata["sessionId"], "ready");
    }

    #[test]
    fn startup_status_names_each_acknowledged_stage() {
        let mut p = started("pantheon", "build");
        let step = p
            .receive(session(config_options(true, true, false)))
            .unwrap();
        assert!(
            step.events
                .iter()
                .any(|event| event.text == "Applying task mode"),
            "{:?}",
            step.events
        );
    }
}
