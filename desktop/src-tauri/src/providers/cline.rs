//! Cline (`cline --acp`) over the shared ACP transport. Cline keeps its own
//! sign-in (`cline auth`, or `CLINE_API_KEY` in the environment). Its ACP
//! session modes are Plan and Act; approvals arrive as permission requests, and
//! the reasoning effort is a launch flag because Cline has no ACP option for it.
use super::*;

/// Shown when Cline has no stored sign-in and no `CLINE_API_KEY`.
pub(super) const SIGN_IN: &str = "Cline is not signed in. Run `cline auth` in a terminal, then send again.";

/// The efforts `cline --thinking` accepts, least to most.
pub(super) const EFFORTS: [&str; 5] = ["none", "low", "medium", "high", "xhigh"];

pub(super) fn configure(command: &mut Command, config: &RunConfig) {
    command.arg("--acp");
    if let Some(effort) = &config.reasoning_effort {
        command.args(["--thinking", effort]);
    }
    // Full access skips Cline's per-tool prompts; every other mode keeps them so
    // Pantheon decides (and Plan declines edits).
    command.args(["--auto-approve", if config.mode == "full" { "true" } else { "false" }]);
}

/// Cline's ACP prompt keeps only text blocks, but its `read_files` tool opens
/// images, so a screenshot is passed by path. Images precede the prompt text.
pub(super) fn image_reference(path: &Path) -> String {
    format!("Attached image: {} (open it with read_files to see it).\n\n", path.display())
}

/// Cline asks before every tool, even reads. Reads and searches are answered
/// here as Standard mode would; Plan also allows fetches and declines changes to files. `None` leaves
/// the request to the shared approval path.
pub(super) fn permission(mode: &str, kind: &str) -> Option<&'static str> {
    match kind {
        "read" | "search" => Some("allow_once"),
        "fetch" if mode == "plan" => Some("allow_once"),
        "edit" | "delete" | "move" if mode == "plan" => Some("reject_once"),
        _ => None,
    }
}

/// Models come from `session/new` (no prompt, no inference); every model gets
/// Cline's `--thinking` levels, with the CLI default left unset.
pub(super) async fn models(path: &Path) -> Result<Vec<ModelInfo>, String> {
    let models = if let Some(models) = acp_catalog::get("cline", path) {
        models
    } else {
        let _refresh = acp_catalog::refresh_lock("cline", path).await;
        if let Some(models) = acp_catalog::get("cline", path) {
            models
        } else {
            let discovery = acp_catalog::begin("cline", path);
            let mut command = super::command(path)?;
            command.arg("--acp");
            let (metadata, _) = acp::inspect(command, &std::env::temp_dir(), false).await?;
            acp_catalog::store(discovery, &metadata);
            acp::parse_models(&metadata)
        }
    };
    Ok(with_efforts(models))
}

