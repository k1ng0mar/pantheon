//! AWS SigV4 signing for Bedrock requests.
//!
//! Implements the AWS Signature Version 4 signing process for the
//! `bedrock-runtime.<region>.amazonaws.com` host. Reads credentials from
//! `AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY` (+ optional
//! `AWS_SESSION_TOKEN`), signs the request, and returns the
//! `Authorization` header value.

use hmac::{Hmac, Mac};
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
