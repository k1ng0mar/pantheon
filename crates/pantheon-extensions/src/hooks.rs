//! Hook points. Pantheon hooks are a superset of the Hermes hook surface
//! observed in the wild (pre_llm_call, pre/post_api_request,
//! pre_gateway_dispatch), each mapped to a canonical Pantheon event.
use serde::{Deserialize, Serialize};

/// Every extension hook point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Hook {
    /// Before model inference. May inject context (anti-ai-writing, time-gap).
    PreLlmCall,
    /// Before an outbound provider API request.
    PreApiRequest,
    /// After a provider API response.
    PostApiRequest,
    /// Before a gateway dispatches an inbound message.
    PreGatewayDispatch,
}

impl Hook {
    pub fn name(&self) -> &'static str {
        match self {
            Hook::PreLlmCall => "pre_llm_call",
            Hook::PreApiRequest => "pre_api_request",
            Hook::PostApiRequest => "post_api_request",
            Hook::PreGatewayDispatch => "pre_gateway_dispatch",
        }
    }
    pub fn parse(name: &str) -> Option<Hook> {
        match name.trim() {
            "pre_llm_call" => Some(Hook::PreLlmCall),
            "pre_api_request" => Some(Hook::PreApiRequest),
            "post_api_request" => Some(Hook::PostApiRequest),
            "pre_gateway_dispatch" => Some(Hook::PreGatewayDispatch),
            _ => None,
        }
    }
    /// All known hooks (for doctor + docs).
    pub fn all() -> &'static [Hook] {
        &[Hook::PreLlmCall, Hook::PreApiRequest,
          Hook::PostApiRequest, Hook::PreGatewayDispatch]
    }
}