fn with_efforts(models: Vec<ModelInfo>) -> Vec<ModelInfo> {
    models
        .into_iter()
        .map(|model| ModelInfo { reasoning_efforts: EFFORTS.iter().map(|id| effort_info(id, None)).collect(), ..model })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true,"promptCapabilities":{"image":true,"audio":false,"embeddedContext":false}},"agentInfo":{"name":"cline","version":"3.0.73"},"authMethods":[{"id":"cline","name":"Sign in with Cline"},{"id":"cline-pass","name":"Sign in with ClinePass"},{"id":"openai-codex","name":"Sign in with ChatGPT Subscription"}]}}"#;

    /// Recorded from `cline --acp` 3.0.73 `session/new` (model list shortened).
    fn session() -> Value {
        json!({"sessionId":"1792719287191_NBdt7_cli",
            "modes":{"availableModes":[{"id":"plan","name":"Plan","description":"Explore the codebase and plan changes without modifying files"},{"id":"act","name":"Act","description":"Make changes to the codebase"}],"currentModeId":"act"},
            "models":{"availableModels":[{"modelId":"anthropic/claude-sonnet-5.5","name":"Claude Sonnet 5.5"},{"modelId":"anthropic/claude-opus-5.5","name":"Claude Opus 5.5"}],"currentModelId":"anthropic/claude-sonnet-5.5"},
            "configOptions":[
                {"type":"select","id":"provider","name":"Provider","description":"The authentication provider to use","category":"model","currentValue":"cline","options":[{"value":"cline","name":"Cline Usage-Billing"},{"value":"cline-pass","name":"ClinePass"},{"value":"openai-codex","name":"OpenAI ChatGPT Subscription"}]},
                {"type":"select","id":"model","name":"Model","category":"model","currentValue":"anthropic/claude-sonnet-5.5","options":[{"value":"anthropic/claude-sonnet-5.5","name":"Claude Sonnet 5.5"},{"value":"anthropic/claude-opus-5.5","name":"Claude Opus 5.5"}]},
                {"type":"select","id":"mode","name":"Session Mode","description":"Controls whether the agent can modify files","category":"mode","currentValue":"act","options":[{"value":"plan","name":"Plan","description":"Explore the codebase and plan changes without modifying files"},{"value":"act","name":"Act","description":"Make changes to the codebase"}]},
                {"type":"boolean","id":"auto_approve","name":"Auto-approve tools","description":"Automatically approve all tool calls without asking for permission","currentValue":false}]})
    }

    fn ready(mode: &str, model: &str, effort: Option<&str>) -> (Protocol, Vec<Value>) {
        let mut cfg = super::super::tests::config("cline");
        cfg.mode = mode.into();
        cfg.model = model.into();
        cfg.reasoning_effort = effort.map(str::to_owned);
        let mut p = Protocol::new(cfg);
        let mut writes = p.receive(serde_json::from_str(INITIALIZE).unwrap()).unwrap().write;
        assert_eq!(writes[0]["method"], "session/new");
        writes.extend(p.receive(json!({"jsonrpc":"2.0","id":2,"result":session()})).unwrap().write);
        (p, writes)
    }

    /// Recorded from `cline --acp` 3.0.73 with an empty config directory.
    #[test]
    fn a_session_without_sign_in_names_the_auth_command() {
        let mut p = Protocol::new(super::super::tests::config("cline"));
        p.receive(serde_json::from_str(INITIALIZE).unwrap()).unwrap();
        let error = p.receive(json!({"jsonrpc":"2.0","id":2,"error":{"code":-32000,"message":"Authentication required: Call authenticate before starting a session"}})).err().unwrap();
        assert_eq!(error, SIGN_IN);
    }

    #[test]
    fn plan_and_act_follow_the_approval_choice() {
        for (mode, expected) in [("ask", "act"), ("auto", "act"), ("plan", "plan"), ("full", "act")] {
            let (mut p, writes) = ready(mode, "default", None);
            let set = &writes[1];
            assert_eq!(set["method"], "session/set_config_option", "{mode}");
            assert_eq!((set["params"]["configId"].clone(), set["params"]["value"].clone()), (json!("mode"), json!(expected)), "{mode}");
            let prompt = p.receive(json!({"jsonrpc":"2.0","id":4,"result":{"configOptions":session()["configOptions"]}})).unwrap().write.remove(0);
            assert_eq!(prompt["method"], "session/prompt");
        }
    }

    #[test]
    fn model_is_set_and_effort_rides_on_the_launch_flag() {
        let (mut p, writes) = ready("ask", "anthropic/claude-opus-5.5", Some("high"));
        assert_eq!(writes[1]["params"]["configId"], "mode");
        let set = p.receive(json!({"jsonrpc":"2.0","id":4,"result":{}})).unwrap().write.remove(0);
        assert_eq!((set["params"]["configId"].clone(), set["params"]["value"].clone()), (json!("model"), json!("anthropic/claude-opus-5.5")));
        let prompt = p.receive(json!({"jsonrpc":"2.0","id":5,"result":{}})).unwrap().write.remove(0);
        assert_eq!(prompt["method"], "session/prompt");

        let mut cfg = super::super::tests::config("cline");
        cfg.mode = "full".into();
        cfg.reasoning_effort = Some("xhigh".into());
        let mut command = Command::new("cline");
        configure(&mut command, &cfg);
        let args: Vec<_> = command.as_std().get_args().map(|arg| arg.to_string_lossy().into_owned()).collect();
        assert_eq!(args, ["--acp", "--thinking", "xhigh", "--auto-approve", "true"]);
    }

    #[test]
    fn recorded_catalog_lists_models_with_thinking_levels() {
        let models = with_efforts(acp::parse_models(&session()));
        assert_eq!(models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["default", "anthropic/claude-sonnet-5.5", "anthropic/claude-opus-5.5"]);
        assert_eq!(models[2].reasoning_efforts.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), EFFORTS);
        for effort in EFFORTS {
            assert!(validate_reasoning_effort("cline", Some(effort)).is_ok());
        }
    }

    /// Cline's recorded permission options and tool kinds.
    fn permission_step(p: &mut Protocol, kind: &str) -> Step {
        p.receive(json!({"jsonrpc":"2.0","id":9,"method":"session/request_permission","params":{"sessionId":"1792719287191_NBdt7_cli",
            "toolCall":{"toolCallId":"c1","title":"editor: a.txt","kind":kind,"status":"pending"},
            "options":[{"optionId":"allow_once","name":"Allow once","kind":"allow_once"},{"optionId":"allow_always","name":"Allow always","kind":"allow_always"},{"optionId":"reject_once","name":"Reject","kind":"reject_once"}]}})).unwrap()
    }

    #[test]
    fn permissions_follow_the_task_mode() {
        let outcome = |step: &Step| step.write.first().map(|w| w["result"]["outcome"]["optionId"].clone());
        for (mode, kind, expected) in [
            ("ask", "read", Some(json!("allow_once"))),
            ("plan", "search", Some(json!("allow_once"))),
            ("plan", "edit", Some(json!("reject_once"))),
            ("plan", "fetch", Some(json!("allow_once"))),
            ("ask", "fetch", None),
            ("auto", "edit", Some(json!("allow_once"))),
            ("full", "execute", Some(json!("allow_once"))),
            ("ask", "edit", None),
            ("auto", "execute", None),
            ("plan", "execute", None),
        ] {
            let (mut p, _) = ready(mode, "default", None);
            let step = permission_step(&mut p, kind);
            assert_eq!(outcome(&step), expected, "{mode} {kind}");
            if expected.is_none() {
                assert!(step.events.iter().any(|e| e.kind == "approval"), "{mode} {kind}");
            }
        }
    }

    #[test]
    fn a_resumed_task_loads_its_saved_session() {
        let mut cfg = super::super::tests::config("cline");
        cfg.resume_id = Some("1792719287191_NBdt7_cli".into());
        let mut p = Protocol::new(cfg);
        let request = p.receive(serde_json::from_str(INITIALIZE).unwrap()).unwrap().write.remove(0);
        assert_eq!(request["method"], "session/load");
        assert_eq!(request["params"]["sessionId"], "1792719287191_NBdt7_cli");
        let mut loaded = session();
        loaded.as_object_mut().unwrap().remove("sessionId");
        let saved = p.receive(json!({"jsonrpc":"2.0","id":2,"result":loaded})).unwrap().events;
        assert_eq!(saved.iter().find(|e| e.kind == "session").unwrap().text, "1792719287191_NBdt7_cli");
    }

    #[test]
    fn images_become_path_references() {
        assert_eq!(image_reference(Path::new("/tmp/a.png")), "Attached image: /tmp/a.png (open it with read_files to see it).\n\n");
    }

    #[tokio::test]
    #[ignore = "needs the Cline CLI and a sign-in; metadata only, never sends a prompt"]
    async fn live_cline_lists_models() {
        super::super::widen_process_path(); // as the app does at startup
        let path = executable("cline").expect("Install Cline first");
        let models = models(&path).await.unwrap();
        assert!(models.len() > 1, "{models:?}");
    }
}
