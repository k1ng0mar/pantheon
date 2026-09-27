//! Embeddings auxiliary client: OpenAI-compatible `POST /embeddings`
//! plus a local fallback (`Embedder::Local` hashing embedder) so the
//! vector recall layer works without any configured provider.
//!
//! The auxiliary is resolved from `ModelPolicy.auxiliaries` under
//! `AuxiliaryKind::Embeddings` — same scoping rule as title-gen and
//! compression. Absent entry = local embedder, never chat.

use crate::http::{ChatTransport, HttpTransport, WireRequest};
use pantheon_api::error::{Layer, PantheonError};
use pantheon_api::model::{AuxiliaryKind, DefaultModel};
use pantheon_secrets::SecretValue;
use std::time::Duration;

pub const EMBED_TIMEOUT_SECS: u64 = 15;
/// Local fallback dimension (matches the hashing scheme below).
pub const LOCAL_DIM: usize = 256;

fn eerr(code: &str, cause: String) -> PantheonError {
    PantheonError::new(
        code,
        Layer::Provider,
        true,
        cause,
        "check the embeddings auxiliary config",
        "",
    )
}

/// Which backend serves embeddings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Embedder {
    /// Remote provider resolved through the auxiliary chain.
    Provider(DefaultModel),
    /// Local deterministic hashing embedder — no network, no key, works
    /// everywhere. Weaker semantics than a real embedding model, but it
    /// turns the vector layer on for every install by default.
    Local,
}

/// One text chunk turned into a fixed-dimension vector.
#[derive(Debug, Clone)]
pub struct Embedding {
    pub vec: Vec<f32>,
    pub dim: usize,
}

/// Trait the search layer depends on, so tests can swap transports.
pub trait EmbedderClient: Send + Sync {
    fn embed(&self, texts: &[String]) -> Result<Vec<Embedding>, PantheonError>;
    fn model_name(&self) -> &str;
}

pub struct EmbedClient {
    target: Option<DefaultModel>,
    transport: Box<dyn ChatTransport>,
    api_key: Option<SecretValue>,
}

impl EmbedClient {
    /// Build from the policy: `AuxiliaryKind::Embeddings` entry present ->
    /// provider-backed; absent -> local hashing embedder.
    pub fn from_policy(
        policy: &pantheon_api::model::ModelPolicy,
        api_key: Option<SecretValue>,
    ) -> Self {
        let target = policy
            .auxiliaries
            .iter()
            .find(|a| a.kind == AuxiliaryKind::Embeddings)
            .map(|a| DefaultModel {
                provider: a.provider.clone(),
                model: a.model.clone(),
            });
        Self {
            target,
            transport: Box::new(HttpTransport {
                timeout: Duration::from_secs(EMBED_TIMEOUT_SECS),
            }),
            api_key,
        }
    }

    pub fn local() -> Self {
        Self {
            target: None,
            transport: Box::new(HttpTransport::default()),
            api_key: None,
        }
    }

    /// Test seam: replay canned responses through any transport.
    pub fn with_transport(mut self, transport: Box<dyn ChatTransport>) -> Self {
        self.transport = transport;
        self
    }

    fn which(&self) -> Embedder {
        match &self.target {
            Some(m) => Embedder::Provider(m.clone()),
            None => Embedder::Local,
        }
    }

    /// Parse an OpenAI-shape embeddings response (`data[i].embedding`).
    /// Pure: unit-tested directly, no transport involved.
    fn parse_embeddings_response(
        resp: &str,
        expected: usize,
    ) -> Result<Vec<Embedding>, PantheonError> {
        let v: serde_json::Value =
            serde_json::from_str(resp).map_err(|e| eerr("EMBED_PARSE", e.to_string()))?;
        let data = v.get("data").and_then(|d| d.as_array()).ok_or_else(|| {
            eerr(
                "EMBED_SHAPE",
                format!(
                    "no data array in response: {}",
                    &resp[..resp.len().min(200)]
                ),
            )
        })?;
        if data.len() != expected {
            return Err(eerr(
                "EMBED_COUNT",
                format!("asked for {expected} embeddings, got {}", data.len()),
            ));
        }
        data.iter()
            .map(|d| {
                let arr = d
                    .get("embedding")
                    .and_then(|e| e.as_array())
                    .ok_or_else(|| eerr("EMBED_SHAPE", "no embedding array".into()))?;
                let vec: Vec<f32> = arr
                    .iter()
                    .filter_map(|x| x.as_f64().map(|f| f as f32))
                    .collect();
                if vec.is_empty() {
                    return Err(eerr("EMBED_SHAPE", "empty embedding vector".into()));
                }
                Ok(Embedding {
                    dim: vec.len(),
                    vec,
                })
            })
            .collect()
    }

