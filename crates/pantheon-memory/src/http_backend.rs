//! HTTP-based memory backend adapter. Talks to any external memory service
//! that implements a small JSON API (recall, write, list_agent). This is
//! the adapter pattern for services like GalaxyMem, Mnemosyne, Honcho, or
//! Hindsight — they run as a separate process or remote service, Pantheon
//! holds the capability gate, and the HTTPBackend forwards calls.
//!
//! The protocol is deliberately simple:
//!
//!   GET  /v1/memory/recall?query=...&limit=N&layers=...
//!     -> 200 [{"key":..,"value":..,"rank":..,"provenance":{...}}]
//!
//!   POST /v1/memory/write
//!     body: {"layer":"Agent","namespace":"nyx","key":"..","value":"..","provenance":{...},
//!            "max_bytes":N}
//!     -> 200 {"key":..,"value":..,"provenance":{...}}
//!
//!   GET  /v1/memory/list_agent?namespace=...
//!     -> 200 [["key","value"], ...]
//!
//! Error responses: 4xx/5xx with JSON {"error":"CODE","cause":"..."} or
//! plain text. The adapter maps those back to PantheonError.
use crate::{LayerKind, MemoryBackend, MemoryRecord, Proposal, Provenance, Recalled};
use pantheon_core::capability::Policy;
use pantheon_core::error::{Layer, PantheonError};
use serde::{Deserialize, Serialize};

fn merr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Memory,
        false,
        cause,
        "check the backend service",
        "",
    )
}

#[derive(Debug, Clone)]
pub struct HttpBackend {
    base: String,
    api_key: Option<String>,
    /// Path prefix the service mounts the protocol under. Default
    /// `/v1/memory`; custom plugins set their own (e.g. `/memory`).
    prefix: String,
}

impl HttpBackend {
    pub fn new(base: String, api_key: Option<String>) -> Self {
        Self {
            base,
            api_key,
            prefix: "/v1/memory".into(),
        }
    }

    /// Same adapter, custom protocol mount point. Trailing slashes are
    /// normalized so both `url` and `prefix` can be written either way.
    pub fn with_prefix(base: String, api_key: Option<String>, prefix: impl Into<String>) -> Self {
        let mut p = prefix.into();
        if !p.starts_with('/') {
            p.insert(0, '/');
        }
        while p.ends_with('/') {
            p.pop();
        }
        Self {
            base,
            api_key,
            prefix: p,
        }
    }

    /// `<base><prefix>/<suffix>` with the base trailing slash trimmed.
    fn endpoint(&self, suffix: &str) -> String {
        format!("{}{}/{}", self.base.trim_end_matches('/'), self.prefix, suffix)
    }
}

#[derive(Serialize)]
struct WriteRequest {
    layer: String,
    namespace: String,
    key: String,
    value: String,
    provenance: Provenance,
    max_bytes: usize,
}

#[derive(Deserialize, Default)]
struct ErrorResponse {
    error: Option<String>,
    cause: Option<String>,
}

fn layer_str(layer: LayerKind) -> String {
    match layer {
        LayerKind::Global => "Global",
        LayerKind::Agent => "Agent",
        LayerKind::Project => "Project",
        LayerKind::TaskSession => "TaskSession",
        LayerKind::EphemeralTurn => "EphemeralTurn",
    }
    .to_string()
}

impl MemoryBackend for HttpBackend {
    fn recall(
        &self,
        _policy: &Policy,
        layers: &[LayerKind],
        query: &str,
        limit: usize,
    ) -> Result<Vec<Recalled>, PantheonError> {
        let layers_str = layers
            .iter()
            .copied()
            .map(layer_str)
            .collect::<Vec<_>>()
            .join(",");
        let url = format!(
            "{}?query={}&limit={}&layers={}",
            self.endpoint("recall"),
            pct(query),
            limit,
            pct(&layers_str)
        );
        let resp = http_get(&url, self.api_key.as_deref())?;
        let hits: Vec<Recalled> = serde_json::from_slice(&resp.body).map_err(|e| {
            merr(
                "MEM_HTTP_DECODE",
                format!("recall response was not valid JSON: {e}"),
            )
        })?;
        Ok(hits)
    }

