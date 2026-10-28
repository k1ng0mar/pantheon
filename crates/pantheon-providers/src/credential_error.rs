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
