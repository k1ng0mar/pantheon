/* Canonical content hash for bundled plugins, shared between
build.rs (hash of what gets embedded) and `pantheon_exec::plugins`
(hash of what seeding materialized on disk).

Canonical form: pairs sorted by relative path; each pair fed as
`rel bytes + 0x00 + content bytes + 0x00`. Hex-encoded sha256.

NOTE: this file is `include!`d by build.rs and `#[path]`-included by
the crate, so it must not start with inner (`//!`) doc comments. */

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