    fn write(
        &self,
        _policy: &Policy,
        proposal: Proposal,
        max_bytes: usize,
    ) -> Result<MemoryRecord, PantheonError> {
        let req = WriteRequest {
            layer: layer_str(proposal.layer),
            namespace: proposal.namespace,
            key: proposal.key,
            value: proposal.value,
            provenance: proposal.provenance,
            max_bytes,
        };
        let body = serde_json::to_vec(&req).map_err(|e| {
            merr(
                "MEM_HTTP_ENCODE",
                format!("failed to encode write request: {e}"),
            )
        })?;
        let endpoint = self.endpoint("write");
        let resp = http_post(&endpoint, &body, self.api_key.as_deref())?;
        if resp.status >= 400 {
            let parsed: ErrorResponse = serde_json::from_slice(&resp.body).unwrap_or_default();
            let code = parsed
                .error
                .unwrap_or_else(|| format!("HTTP {}", resp.status));
            let cause = parsed
                .cause
                .unwrap_or_else(|| format!("HTTP {} from {}", resp.status, endpoint));
            return Err(merr(&code, cause));
        }
        let record: MemoryRecord = serde_json::from_slice(&resp.body).map_err(|e| {
            merr(
                "MEM_HTTP_DECODE",
                format!("write response was not valid JSON: {e}"),
            )
        })?;
        Ok(record)
    }

    fn list_agent(&self, namespace: &str) -> Result<Vec<(String, String)>, PantheonError> {
        let url = format!("{}?namespace={}", self.endpoint("list_agent"), pct(namespace));
        let resp = http_get(&url, self.api_key.as_deref())?;
        let rows: Vec<(String, String)> = serde_json::from_slice(&resp.body).map_err(|e| {
            merr(
                "MEM_HTTP_DECODE",
                format!("list_agent response was not valid JSON: {e}"),
            )
        })?;
        Ok(rows)
    }
}

/// Percent-encode a string for use in a URL query parameter.
fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

fn http_get(url: &str, api_key: Option<&str>) -> Result<HttpResponse, PantheonError> {
    let client = build_client(api_key)?;
    let resp = client
        .get(url)
        .send()
        .map_err(|e| merr("MEM_HTTP_CONN", format!("GET {url}: {e}")))?;
    let status = resp.status().as_u16();
    let body = resp
        .bytes()
        .map_err(|e| merr("MEM_HTTP_BODY", format!("GET {url}: {e}")))?
        .to_vec();
    if status >= 400 {
        let parsed: ErrorResponse = serde_json::from_slice(&body).unwrap_or_default();
        let code = parsed.error.unwrap_or_else(|| format!("HTTP {}", status));
        let cause = parsed
            .cause
            .unwrap_or_else(|| format!("GET {}: HTTP {}", url, status));
        return Err(merr(&code, cause));
    }
    Ok(HttpResponse { status, body })
}

fn http_post(
    url: &str,
    payload: &[u8],
    api_key: Option<&str>,
) -> Result<HttpResponse, PantheonError> {
    let client = build_client(api_key)?;
    let resp = client
        .post(url)
        .body(payload.to_vec())
        .send()
        .map_err(|e| merr("MEM_HTTP_CONN", format!("POST {url}: {e}")))?;
    let status = resp.status().as_u16();
    let body = resp
        .bytes()
        .map_err(|e| merr("MEM_HTTP_BODY", format!("POST {url}: {e}")))?
        .to_vec();
    if status >= 400 {
        let parsed: ErrorResponse = serde_json::from_slice(&body).unwrap_or_default();
        let code = parsed.error.unwrap_or_else(|| format!("HTTP {}", status));
        let cause = parsed
            .cause
            .unwrap_or_else(|| format!("POST {}: HTTP {}", url, status));
        return Err(merr(&code, cause));
    }
    Ok(HttpResponse { status, body })
}

fn build_client(api_key: Option<&str>) -> Result<reqwest::blocking::Client, PantheonError> {
    let mut builder = reqwest::blocking::Client::builder();
    if let Some(k) = api_key {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", k))
                .map_err(|e| merr("MEM_HTTP_AUTH", format!("invalid api key header: {e}")))?,
        );
        builder = builder.default_headers(headers);
    }
    builder
        .build()
        .map_err(|e| merr("MEM_HTTP_CLIENT", format!("building client: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pct_passes_through_unreserved() {
        assert_eq!(pct("abc"), "abc");
        assert_eq!(pct("hello world"), "hello%20world");
        assert_eq!(pct("a/b"), "a%2Fb");
    }

    #[test]
    fn layer_str_round_trips() {
        assert_eq!(layer_str(LayerKind::Global), "Global");
        assert_eq!(layer_str(LayerKind::Agent), "Agent");
        assert_eq!(layer_str(LayerKind::EphemeralTurn), "EphemeralTurn");
    }
}
