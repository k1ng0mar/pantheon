/* canonical hash for bundled plugins: build.rs hashes what gets embedded,
`pantheon_exec::plugins` hashes what seeding materialized. both must agree.

sort pairs by relative path, feed each as `rel bytes + 0x00 + content bytes
+ 0x00`, hex-encode the sha256.

NOTE: `include!`d by build.rs and `#[path]`-included by the crate, so no
inner (`//!`) doc comments here. */

pub fn canonical_sha256(files: &[(&str, &str)]) -> String {
    use sha2::{Digest, Sha256};
    let mut sorted: Vec<(&str, &str)> = files.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    let mut h = Sha256::new();
    for (rel, content) in sorted {
        h.update(rel.as_bytes());
        h.update([0u8]);
        h.update(content.as_bytes());
        h.update([0u8]);
    }
    let digest = h.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}
