//! Client-owned tools. These run in the host, so the host must validate scope and
//! arguments; the provider's shell sandbox does not cover these callbacks.
use super::*;
use std::{future::Future, pin::Pin};

pub type ToolFuture = Pin<Box<dyn Future<Output = Result<Value, String>> + Send>>;
pub type ToolHandler = Arc<dyn Fn(String, Value) -> ToolFuture + Send + Sync>;
#[derive(Clone)]
pub struct ToolHost {
    pub tools: Vec<Value>,
    pub handler: ToolHandler,
    pub approval_tools: HashSet<String>,
}

/// Validates a `ToolHost` catalog before it is exposed to the agent over MCP:
/// every protected name must be a registered function of the right shape.
/// A tool catalog may advertise at most this many callbacks.
const MAX_CALLS: usize = 64;
pub(super) struct HostTools {
    /// `(namespace, name)` pairs the catalog may advertise.
    _catalog: usize,
}
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
}
pub(super) fn bounded_json(value: &Value, limit: usize) -> Result<String, String> {
    let mut preview = Preview::new(limit);
    serde_json::to_writer(&mut preview, value)
        .map_err(|_| "Host tool JSON exceeds its size limit".to_string())?;
    Ok(preview.finish())
}
impl HostTools {
    pub fn new(host: ToolHost) -> Result<Self, String> {
        bounded_json(&json!(host.tools), 128 * 1024)?;
        let mut catalog: HashSet<(Option<String>, String)> = HashSet::new();
        for spec in &host.tools {
            let namespace = match field(spec, "type") {
                "function" => None,
                "namespace" if valid_name(field(spec, "name")) => {
                    Some(field(spec, "name").to_owned())
                }
                _ => return Err("Invalid host tool specification".into()),
            };
            let entries = if namespace.is_some() {
                spec["tools"]
                    .as_array()
                    .ok_or("Invalid host tool namespace")?
                    .iter()
                    .collect::<Vec<_>>()
            } else {
                vec![spec]
            };
            for entry in entries {
                let name = field(entry, "name");
                if field(entry, "type") != "function"
                    || !valid_name(name)
                    || !entry["inputSchema"].is_object()
                    || !entry["description"].is_string()
                    || !catalog.insert((namespace.clone(), name.to_owned()))
                {
                    return Err("Invalid or duplicate host tool specification".into());
                }
            }
        }
        if catalog.len() > MAX_CALLS
            || host
                .approval_tools
                .iter()
                .any(|name| !catalog.iter().any(|(_, registered)| registered == name))
        {
            return Err("Invalid host tool approval catalog".into());
        }
        Ok(Self {
            _catalog: catalog.len(),
        })
    }
}
/// Only app-owned callbacks create this envelope. Keep image bytes out of tool
/// activity text and render them as protocol image blocks, not a base64 transcript.
pub(super) fn image_content(value: &Value) -> Result<Option<(String, String, String)>, String> {
    use base64::{engine::general_purpose::STANDARD, Engine};
    let Some(image) = value.get("_pantheonImage") else {
        return Ok(None);
    };
    let mime = image["mimeType"].as_str().ok_or("Invalid image response")?;
    let data = image["data"].as_str().ok_or("Invalid image response")?;
    let text = image["text"]
        .as_str()
        .unwrap_or("Moodboard visual reference");
    if mime != "image/jpeg" || data.len() > 2 * 1024 * 1024 * 4 / 3 + 4 || text.len() > 8192 {
        return Err("Image response exceeds the supported limits".into());
    }
    let bytes = STANDARD
        .decode(data)
        .map_err(|_| "Invalid image response")?;
    if bytes.len() > 2 * 1024 * 1024 || !bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Err("Invalid image response".into());
    }
    Ok(Some((mime.into(), data.into(), text.into())))
}

impl Protocol {
    pub(super) fn user_input(&mut self, id: &str, response: Value) -> Result<Step, String> {
        let request = self
            .inputs
            .get(id)
            .ok_or("This input request is no longer active")?;
        // Validation finishes before removing pending state. The run loop sends
        // the UI an acknowledgement only after writing this exact response.
        let result = host_inputs::answer(request, response)?;
        let request = self.inputs.remove(id).unwrap();
        let mut event = self.event("input_cancelled", "", None);
        event.request_id = Some(id.into());
        Ok(Step {
            write: vec![json!({"id":request["id"],"result":result})],
            events: vec![event],
            ..Step::default()
        })
    }
    pub(super) fn cancel_requests(&mut self) -> Step {
        let mut step = Step::default();
        // Permission requests are answered on the ACP transport, so nothing can
        // be queued here; flushing the map keeps the event stream honest.
        for id in std::mem::take(&mut self.pending).into_keys() {
            let mut event = self.event("approval_cancelled", "", None);
            event.request_id = Some(id);
            step.events.push(event);
        }
        let requests: Vec<_> = self.inputs.drain().collect();
        for (id, request) in requests {
            let response = if request["method"] == "item/tool/requestUserInput" {
                json!({"answers":{}})
            } else {
                json!({"action":"cancel","content":null})
            };
            if let Ok(result) = host_inputs::answer(&request, response) {
                step.write.push(json!({"id":request["id"],"result":result}));
            }
            let mut event = self.event("input_cancelled", "", None);
            event.request_id = Some(id);
            step.events.push(event);
        }
        step
    }
}

pub(super) async fn apply_step(
    protocol: &mut Protocol,
    step: Step,
    sink: &Sink,
    stdin: &mut ChildStdin,
) -> Result<bool, String> {
    let _ = protocol;
    for event in step.events {
        sink(event);
    }
    for message in step.write {
        write_json(stdin, &message).await?;
    }
    if let Some(error) = step.failure {
        return Err(error);
    }
    Ok(step.done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine};

    #[test]
    fn host_tool_catalogs_are_validated_before_the_agent_sees_them() {
        let host = ToolHost {
            tools: vec![
                json!({"type":"function","name":"pantheon_test","description":"test","inputSchema":{"type":"object"}}),
            ],
            handler: Arc::new(|_, args| Box::pin(async move { Ok(args) })),
            approval_tools: HashSet::from(["pantheon_test".into()]),
        };
        assert!(HostTools::new(host.clone()).is_ok());
        let mut bad = host.clone();
        bad.tools[0]["type"] = json!("other");
        assert!(HostTools::new(bad).is_err());
        let mut missing = host;
        missing.approval_tools = HashSet::from(["pantheon_absent".into()]);
        assert!(HostTools::new(missing).is_err());
    }

    #[test]
    fn image_results_stay_image_blocks_and_remain_bounded() {
        let image = json!({"_pantheonImage":{"mimeType":"image/jpeg","data":STANDARD.encode([0xff,0xd8,0xff,0xd9]),"text":"A visual reference"}});
        let (mime, data, text) = image_content(&image).unwrap().unwrap();
        assert_eq!(mime, "image/jpeg");
        assert_eq!(text, "A visual reference");
        assert_eq!(STANDARD.decode(data).unwrap(), vec![0xff, 0xd8, 0xff, 0xd9]);
        assert!(image_content(
            &json!({"_pantheonImage":{"mimeType":"image/svg+xml","data":"PHN2Zz4="}})
        )
        .is_err());
        assert!(image_content(
            &json!({"_pantheonImage":{"mimeType":"image/jpeg","data":"a".repeat(3*1024*1024)}})
        )
        .is_err());
        assert!(bounded_json(&json!({"text":"x".repeat(140000)}), 32 * 1024).is_err());
    }
}
