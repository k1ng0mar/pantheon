//! Inline image support: detection, dimensions, Kitty/Sixel emission.
//!
//! Kept in `/eval` per the test-location policy: behavioral rendering tests.

use pantheon_tui::richtext::{
    encode_sixel, find_images, image_dimensions, kitty_delete, kitty_image_payload, png_to_rgb,
};
use std::io::Write;

/// A real 1×1 transparent PNG (67 bytes): exercises the PNG header parser,
/// the zlib inflate path, and the Sixel encoder end to end.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

fn write_png(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(TINY_PNG).unwrap();
    path
}

#[test]
fn detects_at_and_markdown_and_bare_paths() {
    let dir = std::env::temp_dir().join(format!("pantheon-img-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = write_png(&dir, "shot.png");
    let s = path.to_string_lossy();

    let found = find_images(&format!("see @{s} for details"));
    assert_eq!(found.len(), 1, "@path");
    assert_eq!(found[0].name, "shot.png");

    let found = find_images(&format!("see ![alt]({s}) here"));
    assert_eq!(found.len(), 1, "markdown");

    let found = find_images(&format!("see {s} here"));
    assert_eq!(found.len(), 1, "bare token");

    // Missing files are not images; duplicates collapse.
    let found = find_images(&format!("{s} and {s} and /nope/missing.png"));
    assert_eq!(found.len(), 1, "dedupe, got {}", found.len());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn png_dimensions_read() {
    let dir = std::env::temp_dir().join(format!("pantheon-dim-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = write_png(&dir, "tiny.png");
    assert_eq!(
        image_dimensions(&path),
        Some((Some(1), Some(1))),
        "1x1 PNG measured"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn kitty_payload_shape() {
    let payload = kitty_image_payload(TINY_PNG, 7, 40);
    let s = String::from_utf8_lossy(&payload);
    assert!(s.starts_with("\x1b_Ga=T,f=100,i=7,c=40"), "control: {s:?}");
    assert!(s.ends_with("\x1b\\"), "terminator");

    let del = kitty_delete(7);
    assert_eq!(del, b"\x1b_Ga=d,d=I,i=7\x1b\\");
}

#[test]
fn png_decodes_and_sixel_encodes() {
    let (rgb, w, h) = png_to_rgb(TINY_PNG).expect("decodes");
    assert_eq!((w, h), (1, 1));
    assert_eq!(rgb.len(), 3);

    let sixel = encode_sixel(&rgb, w, h);
    assert!(sixel.starts_with(b"\x1bPq"), "sixel intro");
    assert!(sixel.ends_with(b"\x1b\\"), "sixel terminator");
    assert!(sixel.windows(2).any(|w| w == b"#0"), "palette defined");
}
