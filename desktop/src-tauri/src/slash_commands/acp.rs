//! Pantheon's own command catalog, read from the ACP session it advertises.
use super::{Catalog, Invocation, SlashCommand};
use serde_json::Value;
use std::{collections::HashSet, path::Path};

pub(super) const NO_COMMANDS: &str =
    "Pantheon did not advertise additional commands. Start a task or refresh to sync its command list.";

/// Opens a throwaway `pantheon serve acp` session and reads its first
/// `available_commands_update`. No prompt is ever sent.
pub(super) async fn discover(provider: &str, cwd: &Path) -> Result<Catalog, String> {
    if provider != "pantheon" {
        return Err("Pantheon is the only supported provider.".into());
    }
    let path = crate::providers::executable(provider)
        .ok_or("Install Pantheon or choose its executable in Settings.")?;
    let mut command = crate::providers::command(&path)?;
    command.args(["serve", "acp"]);
    let (_, frame) = crate::providers::acp::inspect_with(command, cwd, true).await?;
    frame
        .as_ref()
        .and_then(|frame| parse_runtime(provider, frame))
        .ok_or(NO_COMMANDS.into())
}

pub(super) fn parse_runtime(provider: &str, frame: &Value) -> Option<Catalog> {
    if frame["method"] != "session/update"
        || frame["params"]["update"]["sessionUpdate"] != "available_commands_update"
    {
        return None;
    }
    let commands = frame["params"]["update"]["availableCommands"]
        .as_array()?
        .iter()
        .filter_map(|command| {
            let name = command["name"].as_str()?.trim();
            if name.is_empty() || name.len() > 128 {
                return None;
            }
            let hint = command["input"]["hint"]
                .as_str()
                .map(|hint| hint.chars().take(120).collect::<String>());
            Some(SlashCommand {
                id: format!("{provider}:/{name}"),
                name: name.into(),
                argument_hint: hint,
                description: command["description"]
                    .as_str()
                    .map(|text| text.chars().take(500).collect())
                    .unwrap_or_default(),
                source: provider.into(),
                kind: "native".into(),
                reason: None,
                invocation: Invocation::Native,
            })
        })
        .take(512)
        .collect::<Vec<_>>();
    if commands.is_empty() {
        return None;
    }
    let _ = HashSet::<&str>::new();
    Some(Catalog {
        commands,
        ..Catalog::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn commands_use_real_provider_names_and_advertised_hints() {
        let catalog = parse_runtime("pantheon", &json!({"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"available_commands_update","availableCommands":[{"name":"review","description":"Review changes","input":{"hint":"[focus]"}}]}}})).unwrap();
        assert_eq!(catalog.commands[0].id, "pantheon:/review");
        assert_eq!(
            catalog.commands[0].argument_hint.as_deref(),
            Some("[focus]")
        );
    }

    #[test]
    fn an_empty_or_foreign_frame_advertises_nothing() {
        assert!(parse_runtime("pantheon", &json!({"method":"session/update","params":{"update":{"sessionUpdate":"available_commands_update","availableCommands":[]}}})).is_none());
        assert!(parse_runtime("pantheon", &json!({"method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk"}}})).is_none());
    }
}
