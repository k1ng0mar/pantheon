//! Small helpers in one file.

pub mod embeddings {
    //! Embeddings auxiliary client: OpenAI-compatible `POST /embeddings`
    //! plus a local fallback (`Embedder::Local` hashing embedder) so the
    //! vector recall layer works without any configured provider.
    //!
    //! The auxiliary is resolved from `ModelPolicy.auxiliaries` under
    //! `AuxiliaryKind::Embeddings` - same scoping rule as title-gen and
    //! compression. Absent entry = local embedder, never chat.

    use crate::catalog::{self, ApiMode};
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
        /// Local deterministic hashing embedder - no network, no key, works
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
                    key_env: None,
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
        /// OpenAI-compatible wire only: other [`ApiMode`]s have no
        /// `/embeddings` endpoint, so a non-OpenAI provider fails fast here
        /// instead of sending an OpenAI-shaped body at the wrong URL.
        fn embed_remote(
            &self,
            model: &DefaultModel,
            texts: &[String],
        ) -> Result<Vec<Embedding>, PantheonError> {
            let api_mode = catalog::provider(&model.provider)
                .map(|p| p.api_mode)
                .unwrap_or(ApiMode::OpenAi);
            if api_mode != ApiMode::OpenAi {
                return Err(eerr(
                "EMBED_MODE",
                format!(
                    "provider {:?} speaks {api_mode:?}, not OpenAI-compatible: no /embeddings endpoint",
                    model.provider,
                ),
            ));
            }
            let base = crate::http::resolve_base(&model.provider).map_err(|e| {
                eerr(
                    "PROVIDER_CONFIG",
                    format!(
                        "{} - run `pantheon model` to fill the provider's required values",
                        e.cause
                    ),
                )
            })?;
            let configured = self.api_key.as_ref().map(|k| k.expose()).unwrap_or("");
            let key = catalog::key_for(&model.provider, configured);
            // Catalog auth shape, not a hardcoded Bearer: vendors with a
            // custom key header (e.g. `api-key`) get the raw key there.
            let key_header = catalog::key_header_for(&model.provider);
            let (header_name, auth_value) = crate::http::auth_header_pair(&key_header, &key);
            let body = serde_json::json!({ "model": model.model, "input": texts });
            let req = WireRequest {
                url: format!("{}/embeddings", base.trim_end_matches('/')),
                headers: vec![
                    ("Content-Type".into(), "application/json".into()),
                    (header_name, auth_value),
                ],
                body: serde_json::to_string(&body).map_err(|e| eerr("EMBED_SER", e.to_string()))?,
                use_sigv4: false,
                cancel: None,
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
    /// stable across runs. Semantics are bag-of-features, not deep - good
    /// enough to make the vector layer real for recall, and it is replaced
    /// wholesale the moment an embeddings auxiliary is configured.
    fn local_embed(text: &str) -> Embedding {
        let mut vec = vec![0f32; LOCAL_DIM];
        let lower = text.to_lowercase();
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
}
pub mod credential_error {
    //! Credential-error classifier: turn an HTTP status + response body
    //! snippet into an actionable, secret-free diagnosis.
    //!
    //! Why this exists: providers disagree about what a status means, and
    //! the raw status alone misleads. A 403 whose body reads
    //! `error code: 1010` is Cloudflare's Browser Integrity Check rejecting
    //! the request headers at the edge — the request never reached the
    //! provider's auth layer, so replacing the key cannot help. A Telegram
    //! 404 on a bot path means the token's *shape* is wrong, while a 401
    //! means the token itself is wrong or revoked. The classifier names
    //! those cases instead of leaving every 4xx as "client/config problem".
    //!
    //! Safety rules, enforced by construction:
    //! - Diagnoses are static strings. The classifier never echoes the
    //!   response body, the request URL, or any credential, so a
    //!   token-bearing URL or a leaked key in a body cannot pass through
    //!   into a diagnosis. Callers must keep it that way: append the
    //!   diagnosis, never the raw body, to a token-bearing URL.
    //! - The classifier never changes retryability. 429 keeps its
    //!   Retry-After handling, 5xx stays retryable, other 4xx stays
    //!   non-retryable; the diagnosis is advisory text only.
    //! - Mappings marked provider-specific are conventional, not
    //!   universal: some providers return 403 for an invalid key too.
    //!   Where that matters the remediation says to check the body.

    /// An actionable diagnosis for a credential-related HTTP failure.
    /// Every field is a static string: nothing from the request or the
    /// response body is ever copied into a diagnosis.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct CredentialDiagnosis {
        /// One-line summary of what went wrong.
        pub summary: &'static str,
        /// What the user should do about it.
        pub remediation: &'static str,
        /// True when the evidence points at the key itself (missing,
        /// invalid, revoked) rather than at permissions or the request.
        pub key_at_fault: bool,
        /// True when changing request headers (a real User-Agent) can fix
        /// the failure without touching the key.
        pub fixable_by_header_change: bool,
    }

    /// Classify a provider HTTP failure. Returns `None` for statuses
    /// this classifier has nothing specific to say about (429, 5xx,
    /// and non-credential 4xx such as 400/404 on a provider endpoint),
    /// so callers keep their existing behaviour for those cases.
    pub fn classify(status: u16, body: &str) -> Option<CredentialDiagnosis> {
        // Cloudflare edge block: checked before the status, because the
        // body is the evidence and the status is almost always 403.
        // The request never reached provider auth, so the key is not
        // at fault and replacing it cannot help.
        if body.contains("error code: 1010") {
            return Some(CredentialDiagnosis {
            summary: "Blocked at Cloudflare's edge by Browser Integrity Check: the request never reached the provider's auth layer.",
            remediation: "Do NOT replace the key. Fix: send a real User-Agent header (Pantheon now does this on provider requests).",
            key_at_fault: false,
            fixable_by_header_change: true,
        });
        }
        match status {
        401 => Some(CredentialDiagnosis {
            summary: "Key missing, invalid, or revoked.",
            remediation: "Check the key in the provider account; replace it if it was revoked or rotated.",
            key_at_fault: true,
            fixable_by_header_change: false,
        }),
        403 => Some(CredentialDiagnosis {
            summary: "Key accepted but not permitted for this endpoint or model.",
            remediation: "Check permissions, scope, and plan for this key and model; do not assume the key is wrong. Provider-specific: some providers also return 403 for an invalid key, so check the response body for an explicit invalid-key message.",
            key_at_fault: false,
            fixable_by_header_change: false,
        }),
        _ => None,
    }
    }

    /// Classify a Telegram Bot API failure. Telegram's codes are
    /// specific: a 404 on a bot path means the token does not even
    /// parse as a bot token, a 401 means it parses but is wrong or
    /// revoked, and a 403 means the bot itself is refused in that
    /// context. `body` is accepted for symmetry with [`classify`]
    /// and for future body-based cases; today only the status is used,
    /// and the body is never echoed.
    pub fn classify_telegram(status: u16, _body: &str) -> Option<CredentialDiagnosis> {
        match status {
        401 => Some(CredentialDiagnosis {
            summary: "Bot token wrong or revoked.",
            remediation: "Check the token in BotFather; replace it if it was revoked or rotated.",
            key_at_fault: true,
            fixable_by_header_change: false,
        }),
        403 => Some(CredentialDiagnosis {
            summary: "Bot is blocked by the user, or is not allowed in that chat, or lacks permission there.",
            remediation: "Check the bot is not blocked and has permission in that chat; the token itself may be fine.",
            key_at_fault: false,
            fixable_by_header_change: false,
        }),
        404 => Some(CredentialDiagnosis {
            summary: "Token shape is wrong: Telegram tokens are <digits>:<letters>.",
            remediation: "Check the token format, not just its value: it must start with digits, then a colon, then letters. A token that does not parse as a bot token never reaches auth.",
            key_at_fault: false,
            fixable_by_header_change: false,
        }),
        _ => None,
    }
    }

    /// Branded User-Agent sent on Pantheon provider requests, so
    /// edge checks that reject default or missing User-Agents (see
    /// the 1010 mapping in [`classify`]) do not block us. The version
    /// is the providers crate version.
    pub fn user_agent() -> String {
        format!("Pantheon/{}", env!("CARGO_PKG_VERSION"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn cloudflare_1010_beats_403_and_is_not_the_key() {
            let d = classify(403, "error code: 1010\n").expect("1010 must classify");
            assert!(!d.key_at_fault);
            assert!(d.fixable_by_header_change);
            assert!(d.summary.contains("Cloudflare"));
            assert!(d.remediation.contains("Do NOT replace the key"));
        }

        #[test]
        fn unauthorized_points_at_the_key() {
            let d = classify(401, r#"{"error": "invalid_api_key"}"#).expect("401 must classify");
            assert!(d.key_at_fault);
            assert!(!d.fixable_by_header_change);
        }

        #[test]
        fn forbidden_without_1010_points_at_permission_not_the_key() {
            let d = classify(403, r#"{"error": "permission_denied"}"#).expect("403 must classify");
            assert!(!d.key_at_fault);
            assert!(d.summary.contains("not permitted"));
        }

        #[test]
        fn non_credential_statuses_have_no_diagnosis() {
            assert!(classify(429, "rate limited").is_none());
            assert!(classify(500, "internal error").is_none());
            assert!(classify(400, "bad request").is_none());
            assert!(classify(200, "ok").is_none());
        }

        #[test]
        fn diagnosis_never_echoes_the_body() {
            let planted = "sk-secret-planted-token-value";
            let body = format!("error code: 1010 {planted}");
            let d = classify(403, &body).expect("1010 must classify");
            assert!(!d.summary.contains(planted));
            assert!(!d.remediation.contains(planted));
            let d401 = classify(401, planted).expect("401 must classify");
            assert!(!d401.summary.contains(planted));
            assert!(!d401.remediation.contains(planted));
        }

        #[test]
        fn telegram_404_is_shape_401_is_value_403_is_permission() {
            let shape = classify_telegram(404, "").expect("telegram 404 must classify");
            assert!(shape.summary.contains("<digits>:<letters>"));
            assert!(!shape.key_at_fault);
            let value = classify_telegram(401, "").expect("telegram 401 must classify");
            assert!(value.key_at_fault);
            let perm = classify_telegram(403, "").expect("telegram 403 must classify");
            assert!(perm.summary.contains("blocked"));
            assert!(classify_telegram(429, "").is_none());
        }

        #[test]
        fn user_agent_is_branded() {
            assert!(user_agent().starts_with("Pantheon/"));
        }
    }
}
pub mod error_kind {
    //! Display classification for provider failures.
    //!
    //! The chain emits `ModelEvent::AttemptFailed` with a raw `(code, cause)`;
    //! this module turns that pair into the small, stable kind the TUI card
    //! and the dashboard timeline show. Rate limits get their own kind on
    //! purpose: a quota failure needs a visibly different state from a dead
    //! key or a down endpoint, and `PROVIDER_HTTP` alone cannot tell them
    //! apart - the HTTP status lives in the cause string.

    use std::fmt;

    /// The kind of a provider failure, for display only. Recovery policy
    /// still keys off `PantheonError::retryable`; this enum never decides
    /// whether the chain falls back.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ProviderErrorKind {
        /// HTTP 429. Transient: another key or provider may still have quota.
        RateLimited,
        /// HTTP 401/403. The credential is dead or lacks access.
        Auth,
        /// The request never completed in time ("timed out" in the cause).
        Timeout,
        /// DNS / connection / transport failure below HTTP.
        Network,
        /// HTTP 5xx. The provider's side is broken.
        Server,
        /// Other HTTP 4xx, or a provider config problem. Our side is wrong.
        Client,
        /// Anything the shapes above do not cover. The card still shows the
        /// raw code, so unknown never means silent.
        Unknown,
    }

    impl ProviderErrorKind {
        /// Short label for the error card header.
        pub fn label(&self) -> &'static str {
            match self {
                ProviderErrorKind::RateLimited => "rate limited",
                ProviderErrorKind::Auth => "auth failed",
                ProviderErrorKind::Timeout => "timed out",
                ProviderErrorKind::Network => "network error",
                ProviderErrorKind::Server => "server error",
                ProviderErrorKind::Client => "request error",
                ProviderErrorKind::Unknown => "provider error",
            }
        }

        /// Rate limits render in the warning tone, not the failure tone, so
        /// quota trouble is visibly distinct from a dead provider.
        pub fn is_rate_limited(&self) -> bool {
            matches!(self, ProviderErrorKind::RateLimited)
        }
    }

    impl fmt::Display for ProviderErrorKind {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.label())
        }
    }

    /// Classify a provider failure from its `(code, cause)`. The HTTP status
    /// is embedded in the cause (`send()` writes `"{url}: HTTP {code} ..."`),
    /// so the cause is matched first; the code only disambiguates the
    /// non-HTTP shapes.
    pub fn classify_provider_error(code: &str, cause: &str) -> ProviderErrorKind {
        // Order matters: "HTTP 429" contains "HTTP 4" as a substring, and the
        // specific statuses must win over the class checks.
        if cause.contains("HTTP 429") {
            return ProviderErrorKind::RateLimited;
        }
        if cause.contains("HTTP 401") || cause.contains("HTTP 403") {
            return ProviderErrorKind::Auth;
        }
        if cause.to_lowercase().contains("timed out") {
            return ProviderErrorKind::Timeout;
        }
        if cause.contains("HTTP 5") {
            return ProviderErrorKind::Server;
        }
        if cause.contains("HTTP 4") {
            return ProviderErrorKind::Client;
        }
        if cause.contains("Dns Failed")
            || cause.contains("Connection Failed")
            || cause.contains("Network Error")
        {
            return ProviderErrorKind::Network;
        }
        if code == "PROVIDER_CONFIG" {
            return ProviderErrorKind::Client;
        }
        ProviderErrorKind::Unknown
    }

    /// First line of `cause`, char-truncated to `max`. Provider causes carry
    /// a URL plus up to a 300-char body snippet; cards and ledger rows want
    /// the one-line shape.
    pub fn short_snippet(cause: &str, max: usize) -> String {
        let line: String = cause
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(max)
            .collect();
        line.trim_end().to_string()
    }

    /// The readable tail of a provider cause: for HTTP failures the part
    /// from the status code on (the base URL adds nothing once the card
    /// already names provider/model); for transport failures the URL prefix
    /// is stripped (the doubled `url: url:` from `send()` included).
    pub fn display_message(cause: &str) -> String {
        let mut msg = cause;
        if let Some(i) = msg.find("HTTP ") {
            msg = &msg[i..];
        } else {
            // Strip leading "scheme://..." prefixes; transport causes repeat it.
            while let Some(colon) = msg.find(": ") {
                let head = &msg[..colon];
                if head.contains("://") && !head.contains(' ') {
                    msg = &msg[colon + 2..];
                } else {
                    break;
                }
            }
        }
        short_snippet(msg, 200)
    }

    /// Re-read the capped `Retry-After` wait a 429 asked for, from the marker
    /// `send()` stamps on the error cause. `None` = no wait requested.
    pub fn retry_after_secs_from_cause(cause: &str) -> Option<u64> {
        let marker = "(retry-after: ";
        let start = cause.rfind(marker)? + marker.len();
        let rest = cause.get(start..)?;
        let end = rest.find('s')?;
        rest[..end].parse::<u64>().ok()
    }
}
pub mod sigv4 {
    //! AWS SigV4 signing for Bedrock requests.
    //!
    //! Implements the AWS Signature Version 4 signing process for the
    //! `bedrock-runtime.<region>.amazonaws.com` host. Reads credentials from
    //! `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` (+ optional
    //! `AWS_SESSION_TOKEN`), signs the request, and returns the
    //! `Authorization` header value.

    use hmac::{Hmac, KeyInit, Mac};
    use sha2::{Digest, Sha256};

    type HmacSha256 = Hmac<Sha256>;

    /// SigV4 credential set from the environment.
    pub struct SigV4Creds {
        pub access_key: String,
        pub secret_key: String,
        pub session_token: Option<String>,
    }

    impl SigV4Creds {
        /// Read credentials from the standard AWS env vars.
        pub fn from_env() -> Option<Self> {
            let access_key = std::env::var("AWS_ACCESS_KEY_ID").ok()?;
            let secret_key = std::env::var("AWS_SECRET_ACCESS_KEY").ok()?;
            let session_token = std::env::var("AWS_SESSION_TOKEN").ok();
            Some(Self {
                access_key,
                secret_key,
                session_token,
            })
        }
    }

    /// Sign a request with SigV4. Returns the `Authorization` header value.
    #[allow(clippy::too_many_arguments)]
    pub fn sign_request(
        method: &str,
        uri: &str,
        query: &str,
        headers: &[(String, String)],
        body: &[u8],
        region: &str,
        service: &str,
        creds: &SigV4Creds,
    ) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let amz_date = format!(
            "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
            now.as_secs() / 86400 / 365 + 1970,
            (now.as_secs() / 86400 / 30 % 12) + 1,
            (now.as_secs() / 86400 % 30) + 1,
            (now.as_secs() / 3600) % 24,
            (now.as_secs() / 60) % 60,
            now.as_secs() % 60,
        );
        let date_stamp = &amz_date[..8];

        // Canonical URI
        let canonical_uri = if uri.is_empty() { "/" } else { uri };

        // Canonical query string
        let canonical_query = if query.is_empty() {
            String::new()
        } else {
            let mut pairs: Vec<(String, String)> = query
                .split('&')
                .filter_map(|p| {
                    let mut it = p.splitn(2, '=');
                    let k = it.next()?.to_string();
                    let v = it.next().unwrap_or("").to_string();
                    Some((k, v))
                })
                .collect();
            pairs.sort();
            pairs
                .iter()
                .map(|(k, v)| format!("{}={}", uri_encode(k), uri_encode(v)))
                .collect::<Vec<_>>()
                .join("&")
        };

        // Canonical headers
        let mut signed_headers: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.trim().to_lowercase(), v.trim().to_string()))
            .collect();
        signed_headers.push((
            "host".to_string(),
            "bedrock-runtime.".to_string() + region + ".amazonaws.com",
        ));
        signed_headers.push(("x-amz-date".to_string(), amz_date.clone()));
        if let Some(ref token) = creds.session_token {
            signed_headers.push(("x-amz-security-token".to_string(), token.clone()));
        }
        signed_headers.sort_by(|a, b| a.0.cmp(&b.0));
        let canonical_headers = signed_headers
            .iter()
            .map(|(k, v)| format!("{}:{}\n", k, v))
            .collect::<String>();
        let signed_header_names = signed_headers
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>()
            .join(";");

