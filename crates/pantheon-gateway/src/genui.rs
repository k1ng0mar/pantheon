//! Generative-UI references: task id + signed URL, never embedded payloads.
//! A frame carries `GenUiRef`; the client fetches bytes from the URL.
//! Signing is std-only HMAC-SHA256 (compact local implementation) so no
//! new crates are needed. Secrets come from env at serve time.
use serde::{Deserialize, Serialize};
/// Reference to a generated artifact (image, file, card bundle).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenUiRef {
    pub task_id: String,
    pub url: String,
    pub expires_ms: i64,
    pub mime: String,
}
/// Signed URL handed to the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedUrl {
    pub url: String,
    pub expires_ms: i64,
}
/// Task ids are single URL path components. Keeping the alphabet explicit
/// prevents query/header injection and makes signed URLs portable across the
/// stdlib HTTP shim and external clients.
pub fn valid_task_id(task_id: &str) -> bool {
    !task_id.is_empty()
        && task_id.len() <= 128
        && task_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
// --- compact SHA-256 (FIPS 180-4), then HMAC over it. ~70 lines, std-only.
fn sha256(msg: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut buf = msg.to_vec();
    let bitlen = (msg.len() as u64).wrapping_mul(8);
    buf.push(0x80);
    while buf.len() % 64 != 56 {
        buf.push(0);
    }
    buf.extend_from_slice(&bitlen.to_be_bytes());
    for chunk in buf.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[4 * i],
                chunk[4 * i + 1],
                chunk[4 * i + 2],
                chunk[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    let mut out = [0u8; 32];
    for i in 0..8 {
        out[4 * i..4 * i + 4].copy_from_slice(&h[i].to_be_bytes());
    }
    out
}
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        let h = sha256(key);
        k[..32].copy_from_slice(&h);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; 64];
    let mut opad = [0x5cu8; 64];
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let mut inner = ipad.to_vec();
    inner.extend_from_slice(msg);
    let ih = sha256(&inner);
    let mut outer = opad.to_vec();
    outer.extend_from_slice(&ih);
    sha256(&outer)
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
/// Signer: mints and verifies task-scoped URLs.
#[derive(Debug, Clone)]
pub struct GenUiSigner {
    pub base_url: String,
    pub secret: Vec<u8>,
}
impl GenUiSigner {
    pub fn new(base_url: impl Into<String>, secret: Vec<u8>) -> Self {
        Self {
            base_url: base_url.into(),
            secret,
        }
    }
    /// Mint `base_url/<task_id>?exp=<ms>&sig=<hex>`. Payload stays server-side.
    pub fn sign(&self, task_id: &str, mime: &str, ttl_ms: i64) -> GenUiRef {
        let exp = now_ms() + ttl_ms;
        let msg = format!("{task_id}.{exp}");
        let sig = hex(&hmac_sha256(&self.secret, msg.as_bytes()));
        let base = self.base_url.trim_end_matches('/');
        GenUiRef {
            task_id: task_id.into(),
            mime: mime.into(),
            expires_ms: exp,
            url: format!("{base}/{task_id}?exp={exp}&sig={sig}"),
        }
    }
    /// Verify a minted URL. Checks task id, expiry, and signature (constant-time-ish).
    pub fn verify(&self, task_id: &str, expires_ms: i64, sig_hex: &str) -> bool {
        if !valid_task_id(task_id) || expires_ms < now_ms() {
            return false;
        }
        let msg = format!("{task_id}.{expires_ms}");
        let want = hex(&hmac_sha256(&self.secret, msg.as_bytes()));
        if want.len() != sig_hex.len() {
            return false;
        }
        let mut diff = 0u8;
        for (a, b) in want.bytes().zip(sig_hex.bytes()) {
            diff |= a ^ b;
        }
        diff == 0
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn task_ids_are_safe_path_components() {
        assert!(valid_task_id("task-1_A"));
        assert!(!valid_task_id("task/1"));
        assert!(!valid_task_id("task?x"));
        assert!(!valid_task_id(""));
    }

    #[test]
    fn sha256_matches_nist_vector() {
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
    #[test]
    fn round_trip_sign_verify() {
        let s = GenUiSigner::new("https://ui.example/blobs", b"test-secret".to_vec());
        let r = s.sign("task-1", "image/png", 60_000);
        assert!(r.url.contains("task-1"));
        let q = r.url.split('?').nth(1).unwrap();
        let mut exp = 0;
        let mut sig = String::new();
        for kv in q.split('&') {
            if let Some(v) = kv.strip_prefix("exp=") {
                exp = v.parse().unwrap();
            }
            if let Some(v) = kv.strip_prefix("sig=") {
                sig = v.into();
            }
        }
        assert!(s.verify("task-1", exp, &sig));
        assert!(!s.verify("task-1", exp, "00"));
        assert!(!s.verify("other", exp, &sig));
    }
    #[test]
    fn expired_urls_rejected() {
        let s = GenUiSigner::new("https://ui.example/blobs", b"s".to_vec());
        assert!(!s.verify("t", now_ms() - 1, "ab"));
    }
}
