//! Inbound webhook triggers (§21).
//!
//! A webhook job does not fire on a clock — a surface calls in. So these jobs
//! are deliberately never "due": the trigger path identifies the job, and the
//! caller's request id is the occurrence. A caller that retries a delivery
//! (which webhook senders do on any non-2xx) collapses onto the run already
//! claimed for that request instead of starting a second one.
//!
//! ## Signature contract
//!
//! Every inbound webhook request must carry an HMAC-SHA256 signature over
//! the **raw request body bytes**, keyed by a shared secret. Without it,
//! anyone who can reach the endpoint can inject job triggers.
//!
//! ```text
//! X-Pantheon-Signature: sha256=<lowercase hex of HMAC-SHA256(secret, body)>
//! ```
//!
//! - The secret comes from the `PANTHEON_WEBHOOK_SECRET` environment variable
//!   (see [`WebhookAuth::from_env`]). It must be non-empty; an unset or empty
//!   secret means the webhook surface must not be served at all —
//!   [`accept`] takes `&WebhookAuth`, so there is no unsigned code path.
//! - The scheme prefix is the literal `sha256=`; anything else is malformed.
//! - The hex digest is compared in constant time
//!   ([`hmac::Mac::verify_slice`]).
//! - Verification runs **before** routing, so a bad signature reveals
//!   nothing about which paths exist.
//!
//! Rejection mapping for the HTTP surface ([`WebhookReject::http_status`]):
//!
//! | reject | status | meaning |
//! |---|---|---|
//! | `Unauthorized` | 401 | missing, malformed, or mismatched signature |
//! | `NoRoute` / `Paused` / `NotWebhookJob` | 404 | no such triggerable job (paused jobs 404 so their existence is not leaked) |
//! | `AlreadyClaimed` | 200 | retried delivery; the run was already claimed, do not start another |

use crate::tick::{occurrence_key, ClaimLedger};
use crate::{Job, ScheduleKind};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// The request header carrying the signature.
pub const SIGNATURE_HEADER: &str = "X-Pantheon-Signature";
/// Environment variable holding the shared webhook secret.
pub const SECRET_ENV_VAR: &str = "PANTHEON_WEBHOOK_SECRET";
/// Scheme prefix of the signature header value.
const SIGNATURE_SCHEME: &str = "sha256=";

/// The shared secret that authenticates inbound webhook requests.
///
/// Constructing one requires a non-empty secret, so an unset
/// `PANTHEON_WEBHOOK_SECRET` cannot silently downgrade to "no verification".
#[derive(Debug, Clone)]
pub struct WebhookAuth {
    secret: Vec<u8>,
}

impl WebhookAuth {
    /// Build from raw secret bytes. Returns `None` when the secret is empty —
    /// an empty secret authenticates nothing.
    pub fn new(secret: impl AsRef<[u8]>) -> Option<Self> {
        let secret = secret.as_ref().to_vec();
        if secret.is_empty() {
            return None;
        }
        Some(Self { secret })
    }

    /// Read the secret from [`SECRET_ENV_VAR`]. Returns `None` when the
    /// variable is unset or empty; the caller must then refuse to serve
    /// webhook endpoints rather than serving them unsigned.
    pub fn from_env() -> Option<Self> {
        let raw = std::env::var(SECRET_ENV_VAR).ok()?;
        Self::new(raw)
    }

    fn key(&self) -> &[u8] {
        &self.secret
    }
}

/// Why a signature was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureError {
    /// No `X-Pantheon-Signature` header was present.
    Missing,
    /// The header was present but not `sha256=<64 hex chars>`.
    Malformed,
    /// The MAC did not match the body.
    Mismatch,
}

impl std::fmt::Display for SignatureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignatureError::Missing => write!(f, "missing {SIGNATURE_HEADER} header"),
            SignatureError::Malformed => write!(
                f,
                "malformed signature header (want `{SIGNATURE_SCHEME}<hex>`)"
            ),
            SignatureError::Mismatch => write!(f, "signature does not match the request body"),
        }
    }
}

/// Mint a signature header value for `body` (what a sender puts in
/// `X-Pantheon-Signature`). Returns `sha256=<hex>`.
pub fn sign(secret: &[u8], body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body);
    let digest = mac.finalize().into_bytes();
    let mut s = String::with_capacity(SIGNATURE_SCHEME.len() + 64);
    s.push_str(SIGNATURE_SCHEME);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Verify the `X-Pantheon-Signature` header against the raw request body.