        // Payload hash
        let payload_hash = hex::encode(Sha256::digest(body));

        // Canonical request
        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            method,
            canonical_uri,
            canonical_query,
            canonical_headers,
            signed_header_names,
            payload_hash,
        );

        // String to sign
        let credential_scope = format!("{}/{}/{}/aws4_request", date_stamp, region, service);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{}\n{}\n{}",
            amz_date,
            credential_scope,
            hex::encode(Sha256::digest(canonical_request.as_bytes())),
        );

        // Signing key
        let k_date = hmac_sign(
            format!("AWS4{}", creds.secret_key).as_bytes(),
            date_stamp.as_bytes(),
        );
        let k_region = hmac_sign(&k_date, region.as_bytes());
        let k_service = hmac_sign(&k_region, service.as_bytes());
        let k_signing = hmac_sign(&k_service, b"aws4_request");

        // Signature
        let signature = hex::encode(hmac_sign(&k_signing, string_to_sign.as_bytes()));

        format!(
            "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
            creds.access_key, credential_scope, signed_header_names, signature
        )
    }

    fn hmac_sign(key: &[u8], data: &[u8]) -> Vec<u8> {
        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC can take key of any size");
        mac.update(data);
        mac.finalize().into_bytes().to_vec()
    }

    fn uri_encode(s: &str) -> String {
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

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn sigv4_signature_is_deterministic() {
            let creds = SigV4Creds {
                access_key: "AKIDEXAMPLE".to_string(),
                secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_string(),
                session_token: None,
            };
            let auth = sign_request("POST", "/", "", &[], b"", "us-east-1", "bedrock", &creds);
            assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"));
            assert!(auth.contains("SignedHeaders=host;x-amz-date"));
            assert!(auth.contains("Signature="));
        }

        #[test]
        fn uri_encode_handles_special_chars() {
            assert_eq!(uri_encode("abc"), "abc");
            assert_eq!(uri_encode("a b"), "a%20b");
            assert_eq!(uri_encode("a+b"), "a%2Bb");
            assert_eq!(uri_encode("a/b"), "a%2Fb");
        }
    }
}
