//! Rich transcript rendering: inline images, mermaid diagrams, LaTeX math.
//!
//! The transcript is line-oriented ([`crate::session::render_block`] builds
//! `Vec<Line>`), so every feature here degrades to styled text lines. The
//! actual terminal graphics (Kitty/Sixel) are emitted post-draw by
//! [`ImagePaintState::paint`]: ratatui owns the cells, graphics are placed
//! absolutely, and the two must never interleave inside one draw.
//!
//! Everything in the `*_impl` sections is pure and covered from `pantheon-eval`;
//! only `paint`/`clear` touch the terminal.

use std::io::Write;
use std::path::{Path, PathBuf};

// ===========================================================================
// Image references
// ===========================================================================

/// An image file referenced from a transcript message.
#[derive(Debug, Clone)]
pub struct ImageRef {
    /// Resolved absolute path.
    pub path: PathBuf,
    /// File name for display.
    pub name: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];

fn is_image_path(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    IMAGE_EXTS.iter().any(|e| lower.ends_with(e))
}

/// Scan message text for image file references: `@path`, markdown
/// `![alt](path)`, and bare path tokens. Only files that exist on disk are
/// returned, resolved to absolute paths, deduplicated in first-seen order.
pub fn find_images(text: &str) -> Vec<ImageRef> {
    let mut out: Vec<ImageRef> = Vec::new();
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut push = |raw: &str| {
        let raw = raw.trim().trim_matches(|c| c == '"' || c == '\'');
        if raw.is_empty() || !is_image_path(raw) {
            return;
        }
        // Expand a leading ~ the way shells do; leave everything else alone.
        let expanded = if let Some(rest) = raw.strip_prefix("~/") {
            match std::env::var("HOME") {
                Ok(h) => format!("{h}/{rest}"),
                Err(_) => raw.to_string(),
            }
        } else {
            raw.to_string()
        };
        let path = PathBuf::from(&expanded);
        if !path.is_file() {
            return;
        }
        let abs = std::fs::canonicalize(&path).unwrap_or(path);
        if seen.contains(&abs) {
            return;
        }
        seen.push(abs.clone());
        let (width, height) = image_dimensions(&abs).unwrap_or((None, None));
        let (width, height) = (width, height);
        out.push(ImageRef {
            name: abs
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| raw.to_string()),
            path: abs,
            width,
            height,
        });
    };

    // Markdown images first: ![alt](path).
    let mut rest = text;
    while let Some(bang) = rest.find("![") {
        let after = &rest[bang + 2..];
        if let Some(paren) = after.find("](") {
            let url = &after[paren + 2..];
            if let Some(end) = url.find(')') {
                push(&url[..end]);
                rest = &url[end + 1..];
                continue;
            }
        }
        rest = after;
    }
    // @path and bare tokens.
    for tok in text.split_whitespace() {
        let tok = tok.trim_matches(|c: char| ".,;:!?()[]{}".contains(c));
        let tok = tok.strip_prefix('@').unwrap_or(tok);
        push(tok);
    }
    out
}

fn read_u32_be(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// Image dimensions without decoding pixels: PNG (IHDR), JPEG (SOF), GIF
/// (logical screen descriptor). Returns `(width, height)` options so a
/// caller can still name a file it cannot measure.
pub fn image_dimensions(path: &Path) -> Option<(Option<u32>, Option<u32>)> {
    let bytes = std::fs::read(path).ok()?;
    // PNG: 8-byte signature, then IHDR width/height at offset 16.
    if bytes.len() > 24 && bytes.starts_with(b"\x89PNG\r\n\x1a\n") && &bytes[12..16] == b"IHDR" {
        return Some((
            Some(read_u32_be(&bytes[16..20])),
            Some(read_u32_be(&bytes[20..24])),
        ));
    }
    // GIF: "GIF8" then width/height u16 LE at offset 6.
    if bytes.len() > 10 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        let w = u16::from_le_bytes([bytes[6], bytes[7]]) as u32;
        let h = u16::from_le_bytes([bytes[8], bytes[9]]) as u32;
        return Some((Some(w), Some(h)));
    }
    // JPEG: scan markers for a Start-Of-Frame segment.
    if bytes.len() > 4 && bytes[0] == 0xFF && bytes[1] == 0xD8 {
        let mut i = 2;
        while i + 4 < bytes.len() {
            if bytes[i] != 0xFF {
                i += 1;
                continue;
            }
            let marker = bytes[i + 1];
            i += 2;
            if marker == 0xD8 || marker == 0xD9 || (0xD0..=0xD7).contains(&marker) || marker == 0x01
            {
                continue;
            }
            if i + 2 > bytes.len() {
                break;
            }
            let len = u16::from_be_bytes([bytes[i], bytes[i + 1]]) as usize;
            if (0xC0..=0xC3).contains(&marker) && i + 7 < bytes.len() {
                let h = u16::from_be_bytes([bytes[i + 3], bytes[i + 4]]) as u32;
                let w = u16::from_be_bytes([bytes[i + 5], bytes[i + 6]]) as u32;
                return Some((Some(w), Some(h)));
            }
            i += len;
        }
    }
    None
}

// ===========================================================================
// Graphics support detection
// ===========================================================================

/// Terminal inline-graphics capability, probed once at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GraphicsSupport {
    /// Kitty graphics protocol (also WezTerm, Ghostty, Konsole 23+).
    Kitty,
    /// Sixel (mlterm, foot with sixel, xterm -ti 340, iTerm2…).
    Sixel,
    /// No inline graphics: transcript shows a text placeholder.
    #[default]
    None,
}

impl GraphicsSupport {
    /// Probe once. Kitty advertises `KITTY_WINDOW_ID`; Sixel has no query
    /// that works everywhere, so it is opt-in via `PANTHEON_SIXEL=1` or a
    /// TERM that names it. Never blocks: no terminal queries are sent.
    pub fn detect() -> Self {
        if std::env::var_os("KITTY_WINDOW_ID").is_some()
            || std::env::var("TERM_PROGRAM")
                .map(|t| t.eq_ignore_ascii_case("wezterm") || t.eq_ignore_ascii_case("ghostty"))
                .unwrap_or(false)
        {
            return Self::Kitty;
        }
        if std::env::var("PANTHEON_SIXEL")
            .map(|v| v == "1")
            .unwrap_or(false)
            || std::env::var("TERM")
                .map(|t| t.to_ascii_lowercase().contains("sixel") || t == "mlterm")
                .unwrap_or(false)
        {
            return Self::Sixel;
        }
        Self::None
    }
}