///
/// The comparison is constant-time via [`hmac::Mac::verify_slice`]; a wrong
/// length or non-hex digest fails closed as [`SignatureError::Malformed`]
/// rather than reaching the MAC.
pub fn verify_signature(
    secret: &[u8],
    body: &[u8],
    header: Option<&str>,
) -> Result<(), SignatureError> {
    let header = header.ok_or(SignatureError::Missing)?;
    let hex = header
        .strip_prefix(SIGNATURE_SCHEME)
        .ok_or(SignatureError::Malformed)?;
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(SignatureError::Malformed);
    }
    let mut expected = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16).unwrap();
        let lo = (chunk[1] as char).to_digit(16).unwrap();
        expected[i] = (hi as u8) * 16 + lo as u8;
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body);
    mac.verify_slice(&expected)
        .map_err(|_| SignatureError::Mismatch)
}

/// Why an inbound webhook request was not turned into a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebhookReject {
    /// The signature was missing, malformed, or did not match the body.
    Unauthorized(SignatureError),
    /// No webhook job is registered for the request path.
    NoRoute,
    /// The job exists but is paused.
    Paused,
    /// The matched job is not a webhook job (unreachable via [`route`]).
    NotWebhookJob,
    /// The request id was already claimed: a retried delivery.
    AlreadyClaimed,
}

impl WebhookReject {
    /// The HTTP status the serving surface should return.
    pub fn http_status(&self) -> u16 {
        match self {
            WebhookReject::Unauthorized(_) => 401,
            WebhookReject::NoRoute | WebhookReject::Paused | WebhookReject::NotWebhookJob => 404,
            // The sender's retry collapsed onto the already-claimed run: 2xx
            // so it stops retrying.
            WebhookReject::AlreadyClaimed => 200,
        }
    }
}

impl std::fmt::Display for WebhookReject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebhookReject::Unauthorized(e) => write!(f, "unauthorized webhook request: {e}"),
            WebhookReject::NoRoute => write!(f, "no webhook job for this path"),
            WebhookReject::Paused => write!(f, "webhook job is paused"),
            WebhookReject::NotWebhookJob => write!(f, "job is not a webhook job"),
            WebhookReject::AlreadyClaimed => write!(f, "request id already claimed"),
        }
    }
}

/// An accepted webhook request: the caller should enqueue this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fire {
    pub job_id: String,
    /// Key for the idempotency ledger, derived from the request id.
    pub occurrence_key: String,
    pub agent: String,
}

/// Compare a configured path with a request path, ignoring surrounding and
/// duplicate slashes so `/hook/nyx`, `hook/nyx` and `//hook/nyx/` agree.
fn same_path(configured: &str, requested: &str) -> bool {
    fn normalize(p: &str) -> Vec<&str> {
        p.split('/').filter(|s| !s.is_empty()).collect()
    }
    normalize(configured) == normalize(requested)
}

/// Which job a request path belongs to. First match wins, so a duplicate
/// path registered twice is resolved deterministically by registration order.
pub fn route<'a>(jobs: &'a [Job], path: &str) -> Option<&'a Job> {
    jobs.iter().find(|job| match &job.kind {
        ScheduleKind::Webhook { path: configured } => same_path(configured, path),
        _ => false,
    })
}

/// Accept an inbound webhook request for one job.
///
/// The signature is verified **first**, before routing or claiming, so a bad
/// signature reveals nothing about which paths exist and never triggers a
/// run. `body` is the raw request body the signature covers; `signature` is
/// the `X-Pantheon-Signature` header value (or `None` when absent).
///
/// There is no unsigned path: `auth` is required, and [`WebhookAuth`] cannot
/// be built from an empty secret. When the secret is not configured, the
/// surface must not serve webhook endpoints at all.
pub fn accept(
    job: &Job,
    path: &str,
    request_id: &str,
    body: &[u8],
    signature: Option<&str>,
    auth: &WebhookAuth,
    ledger: &mut ClaimLedger,
) -> Result<Fire, WebhookReject> {
    verify_signature(auth.key(), body, signature).map_err(WebhookReject::Unauthorized)?;

    let configured = match &job.kind {
        ScheduleKind::Webhook { path } => path,
        _ => return Err(WebhookReject::NotWebhookJob),
    };
    if job.paused {
        return Err(WebhookReject::Paused);
    }
    if !same_path(configured, path) {
        return Err(WebhookReject::NoRoute);
    }
    let key = occurrence_key(job, request_id);
    if !ledger.claim(&key) {
        return Err(WebhookReject::AlreadyClaimed);
    }
    Ok(Fire {
        job_id: job.id.clone(),
        occurrence_key: key,
        agent: job.agent.clone(),
    })
}