    /// POST /embeddings (OpenAI shape) and parse `data[i].embedding`.
    fn embed_remote(
        &self,
        model: &DefaultModel,
        texts: &[String],
    ) -> Result<Vec<Embedding>, PantheonError> {
        let base = crate::http::resolve_base(&model.provider).map_err(|e| {
            eerr(
                "PROVIDER_CONFIG",
                format!(
                    "{} — run `pantheon model` to fill the provider's required values",
                    e.cause
                ),
            )
        })?;
        let configured = self.api_key.as_ref().map(|k| k.expose()).unwrap_or("");
        let key = crate::catalog::key_for(&model.provider, configured);
        let body = serde_json::json!({ "model": model.model, "input": texts });
        let req = WireRequest {
            url: format!("{}/embeddings", base.trim_end_matches('/')),
            headers: vec![
                ("Content-Type".into(), "application/json".into()),
                ("Authorization".into(), format!("Bearer {key}")),
            ],
            body: serde_json::to_string(&body).map_err(|e| eerr("EMBED_SER", e.to_string()))?,
        };
        let resp = self.transport.post(&req)?;
        Self::parse_embeddings_response(&resp, texts.len())
    }
}

impl EmbedderClient for EmbedClient {
    fn model_name(&self) -> &str {
        match &self.target {
            Some(m) => &m.model,
            None => "local-hash",
        }
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Embedding>, PantheonError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        match self.which() {
            Embedder::Provider(m) => self.embed_remote(&m, texts),
            Embedder::Local => Ok(texts.iter().map(|t| local_embed(t)).collect()),
        }
    }
}

/// Deterministic local embedding: hashed character n-grams into a
/// fixed-dimension L2-normalised vector. Zero network, zero key,
/// stable across runs. Semantics are bag-of-features, not deep — good
/// enough to make the vector layer real for recall, and it is replaced
/// wholesale the moment an embeddings auxiliary is configured.
fn local_embed(text: &str) -> Embedding {
    let mut vec = vec![0f32; LOCAL_DIM];
    let lower = text.to_lowercase();
    let bytes: Vec<char> = lower.chars().collect();
    // Character trigrams over words, hashed into buckets.
    for w in lower.split_whitespace() {
        let wc: Vec<char> = w.chars().collect();
        if wc.len() < 3 {
            // Short token: hash the whole token.
            let h = fx_hash(&wc.iter().map(|c| *c as u32).collect::<Vec<_>>());
            vec[(h % LOCAL_DIM as u32) as usize] += 1.0;
            continue;
        }
        for i in 0..=wc.len() - 3 {
            let tri: Vec<u32> = wc[i..i + 3].iter().map(|c| *c as u32).collect();
            let h = fx_hash(&tri);
            vec[(h % LOCAL_DIM as u32) as usize] += 1.0;
        }
    }
    // L2 normalise.
    let norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in vec.iter_mut() {
            *x /= norm;
        }
    }
    let _ = &bytes;
    Embedding {
        dim: LOCAL_DIM,
        vec,
    }
}

/// FNV-1a-ish 32-bit hash over u32 symbols.
fn fx_hash(syms: &[u32]) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for s in syms {
        h ^= *s;
        h = h.wrapping_mul(0x01000193);
    }
    h
}

/// Cosine similarity between two equal-dimension vectors.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na * nb)
}

#[cfg(test)]
#[path = "embeddings_tests.rs"]
mod tests;