// ===========================================================================
// Kitty graphics protocol
// ===========================================================================

const ESC: &str = "\x1b";
const APC_START: &str = "\x1b_G";
const APC_END: &str = "\x1b\\";

fn b64(data: &[u8]) -> String {
    const ALPH: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len() * 4 / 3 + 4);
    for chunk in data.chunks(3) {
        let mut n: u32 = 0;
        for (i, &b) in chunk.iter().enumerate() {
            n |= (b as u32) << (16 - 8 * i);
        }
        let pad = 3 - chunk.len();
        for i in 0..4 - pad {
            out.push(ALPH[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
        for _ in 0..pad {
            out.push('=');
        }
    }
    out
}

/// Build the byte stream that transmits a PNG (`f=100`: the terminal takes
/// PNG directly, no pixel decoding needed) and displays it `cols` cells
/// wide at image id `id`. Data is chunked at 4096 base64 chars per the
/// protocol; the first chunk carries the control payload.
pub fn kitty_image_payload(png: &[u8], id: u32, cols: u16) -> Vec<u8> {
    let encoded = b64(png);
    let chunks: Vec<&str> = encoded
        .as_bytes()
        .chunks(4096)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();
    let mut out = Vec::new();
    for (i, chunk) in chunks.iter().enumerate() {
        let last = i + 1 == chunks.len();
        if i == 0 {
            out.extend_from_slice(
                format!(
                    "{APC_START}a=T,f=100,i={id},c={cols},m={};",
                    i32::from(!last)
                )
                .as_bytes(),
            );
        } else {
            out.extend_from_slice(format!("{APC_START}m={};", i32::from(!last)).as_bytes());
        }
        out.extend_from_slice(chunk.as_bytes());
        out.extend_from_slice(APC_END.as_bytes());
    }
    if chunks.is_empty() {
        out.extend_from_slice(
            format!("{APC_START}a=T,f=100,i={id},c={cols},m=0;{APC_END}").as_bytes(),
        );
    }
    out
}

/// Delete one placed image by id (`a=d,d=I`).
pub fn kitty_delete(id: u32) -> Vec<u8> {
    format!("{APC_START}a=d,d=I,i={id}{APC_END}").into_bytes()
}

// ===========================================================================
// Sixel
// ===========================================================================

/// Decode a PNG to raw RGB. Minimal on purpose: 8-bit RGB/RGBA,
/// non-interlaced only — enough for screenshots and pasted images, which is
/// what the transcript ever shows. Returns `(rgb, width, height)`.
pub fn png_to_rgb(png: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    if !png.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    let mut width = 0u32;
    let mut height = 0u32;
    let mut bit_depth = 0u8;
    let mut color_type = 0u8;
    let mut interlace = 1u8;
    let mut idat: Vec<u8> = Vec::new();
    let mut i = 8;
    while i + 8 <= png.len() {
        let len = read_u32_be(&png[i..]) as usize;
        let kind = &png[i + 4..i + 8];
        let data = png.get(i + 8..i + 8 + len)?;
        match kind {
            b"IHDR" => {
                width = read_u32_be(&data[0..]);
                height = read_u32_be(&data[4..]);
                bit_depth = data[8];
                color_type = data[9];
                interlace = data[12];
            }
            b"IDAT" => idat.extend_from_slice(data),
            b"IEND" => break,
            _ => {}
        }
        i += 12 + len;
    }
    if width == 0 || height == 0 || bit_depth != 8 || interlace != 0 {
        return None;
    }
    let channels: usize = match color_type {
        2 => 3, // RGB
        6 => 4, // RGBA
        _ => return None,
    };
    // Inflate the zlib stream (2-byte header + Adler32 trailer).
    if idat.len() < 6 {
        return None;
    }
    let mut decoder = flate2::Decompress::new(false);
    let mut raw = vec![0u8; (width as usize) * (height as usize) * channels + height as usize];
    let status = decoder
        .decompress(&idat[2..], &mut raw, flate2::FlushDecompress::Finish)
        .ok()?;
    let out_len = decoder.total_out() as usize;
    let _ = status;
    let stride = width as usize * channels;
    if out_len < (stride + 1) * height as usize {
        return None;
    }
    // Unfilter scanlines (None/Sub/Up/Average/Paeth).
    let bpp = channels;
    let mut rgb = vec![0u8; width as usize * height as usize * 3];
    let mut prev = vec![0u8; stride];
    for y in 0..height as usize {
        let row_start = y * (stride + 1);
        let filter = raw[row_start];
        let cur = &raw[row_start + 1..row_start + 1 + stride];
        let mut recon = vec![0u8; stride];
        for x in 0..stride {
            let a = if x >= bpp { recon[x - bpp] } else { 0 };
            let b = prev[x];
            let c = if x >= bpp { prev[x - bpp] } else { 0 };
            let filt = cur[x];
            recon[x] = match filter {
                0 => filt,
                1 => filt.wrapping_add(a),
                2 => filt.wrapping_add(b),
                3 => filt.wrapping_add(((a as u16 + b as u16) / 2) as u8),
                4 => {
                    let p = a as i16 + b as i16 - c as i16;
                    let pa = (p - a as i16).abs();
                    let pb = (p - b as i16).abs();
                    let pc = (p - c as i16).abs();
                    let pr = if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    };
                    filt.wrapping_add(pr)
                }
                _ => return None,
            };
        }
        for x in 0..width as usize {
            let dst = (y * width as usize + x) * 3;
            rgb[dst] = recon[x * channels];
            rgb[dst + 1] = recon[x * channels + 1];
            rgb[dst + 2] = recon[x * channels + 2];
        }
        prev = recon;
    }
    Some((rgb, width, height))
}

/// Nearest-neighbor downscale of RGB data to fit within `max_w`.
fn downscale_rgb(rgb: &[u8], w: u32, h: u32, max_w: u32) -> (Vec<u8>, u32, u32) {
    if w <= max_w {
        return (rgb.to_vec(), w, h);
    }
    let nw = max_w;
    let nh = ((h as f64 * max_w as f64 / w as f64).round() as u32).max(1);
    let mut out = vec![0u8; (nw * nh * 3) as usize];
    for y in 0..nh {
        for x in 0..nw {
            let sx = (x as f64 * w as f64 / nw as f64) as u32;
            let sy = (y as f64 * h as f64 / nh as f64) as u32;
            let s = ((sy * w + sx) * 3) as usize;
            let d = ((y * nw + x) * 3) as usize;
            out[d..d + 3].copy_from_slice(&rgb[s..s + 3]);
        }
    }
    (out, nw, nh)
}

/// Encode raw RGB as Sixel. Palette is uniform 3-3-2 (256 entries max, only
/// the used ones are defined); each band is 6 pixel rows tall.
pub fn encode_sixel(rgb: &[u8], w: u32, h: u32) -> Vec<u8> {
    let w = w as usize;
    let h = h as usize;
    let quant = |r: u8, g: u8, b: u8| -> usize {
        ((r >> 5) as usize) << 5 | ((g >> 5) as usize) << 2 | ((b >> 6) as usize)
    };
    let mut used = [false; 256];
    for px in rgb.chunks_exact(3) {
        used[quant(px[0], px[1], px[2])] = true;
    }
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(format!("{ESC}Pq\"1;1;{w};{h}").as_bytes());
    for (i, u) in used.iter().enumerate() {
        if *u {
            let r = (((i >> 5) & 7) * 100 / 7) as u8;
            let g = (((i >> 2) & 7) * 100 / 7) as u8;
            let b = ((i & 3) * 100 / 3) as u8;
            out.extend_from_slice(format!("#{i};2;{r};{g};{b}").as_bytes());
        }
    }
    let bands = h.div_ceil(6);
    for band in 0..bands {
        for (i, u) in used.iter().enumerate() {
            if !*u {
                continue;
            }
            // Collect the sixel column bytes for this color in this band.
            let mut cols: Vec<u8> = Vec::with_capacity(w);
            for x in 0..w {
                let mut bits = 0u8;
                for row in 0..6 {
                    let y = band * 6 + row;
                    if y >= h {
                        break;
                    }
                    let px = &rgb[(y * w + x) * 3..][..3];
                    if quant(px[0], px[1], px[2]) == i {
                        bits |= 1 << row;
                    }
                }
                cols.push(bits + 63);
            }
            if cols.iter().all(|&c| c == 63) {
                continue; // color absent in this band
            }
            out.extend_from_slice(format!("#{i}").as_bytes());
            // Run-length encode repeats.
            let mut j = 0;
            while j < cols.len() {
                let mut k = j + 1;
                while k < cols.len() && cols[k] == cols[j] && k - j < 255 {
                    k += 1;
                }
                if k - j > 3 {
                    out.extend_from_slice(format!("!{}{}", k - j, cols[j] as char).as_bytes());
                } else {
                    out.extend_from_slice(&cols[j..k]);
                }
                j = k;
            }
            out.push(b'$');
        }
        out.push(b'-');
    }
    out.extend_from_slice(format!("{ESC}\\").as_bytes());
    out
}

// ===========================================================================

// ===========================================================================
// Mermaid flowcharts → Unicode box diagrams
// ===========================================================================

#[derive(Debug, Clone)]
struct MermaidNode {
    label: String,
}

#[derive(Debug, Clone)]
struct MermaidEdge {
    from: String,
    to: String,
    #[allow(dead_code)]
    label: Option<String>,
}

/// Common subset: `graph TD` / `graph LR` (`flowchart` alias accepted),
/// `A-->B`, `A --> B`, `A-->|label|B`, `A---B`, `A==>B`, `A-.->B`,
/// and node labels `A[text]`, `A{text}`, `A((text))`, `A([text])`, `A[[text]]`.
/// Anything outside the subset makes the whole block return `None` and the
/// caller shows the raw fence instead of a wrong diagram.
pub fn render_mermaid(src: &str) -> Option<Vec<String>> {
    let mut lines = src.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.next()?;
    let vertical = if first.starts_with("graph TD") || first.starts_with("flowchart TD") {
        true
    } else if first.starts_with("graph LR") || first.starts_with("flowchart LR") {
        false
    } else {
        return None;
    };

    let mut nodes: Vec<(String, MermaidNode)> = Vec::new();
    let mut edges: Vec<MermaidEdge> = Vec::new();

    fn node_id(s: &str) -> Option<String> {
        let s = s.trim();
        let end = s
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(s.len());
        let id = &s[..end];
        if id.is_empty() || !id.chars().next().is_some_and(|c| c.is_alphabetic()) {
            return None;
        }
        Some(id.to_string())
    }
    fn ensure(nodes: &mut Vec<(String, MermaidNode)>, id: &str) {
        if !nodes.iter().any(|(n, _)| n == id) {
            nodes.push((
                id.to_string(),
                MermaidNode {
                    label: id.to_string(),
                },
            ));
        }
    }
    // Parse `A[label]` / `A{text}` / `A((text))` / `A([text])` / `A[[text]]`
    // or a bare `A` (label defaults to the id).
    fn parse_node_def(s: &str, nodes: &mut Vec<(String, MermaidNode)>) -> Option<String> {
        let s = s.trim();
        let id = node_id(s)?;
        let rest = s[id.len()..].trim_start();
        if rest.is_empty() {
            ensure(nodes, &id);
            return Some(id);
        }
        let (open, close) = if rest.starts_with("((") {
            ("((", "))")
        } else if rest.starts_with("[[") {
            ("[[", "]]")
        } else if rest.starts_with("([") {
            ("([", "])")
        } else if rest.starts_with('[') {
            ("[", "]")
        } else if rest.starts_with('{') {
            ("{", "}")
        } else if rest.starts_with('(') {
            ("(", ")")
        } else {
            return None;
        };
        let inner = rest[open.len()..].strip_suffix(close)?;
        if let Some(entry) = nodes.iter_mut().find(|(n, _)| n == &id) {
            entry.1.label = inner.trim().to_string();
        } else {
            nodes.push((
                id.clone(),
                MermaidNode {
                    label: inner.trim().to_string(),
                },
            ));
        }
        Some(id)
    }

    for line in lines {
        let line = line.trim_end_matches(';').trim();
        if line.starts_with("%%") {
            continue;
        }
        // Edge operators, longest first. `-->` with a label needs `-->|x|`.
        let mut op_at: Option<(usize, &str)> = None;
        for op in ["-->|", "-->", "==>", "---", "-.->"] {
            if let Some(p) = line.find(op) {
                if op == "-->|" {
                    let after = &line[p + op.len()..];
                    if after.find('|').is_none() {
                        continue;
                    }
                }
                op_at = Some((p, op));
                break;
            }
        }
        if let Some((p, op)) = op_at {
            let left = line[..p].trim();
            let mut right = line[p + op.len()..].trim();
            let mut edge_label = None;
            if op == "-->|" {
                let end = right.find('|')?;
                edge_label = Some(right[..end].trim().to_string());
                right = right[end + 1..].trim();
            }
            // Each side may itself carry a node def: `A[x]-->B[y]`.
            let from = parse_node_def(left, &mut nodes)?;
            let to = parse_node_def(right, &mut nodes)?;
            edges.push(MermaidEdge {
                from,
                to,
                label: edge_label,
            });
            continue;
        }
        // Bare node definition line.
        if parse_node_def(line, &mut nodes).is_none() {
            return None;
        }
    }
    if nodes.is_empty() {
        return None;
    }

    // Longest-path layering: layer 0 has no incoming edges.
    let mut layer_of: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (id, _) in &nodes {
        layer_of.insert(id.as_str(), 0);
    }
    for _ in 0..nodes.len() + 1 {
        let mut changed = false;
        for e in &edges {
            let fl = layer_of[e.from.as_str()];
            let tl = layer_of[e.to.as_str()];
            if tl < fl + 1 {
                layer_of.insert(e.to.as_str(), fl + 1);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let max_layer = layer_of.values().copied().max().unwrap_or(0);
    let mut layers: Vec<Vec<&str>> = vec![Vec::new(); max_layer + 1];
    for (id, _) in &nodes {
        layers[layer_of[id.as_str()]].push(id.as_str());
    }
    let labels: std::collections::HashMap<&str, &str> = nodes
        .iter()
        .map(|(id, n)| (id.as_str(), n.label.as_str()))
        .collect();

    if vertical {
        render_td(&layers, &edges, &labels)
    } else {
        render_lr(&layers, &edges, &labels)
    }
}

/// Box width for a label: label chars + padding, minimum 6.
fn mbox_w(label: &str) -> usize {
    (label.chars().count() + 4).max(6)
}

fn mlabel<'a>(labels: &'a std::collections::HashMap<&str, &str>, id: &'a str) -> &'a str {
    labels.get(id).copied().unwrap_or(id)
}

/// One node box: `width` chars wide including borders, label padded left.
fn draw_box(label: &str, width: usize) -> (String, String, String) {
    let inner = width.saturating_sub(2);
    let text: String = label.chars().take(inner.saturating_sub(2)).collect();
    let pad = inner.saturating_sub(2).saturating_sub(text.chars().count());
    let mid = format!("│ {text}{} │", " ".repeat(pad));
    (
        format!("╭{}╮", "─".repeat(width)),
        mid,
        format!("╰{}╯", "─".repeat(width)),
    )
}

/// Top-down layout: one layer per row, connectors as `▼` under each parent.
fn render_td(
    layers: &[Vec<&str>],
    edges: &[MermaidEdge],
    labels: &std::collections::HashMap<&str, &str>,
) -> Option<Vec<String>> {
    let col_w = layers
        .iter()
        .flat_map(|l| l.iter())
        .map(|id| mbox_w(mlabel(labels, id)))
        .max()
        .unwrap_or(8)
        .max(8);
    let cols = layers.iter().map(|l| l.len()).max().unwrap_or(1);
    let total_w = cols * (col_w + 2);

    let mut pos: std::collections::HashMap<&str, (usize, usize)> = std::collections::HashMap::new();
    for (li, layer) in layers.iter().enumerate() {
        for (ii, id) in layer.iter().enumerate() {
            pos.insert(id, (li, ii));
        }
    }
    let center_x = |li: usize, ii: usize| -> usize {
        let start = total_w.saturating_sub(layers[li].len() * (col_w + 2)) / 2;
        start + ii * (col_w + 2) + col_w / 2
    };

    let mut out: Vec<String> = Vec::new();
    for (li, layer) in layers.iter().enumerate() {
        let mut top = String::new();
        let mut mid = String::new();
        let mut bot = String::new();
        for (ii, id) in layer.iter().enumerate() {
            let w = mbox_w(mlabel(labels, id));
            let (t, m, b) = draw_box(mlabel(labels, id), w);
            let x = center_x(li, ii).saturating_sub(w / 2);
            for row in [&mut top, &mut mid, &mut bot] {
                while row.chars().count() < x {
                    row.push(' ');
                }
            }
            top.push_str(&t);
            mid.push_str(&m);
            bot.push_str(&b);
        }
        out.push(top.trim_end().to_string());
        out.push(mid.trim_end().to_string());
        out.push(bot.trim_end().to_string());
        if li + 1 < layers.len() {
            let mut conn: Vec<char> = vec![' '; total_w.max(1)];
            for e in edges {
                let (fl, fi) = pos.get(e.from.as_str()).copied().unwrap_or((0, 0));
                let tl = pos.get(e.to.as_str()).copied().map(|(l, _)| l).unwrap_or(0);
                if fl == li && tl == li + 1 {
                    let x = center_x(fl, fi);
                    if x < conn.len() {
                        conn[x] = '▼';
                    }
                }
            }
            let s: String = conn.into_iter().collect();
            out.push(if s.trim().is_empty() {
                String::new()
            } else {
                s.trim_end().to_string()
            });
        }
    }
    Some(out)
}

/// Left-right layout: layers become columns, `──▶` on the middle row.
fn render_lr(
    layers: &[Vec<&str>],
    edges: &[MermaidEdge],
    labels: &std::collections::HashMap<&str, &str>,
) -> Option<Vec<String>> {
    let rows = layers.iter().map(|l| l.len()).max().unwrap_or(1);
    let mut grid: Vec<Vec<Option<&str>>> = vec![vec![None; layers.len()]; rows];
    for (li, layer) in layers.iter().enumerate() {
        for (ri, id) in layer.iter().enumerate() {
            grid[ri][li] = Some(id);
        }
    }
    let col_w: Vec<usize> = layers
        .iter()
        .map(|l| {
            l.iter()
                .map(|id| mbox_w(mlabel(labels, id)))
                .max()
                .unwrap_or(8)
        })
        .collect();

    let edge_between = |a: Option<&str>, b: Option<&str>| -> bool {
        match (a, b) {
            (Some(a), Some(b)) => edges.iter().any(|e| e.from == a && e.to == b),
            _ => false,
        }
    };

    let mut out: Vec<String> = Vec::new();
    for row in &grid {
        let mut top = String::new();
        let mut mid = String::new();
        let mut bot = String::new();
        for (li, cell) in row.iter().enumerate() {
            let w = col_w[li];
            if let Some(id) = cell {
                let (t, m, b) = draw_box(mlabel(labels, id), w);
                top.push_str(&t);
                mid.push_str(&m);
                bot.push_str(&b);
            } else {
                top.push_str(&" ".repeat(w));
                mid.push_str(&" ".repeat(w));
                bot.push_str(&" ".repeat(w));
            }
            if li + 1 < row.len() {
                let arrow = if edge_between(*cell, row[li + 1]) {
                    "──▶ "
                } else {
                    "    "
                };
                top.push_str("    ");
                mid.push_str(arrow);
                bot.push_str("    ");
            }
        }
        out.push(top.trim_end().to_string());
        out.push(mid.trim_end().to_string());
        out.push(bot.trim_end().to_string());
        out.push(String::new());
    }
    Some(out)
}

/// Expand fenced ```mermaid blocks into Unicode diagrams. Spans lines, so it
/// runs before line splitting. Unknown syntax keeps the raw fence verbatim.
fn expand_mermaid_fences(text: &str) -> String {
    let mut expanded = String::new();
    let mut rest = text;
    loop {
        let start = match rest.find("```mermaid") {
            Some(p) => p,
            None => {
                expanded.push_str(rest);
                break;
            }
        };
        expanded.push_str(&rest[..start]);
        let body_start = start + "```mermaid".len();
        let after = &rest[body_start..];
        match after.find("```") {
            Some(end) => {
                let body = &after[..end];
                match render_mermaid(body) {
                    Some(diagram) => {
                        expanded.push('\n');
                        for line in diagram {
                            expanded.push_str(&line);
                            expanded.push('\n');
                        }
                    }
                    None => {
                        // Unknown syntax: keep the raw fence verbatim.
                        expanded.push_str(&rest[start..body_start + end + 3]);
                    }
                }
                rest = &after[end + 3..];
            }
            None => {
                expanded.push_str(&rest[start..]);
                break;
            }
        }
    }
    expanded
}

// ===========================================================================
// LaTeX math → Unicode approximations
// ===========================================================================

/// Replace a LaTeX command name (without backslash) with its Unicode
/// approximation. Unknown commands return `None` and pass through untouched.
fn latex_command(name: &str) -> Option<&'static str> {
    Some(match name {
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" => "ε",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" => "θ",
        "lambda" => "λ",
        "mu" => "μ",
        "pi" => "π",
        "sigma" => "σ",
        "tau" => "τ",
        "phi" => "φ",
        "omega" => "ω",
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Sigma" => "Σ",
        "Omega" => "Ω",
        "infty" => "∞",
        "sum" => "∑",
        "prod" => "∏",
        "int" => "∫",
        "sqrt" => "√",
        "times" => "×",
        "cdot" => "·",
        "div" => "÷",
        "pm" => "±",
        "leq" => "≤",
        "geq" => "≥",
        "neq" => "≠",
        "approx" => "≈",
        "equiv" => "≡",
        "rightarrow" => "→",
        "leftarrow" => "←",
        "leftrightarrow" => "↔",
        "Rightarrow" => "⇒",
        "to" => "→",
        "ldots" => "…",
        "cdots" => "⋯",
        "partial" => "∂",
        "forall" => "∀",
        "exists" => "∃",
        "in" => "∈",
        "notin" => "∉",
        "subset" => "⊂",
        "cup" => "∪",
        "cap" => "∩",
        "angle" => "∠",
        "degree" => "°",
        _ => return None,
    })
}

fn superscript(c: char) -> Option<char> {
    Some(match c {
        '0' => '⁰',
        '1' => '¹',
        '2' => '²',
        '3' => '³',
        '4' => '⁴',
        '5' => '⁵',
        '6' => '⁶',
        '7' => '⁷',
        '8' => '⁸',
        '9' => '⁹',
        '+' => '⁺',
        '-' => '⁻',
        '=' => '⁼',
        '(' => '⁽',
        ')' => '⁾',
        'n' => 'ⁿ',
        'i' => 'ⁱ',
        'a' => 'ᵃ',
        'b' => 'ᵇ',
        'c' => 'ᶜ',
        'd' => 'ᵈ',
        'e' => 'ᵉ',
        'f' => 'ᶠ',
        'g' => 'ᵍ',
        'h' => 'ʰ',
        'j' => 'ʲ',
        'k' => 'ᵏ',
        'l' => 'ˡ',
        'm' => 'ᵐ',
        'o' => 'ᵒ',
        'p' => 'ᵖ',
        'r' => 'ʳ',
        's' => 'ˢ',
        't' => 'ᵗ',
        'u' => 'ᵘ',
        'v' => 'ᵛ',
        'w' => 'ʷ',
        'x' => 'ˣ',
        'y' => 'ʸ',
        'z' => 'ᶻ',
        _ => return None,
    })
}

fn subscript(c: char) -> Option<char> {
    Some(match c {
        '0' => '₀',
        '1' => '₁',
        '2' => '₂',
        '3' => '₃',
        '4' => '₄',
        '5' => '₅',
        '6' => '₆',
        '7' => '₇',
        '8' => '₈',
        '9' => '₉',
        '+' => '₊',
        '-' => '₋',
        '=' => '₌',
        '(' => '₍',
        ')' => '₎',
        'a' => 'ₐ',
        'e' => 'ₑ',
        'i' => 'ᵢ',
        'j' => 'ⱼ',
        'n' => 'ₙ',
        'o' => 'ₒ',
        'r' => 'ᵣ',
        'u' => 'ᵤ',
        'v' => 'ᵥ',
        'x' => 'ₓ',
        _ => return None,
    })
}

/// Render one math span (without the surrounding `$`s) to Unicode.
/// `display` stacks `\frac` vertically; inline uses the fraction slash.
fn render_math_span(span: &str, display: bool) -> String {
    let chars: Vec<char> = span.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    // Extract a brace group starting at `i` (which points at `{`).
    let take_group = |i: &mut usize| -> Option<String> {
        if chars.get(*i) != Some(&'{') {
            return None;
        }
        let mut depth = 0;
        let mut s = String::new();
        for (k, &c) in chars.iter().enumerate().skip(*i) {
            if c == '{' {
                depth += 1;
                if depth > 1 {
                    s.push(c);
                }
            } else if c == '}' {
                depth -= 1;
                if depth == 0 {
                    *i = k + 1;
                    return Some(s);
                }
                s.push(c);
            } else if depth >= 1 {
                s.push(c);
            }
        }
        None
    };
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' {
            // \frac, \sqrt need brace args; try them first.
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_alphabetic() {
                j += 1;
            }
            let name: String = chars[i + 1..j].iter().collect();
            if name == "frac" {
                let mut k = j;
                k += chars[j..].iter().take_while(|c| **c == ' ').count();
                if let Some(num) = take_group(&mut k) {
                    let mut k2 = k;
                    k2 += chars[k..].iter().take_while(|c| **c == ' ').count();
                    if let Some(den) = take_group(&mut k2) {
                        let num = render_math_span(&num, false);
                        let den = render_math_span(&den, false);
                        if display {
                            let w = num.chars().count().max(den.chars().count()).max(1);
                            out.push('\n');
                            out.push_str(&format!("{:^w$}\n{}", num, "─".repeat(w), w = w));
                            out.push('\n');
                            out.push_str(&format!("{:^w$}", den, w = w));
                        } else {
                            out.push_str(&format!("{num}⁄{den}"));
                        }
                        i = k2;
                        continue;
                    }
                }
                // Malformed \frac: pass through.
                out.push('\\');
                out.push_str(&name);
                i = j;
                continue;
            }
            if name == "sqrt" {
                let mut k = j;
                k += chars[j..].iter().take_while(|c| **c == ' ').count();
                if let Some(inner) = take_group(&mut k) {
                    out.push('√');
                    out.push('(');
                    out.push_str(&render_math_span(&inner, false));
                    out.push(')');
                    i = k;
                    continue;
                }
                out.push('\\');
                out.push_str(&name);
                i = j;
                continue;
            }
            if let Some(rep) = latex_command(&name) {
                out.push_str(rep);
            } else {
                // Unknown command: pass through verbatim.
                out.push('\\');
                out.push_str(&name);
            }
            i = j;
            continue;
        }
        if c == '^' || c == '_' {
            let map = if c == '^' { superscript } else { subscript };
            let mut j = i + 1;
            if chars.get(j) == Some(&'{') {
                j += 1;
                while j < chars.len() && chars[j] != '}' {
                    if let Some(r) = map(chars[j]) {
                        out.push(r);
                    } else {
                        out.push(chars[j]);
                    }
                    j += 1;
                }
                i = j + 1;
            } else if let Some(&next) = chars.get(j) {
                if let Some(r) = map(next) {
                    out.push(r);
                    i = j + 1;
                } else {
                    out.push(c);
                    i += 1;
                }
            } else {
                out.push(c);
                i += 1;
            }
            continue;
        }
        if c == '{' || c == '}' {
            i += 1; // grouping braces vanish after command handling
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Render LaTeX math in text: display `$$…$$` becomes its own indented
/// block, inline `$…$` is replaced in place. A `$` needs a matching close
/// on a later position; unmatched `$` passes through untouched.
pub fn render_latex(text: &str) -> String {
    let mut out = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '$' {
            let display = chars.get(i + 1) == Some(&'$');
            let open_len = if display { 2 } else { 1 };
            // Find the matching close.
            let mut j = i + open_len;
            let mut found = None;
            while j < chars.len() {
                if chars[j] == '$' {
                    if display {
                        if chars.get(j + 1) == Some(&'$') {
                            found = Some(j);
                            break;
                        }
                        j += 1;
                    } else {
                        found = Some(j);
                        break;
                    }
                } else {
                    j += 1;
                }
            }
            if let Some(close) = found {
                let span: String = chars[i + open_len..close].iter().collect();
                let rendered = render_math_span(span.trim(), display);
                if display {
                    out.push('\n');
                    for line in rendered.lines() {
                        out.push_str("    ");
                        out.push_str(line);
                        out.push('\n');
                    }
                } else {
                    out.push_str(&rendered);
                }
                i = close + open_len;
                continue;
            }
            // Unmatched: literal.
            out.push('$');
            i += 1;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

// ===========================================================================
// Message segmentation: text + images + rendered fences
// ===========================================================================

/// One renderable piece of a message.
#[derive(Debug)]
pub enum RichSegment {
    /// Plain (already LaTeX-expanded, mermaid-rendered) text, may be multiline.
    Text(String),
    /// An image reference; the renderer shows a thumbnail or placeholder.
    Image(ImageRef),
}

/// Split a message into text and image segments. Fenced ` ```mermaid `
/// blocks become rendered diagrams (or pass through raw when the syntax is
/// unknown); image references become [`RichSegment::Image`]; everything
/// else gets [`render_latex`] applied.
pub fn segment_message(text: &str) -> Vec<RichSegment> {
    // First pass: mermaid fences (they span lines, so handle before split).
    let expanded = expand_mermaid_fences(text);

    // Second pass: images line by line, latex on the text lines.
    let mut segments: Vec<RichSegment> = Vec::new();
    let mut pending_text = String::new();
    let flush_text = |pending: &mut String, segments: &mut Vec<RichSegment>| {
        if !pending.trim().is_empty() {
            segments.push(RichSegment::Text(render_latex(pending)));
        }
        pending.clear();
    };
    for line in expanded.lines() {
        let images = find_images(line);
        if images.is_empty() {
            pending_text.push_str(line);
            pending_text.push('\n');
        } else {
            flush_text(&mut pending_text, &mut segments);
            // Keep any non-path text on the line as its own text segment.
            let stripped = strip_image_tokens(line, &images);
            if !stripped.trim().is_empty() {
                segments.push(RichSegment::Text(render_latex(&stripped)));
            }
            for img in images {
                segments.push(RichSegment::Image(img));
            }
        }
    }
    flush_text(&mut pending_text, &mut segments);
    segments
}

/// Remove image path tokens (and their `@`/`![..](..)` wrappers) from a line
/// so the prose that introduced the image still reads naturally.
fn strip_image_tokens(line: &str, images: &[ImageRef]) -> String {
    let names: Vec<&str> = images.iter().map(|i| i.name.as_str()).collect();
    let mut s = line.to_string();
    // Markdown wrappers: ![alt](path).
    let mut idx = 0;
    while idx < s.len() {
        let bang = match s[idx..].find("![") {
            Some(p) => idx + p,
            None => break,
        };
        let after = &s[bang + 2..];
        let paren = match after.find("](") {
            Some(p) => p,
            None => break,
        };
        let url_start = bang + 2 + paren + 2;
        let end = match s[url_start..].find(')') {
            Some(p) => p,
            None => break,
        };
        let fname = s[url_start..url_start + end]
            .rsplit('/')
            .next()
            .unwrap_or("");
        if names.contains(&fname) {
            s.replace_range(bang..url_start + end + 1, "");
        } else {
            idx = url_start + end + 1;
        }
    }
    // Bare and @-prefixed tokens, matched on file name.
    let mut kept: Vec<&str> = Vec::new();
    for tok in s.split_whitespace() {
        let clean = tok
            .trim_matches(|c: char| ".,;:!?()[]{}\"'".contains(c))
            .strip_prefix('@')
            .unwrap_or_else(|| tok.trim_matches(|c: char| ".,;:!?()[]{}\"'".contains(c)));
        let fname = clean.rsplit('/').next().unwrap_or(clean);
        if names.contains(&fname) {
            continue;
        }
        kept.push(tok);
    }
    kept.join(" ")
}

// ===========================================================================
// Inline painting: rows reserved at render time, pixels placed post-draw
// ===========================================================================

/// One image row reservation inside the transcript.
#[derive(Debug, Clone)]
pub struct ImagePlacement {
    /// Unique id for Kitty placement/deletion.
    pub id: u32,
    /// Transcript line index of the first reserved row.
    pub line_idx: usize,
    /// Resolved image path.
    pub path: PathBuf,
    /// File name for the placeholder text.
    pub name: String,
    /// Cells wide the thumbnail is painted.
    pub cols: u16,
    /// Transcript rows reserved (thumbnail height + label row).
    pub rows: u16,
}

/// Transcript viewport geometry captured during render.
#[derive(Debug, Clone, Copy)]
pub struct TranscriptView {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub scroll_off: usize,
}

/// Fullscreen image preview state.
#[derive(Debug, Clone)]
pub struct ImagePreview {
    /// All transcript images, so `[`/`]` can cycle.
    pub images: Vec<PathBuf>,
    pub sel: usize,
    pub zoom: f32,
    pub pan_x: i32,
    pub pan_y: i32,
}

/// Owns everything about inline images: placements recorded during render,
/// what is currently painted on the terminal, capability, and the preview.
#[derive(Debug, Default)]
pub struct ImagePaintState {
    pub graphics: GraphicsSupport,
    pub placements: Vec<ImagePlacement>,
    pub view: Option<TranscriptView>,
    pub preview: Option<ImagePreview>,
    /// Where the preview box was drawn this frame (set by the renderer).
    pub preview_area: Option<ratatui::layout::Rect>,
    next_id: u32,
    painted: Vec<PaintedImage>,
    /// (path, sel, zoom_pct, pan_x, pan_y) of the preview image currently
    /// on the terminal: skip repaint when nothing changed.
    preview_painted: Option<(PathBuf, usize, u32, i32, i32)>,
}

#[derive(Debug, Clone)]
struct PaintedImage {
    id: u32,
    row: u16,
    col: u16,
}

impl ImagePaintState {
    pub fn new() -> Self {
        Self {
            graphics: GraphicsSupport::detect(),
            ..Default::default()
        }
    }

    /// Reserve rows for an image and return its placement id. Called from
    /// `render_block`; rows are painted later by [`Self::paint`].
    pub fn reserve(&mut self, img: &ImageRef, line_idx: usize, term_width: u16) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        let cols = (term_width.saturating_sub(12).min(48).max(16)) as u16;
        // Thumbnail rows at ~2:1 cell aspect + one label row.
        let rows = match (img.width, img.height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => {
                ((cols as f32 * h as f32 / w as f32 / 2.0).ceil() as u16).clamp(2, 12) + 1
            }
            _ => 5,
        };
        self.placements.push(ImagePlacement {
            id,
            line_idx,
            path: img.path.clone(),
            name: img.name.clone(),
            cols,
            rows,
        });
        id
    }

    /// Text shown when graphics are unavailable (or as the first reserved
    /// row under a painted thumbnail): `[1] 🖼 name (WxH) — v to view`.
    pub fn placeholder_text(p: &ImagePlacement, dims: Option<(u32, u32)>, index: usize) -> String {
        let dims = dims
            .map(|(w, h)| format!("{w}×{h}"))
            .unwrap_or_else(|| "unknown size".to_string());
        format!("[{}] 🖼 {} ({dims}) — press v to view", index + 1, p.name)
    }

    /// Diff desired placements against what is painted and emit Kitty/Sixel
    /// sequences. No-op unless Kitty is available: Sixel and text fallback
    /// need no per-frame painting (Sixel paints on demand in the preview).
    pub fn paint(&mut self) -> std::io::Result<()> {
        if self.graphics != GraphicsSupport::Kitty {
            return Ok(());
        }
        let Some(view) = self.view else {
            return self.clear_painted();
        };
        // Desired: visible placements with absolute rows. The label row
        // stays text; the thumbnail paints into the reserved rows below it.
        let mut desired: Vec<(u32, PathBuf, u16, u16, u16)> = Vec::new(); // id, path, row, col, cols
        for p in &self.placements {
            if p.line_idx < view.scroll_off {
                continue;
            }
            let label_row = view.y + 1 + (p.line_idx - view.scroll_off) as u16;
            let row = label_row + 1;
            if row + p.rows.saturating_sub(1) > view.y + view.height.saturating_sub(1) {
                continue;
            }
            desired.push((p.id, p.path.clone(), row, view.x + 2, p.cols));
        }
        // Delete anything painted that is no longer desired (or moved).
        let mut keep: Vec<PaintedImage> = Vec::new();
        let mut stale: Vec<u32> = Vec::new();
        for painted in self.painted.drain(..) {
            let still_there = desired.iter().any(|(id, _, row, col, _)| {
                *id == painted.id && *row == painted.row && *col == painted.col
            });
            if still_there {
                keep.push(painted);
            } else {
                stale.push(painted.id);
            }
        }
        self.painted = keep;
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        for id in stale {
            lock.write_all(&kitty_delete(id))?;
        }
        // Paint new ones (PNG via f=100; anything else skipped — the
        // placeholder text already describes it).
        for (id, path, row, col, cols) in &desired {
            if self.painted.iter().any(|p| p.id == *id) {
                continue;
            }
            let is_png = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("png"))
                .unwrap_or(false);
            if !is_png {
                continue;
            }
            if let Ok(png) = std::fs::read(path) {
                crossterm::execute!(lock, crossterm::cursor::MoveTo(*col, *row))?;
                lock.write_all(&kitty_image_payload(&png, *id, *cols))?;
                self.painted.push(PaintedImage {
                    id: *id,
                    row: *row,
                    col: *col,
                });
            }
        }
        lock.flush()?;
        Ok(())
    }

    /// Delete every painted image. Called on overlay open, scroll jumps,
    /// and shutdown.
    pub fn clear_painted(&mut self) -> std::io::Result<()> {
        if self.painted.is_empty() {
            return Ok(());
        }
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        for p in self.painted.drain(..) {
            lock.write_all(&kitty_delete(p.id))?;
        }
        lock.flush()?;
        Ok(())
    }

    /// Paint the fullscreen preview image centered in `area` (called
    /// post-draw, like [`Self::paint`]). Returns `Ok(false)` when there is
    /// nothing to paint (text fallback already rendered the box).
    pub fn paint_preview(&mut self, area: ratatui::layout::Rect) -> std::io::Result<bool> {
        let Some(preview) = self.preview.clone() else {
            return Ok(false);
        };
        let path = match preview.images.get(preview.sel) {
            Some(p) => p.clone(),
            None => return Ok(false),
        };
        // Skip repaint when nothing changed: Kitty/Sixel images persist
        // until deleted, so repainting every frame would just flicker.
        let key = (
            path.clone(),
            preview.sel,
            (preview.zoom * 100.0) as u32,
            preview.pan_x,
            preview.pan_y,
        );
        if self.preview_painted.as_ref() == Some(&key) {
            return Ok(true);
        }
        let png = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => return Ok(false),
        };
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        match self.graphics {
            GraphicsSupport::Kitty => {
                // Scale by cells: base 60 cols * zoom, clamped to the box.
                let cols =
                    ((60.0 * preview.zoom) as u16).clamp(10, area.width.saturating_sub(4).max(10));
                let col =
                    (area.x as i32 + (area.width.saturating_sub(cols) as i32) / 2 + preview.pan_x)
                        .max(0) as u16;
                let row = (area.y as i32 + 1 + preview.pan_y).max(0) as u16;
                crossterm::execute!(lock, crossterm::cursor::MoveTo(col, row))?;
                // Kitty id 0 is reserved for the preview; repaint deletes first.
                lock.write_all(&kitty_delete(0))?;
                // c= alone preserves aspect; zoom scales the cell width.
                lock.write_all(&kitty_image_payload(&png, 0, cols))?;
            }
            GraphicsSupport::Sixel => {
                if let Some((rgb, w, h)) = png_to_rgb(&png) {
                    let (rgb, w, h) = downscale_rgb(&rgb, w, h, 480);
                    let col = (area.x as i32 + 2 + preview.pan_x).max(0) as u16;
                    let row = (area.y as i32 + 2 + preview.pan_y).max(0) as u16;
                    crossterm::execute!(lock, crossterm::cursor::MoveTo(col, row))?;
                    lock.write_all(&encode_sixel(&rgb, w, h))?;
                } else {
                    return Ok(false);
                }
            }
            GraphicsSupport::None => return Ok(false),
        }
        lock.flush()?;
        self.preview_painted = Some(key);
        Ok(true)
    }

    /// Open the preview on the most recent transcript image.
    pub fn open_preview(&mut self) {
        let images: Vec<PathBuf> = self.placements.iter().map(|p| p.path.clone()).collect();
        if images.is_empty() {
            return;
        }
        self.preview = Some(ImagePreview {
            sel: images.len() - 1,
            images,
            zoom: 1.0,
            pan_x: 0,
            pan_y: 0,
        });
    }

    pub fn close_preview(&mut self) -> std::io::Result<()> {
        self.preview = None;
        self.preview_painted = None;
        // The preview used Kitty id 0; remove it.
        if self.graphics == GraphicsSupport::Kitty {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            lock.write_all(&kitty_delete(0))?;
            lock.flush()?;
        }
        Ok(())
    }
}
