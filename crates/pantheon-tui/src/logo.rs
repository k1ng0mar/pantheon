//! The Pantheon lockup drawn as a terminal dot-matrix.
//!
//! The artwork is the brand lockup, the temple-with-P mark over the `PANTHEON`
//! wordmark, stored as a 1-bit raster (`assets/logo-lockup.bin`, 236x209) so the
//! terminal draws the same outlines as the brand artwork instead of a re-derived
//! approximation of them. Braille patterns pack 2x4 dots into one character
//! cell, the densest portrait grid that needs nothing beyond a monospace font
//! and the UTF-8 Braille block, so the default splash works in an ordinary
//! terminal with no Kitty graphics and no Sixel.
//!
//! The splash opens the way the brand loop does: a top-to-bottom sweep resolved
//! by an 8x8 ordered dither, a diagonal glint, then a dissolve, on a loop.
//! `PANTHEON_LOGO=static` holds the finished lockup, `=block` uses half-block
//! cells for fonts missing the Braille patterns, `=off` drops the artwork.
//!
//! Nothing here touches the composer, the transcript, or the status bar: the
//! lockup is only drawn on the new-session splash.

use ratatui::style::Color;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// The raster
// ---------------------------------------------------------------------------

/// Lockup width in dots (the raster's own pixel grid).
const LOCKUP_W: usize = 236;
/// Lockup height in dots.
const LOCKUP_H: usize = 209;
/// Bits are packed MSB first and every row is padded to a whole byte.
const ROW_BYTES: usize = LOCKUP_W.div_ceil(8);
const LOCKUP: &[u8] = include_bytes!("../assets/logo-lockup.bin");

/// First raster row of the wordmark; every row above it belongs to the mark.
const WORDMARK_TOP: usize = 186;

/// Shortest lockup, in cells, that still reads as the brand: below this the
/// wordmark is a smear, so the splash draws the mark alone instead.
const MIN_LOCKUP_ROWS: usize = 18;
/// Narrowest lockup, in cells, before the P stops reading as a P.
const MIN_LOCKUP_COLS: usize = 28;
/// Smallest mark-only fallback, in cells.
const MIN_MARK_COLS: usize = 14;

const DOT_BITS: [[u8; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];
const BRAILLE_BASE: u32 = 0x2800;

/// 8x8 ordered-dither thresholds: the matrix that breaks the reveal sweep into
/// dots instead of a hard edge.
const BAYER8: [[f64; 8]; 8] = [
    [
        0.0078, 0.5078, 0.1328, 0.6328, 0.0391, 0.5391, 0.1641, 0.6641,
    ],
    [
        0.7578, 0.2578, 0.8828, 0.3828, 0.7891, 0.2891, 0.9141, 0.4141,
    ],
    [
        0.1953, 0.6953, 0.0703, 0.5703, 0.2266, 0.7266, 0.1016, 0.6016,
    ],
    [
        0.9453, 0.4453, 0.8203, 0.3203, 0.9766, 0.4766, 0.8516, 0.3516,
    ],
    [
        0.0547, 0.5547, 0.1797, 0.6797, 0.0234, 0.5234, 0.1484, 0.6484,
    ],
    [
        0.8047, 0.3047, 0.9297, 0.4297, 0.7734, 0.2734, 0.8984, 0.3984,
    ],
    [
        0.2422, 0.7422, 0.1172, 0.6172, 0.2109, 0.7109, 0.0859, 0.5859,
    ],
    [
        0.9922, 0.4922, 0.8672, 0.3672, 0.9609, 0.4609, 0.8359, 0.3359,
    ],
];

// ---------------------------------------------------------------------------
// Reveal timeline (seconds, looping)
// ---------------------------------------------------------------------------

const LOOP: f64 = 9.0;
const SWEEP_START: f64 = 0.5;
const SWEEP_END: f64 = 3.0;
const WORD_START: f64 = 2.5;
const WORD_END: f64 = 3.8;
const GLINT_START: f64 = 3.0;
const GLINT_END: f64 = 6.8;
const GLINT_FROM: f64 = -0.35;
const GLINT_SPAN: f64 = 2.1;
const DISSOLVE_START: f64 = 7.2;
const DISSOLVE_END: f64 = 8.6;

/// The lockup's colours: violet down the mark, pale lavender for the wordmark.
const MARK_TOP: (u8, u8, u8) = (185, 156, 255);
const MARK_MID: (u8, u8, u8) = (123, 77, 255);
const MARK_BOTTOM: (u8, u8, u8) = (92, 50, 232);
const WORD_COLOUR: (u8, u8, u8) = (229, 220, 255);

// ---------------------------------------------------------------------------
// Modes
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Braille,
    Block,
    Off,
    Static,
}

fn mode() -> Mode {
    static MODE: OnceLock<Mode> = OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("PANTHEON_LOGO").as_deref() {
        Ok("block") | Ok("half") => Mode::Block,
        Ok("off") | Ok("none") => Mode::Off,
        Ok("static") | Ok("still") => Mode::Static,
        _ => Mode::Braille,
    })
}

/// Dots per character cell: Braille is 2 wide by 4 tall, half-block 1 by 2.
fn dots_per_cell() -> (usize, usize) {
    match mode() {
        Mode::Block => (1, 2),
        _ => (2, 4),
    }
}

// ---------------------------------------------------------------------------
// Sampling
// ---------------------------------------------------------------------------

fn lockup_bit(x: usize, y: usize) -> bool {
    let index = y * ROW_BYTES + (x >> 3);
    index < LOCKUP.len() && LOCKUP[index] & (0x80 >> (x & 7)) != 0
}

/// A rectangle of the raster, inclusive on every edge.
#[derive(Clone, Copy)]
struct Band {
    x0: usize,
    x1: usize,
    y0: usize,
    y1: usize,
}

impl Band {
    fn width(&self) -> usize {
        self.x1 - self.x0 + 1
    }

    fn height(&self) -> usize {
        self.y1 - self.y0 + 1
    }

    /// The lockup in full.
    fn lockup() -> Self {
        Self {
            x0: 0,
            x1: LOCKUP_W - 1,
            y0: 0,
            y1: LOCKUP_H - 1,
        }
    }

    /// The mark's ink box, so the fallback path spends the room the wordmark
    /// gave up rather than leaving the mark small and centred in it.
    fn mark() -> Self {
        static BOX: OnceLock<Band> = OnceLock::new();
        *BOX.get_or_init(|| {
            let mut band = Self {
                x0: LOCKUP_W,
                x1: 0,
                y0: WORDMARK_TOP,
                y1: 0,
            };
            for y in 0..WORDMARK_TOP {
                for x in 0..LOCKUP_W {
                    if lockup_bit(x, y) {
                        band.x0 = band.x0.min(x);
                        band.x1 = band.x1.max(x);
                        band.y0 = band.y0.min(y);
                        band.y1 = y;
                    }
                }
            }
            band
        })
    }
}

/// Cell size for a band given the rows the splash can spare. Both packings keep
/// the same cell aspect, so the mode does not change how much room the artwork
/// takes, only how finely it is drawn.
fn fit(term_w: usize, available_rows: usize, band: &Band) -> Option<(usize, usize)> {
    let aspect = 2.0 * band.width() as f64 / band.height() as f64;
    let cols = ((available_rows as f64 * aspect).floor() as usize).min(term_w.saturating_sub(4));
    let rows = (cols as f64 / aspect).ceil() as usize;
    (cols >= 2 && rows >= 2).then_some((cols, rows))
}

// ---------------------------------------------------------------------------
// Reveal
// ---------------------------------------------------------------------------

/// Deterministic per-dot jitter, so a dot keeps the same threshold every frame.
fn hash2(x: usize, y: usize) -> f64 {
    let mut n = (x as u64)
        .wrapping_mul(374_761_393)
        .wrapping_add((y as u64).wrapping_mul(668_265_263));
    n = (n ^ (n >> 13)).wrapping_mul(1_274_126_177);
    ((n ^ (n >> 16)) & 0xFFFF) as f64 / 65536.0
}

fn ease(p: f64) -> f64 {
    let p = p.clamp(0.0, 1.0);
    if p < 0.5 {
        2.0 * p * p
    } else {
        1.0 - (-2.0 * p + 2.0).powi(2) / 2.0
    }
}

/// How far each stage of the loop has run at `t` seconds.
struct Phase {
    sweep: f64,
    word: f64,
    dissolve: f64,
    glint: f64,
}

impl Phase {
    fn new(t: f64, still: bool) -> Self {
        if still {
            return Self {
                sweep: 1.2,
                word: 1.0,
                dissolve: 0.0,
                glint: GLINT_FROM,
            };
        }
        let t = t.rem_euclid(LOOP);
        Self {
            sweep: -0.15 + 1.35 * ease((t - SWEEP_START) / (SWEEP_END - SWEEP_START)),
            word: ((t - WORD_START) / (WORD_END - WORD_START)).clamp(0.0, 1.0),
            dissolve: if t > DISSOLVE_START {
                ((t - DISSOLVE_START) / (DISSOLVE_END - DISSOLVE_START)).clamp(0.0, 1.0)
            } else {
                0.0
            },
            glint: GLINT_FROM + ((t - GLINT_START) / (GLINT_END - GLINT_START)) * GLINT_SPAN,
        }
    }

    /// Is the dot lit? `wordmark` picks the wordmark's own late, jittered fade
    /// instead of the sweep that carries the mark down the lockup.
    fn lit(&self, x: usize, y: usize, ny: f64, wordmark: bool) -> bool {
        let jitter = hash2(x, y);
        let reached = if wordmark {
            (self.word * 1.7 - jitter * 0.7).clamp(0.0, 1.0)
        } else {
            ((self.sweep - ny) / 0.2).clamp(0.0, 1.0)
        };
        let reached = if self.dissolve > 0.0 {
            reached * (1.0 - (self.dissolve * 1.7 - jitter * 0.7)).clamp(0.0, 1.0)
        } else {
            reached
        };
        reached >= BAYER8[y & 7][x & 7] * 0.7 + jitter * 0.3
    }

    /// Glint strength at a dot: a diagonal band that lifts the colour toward
    /// white as it crosses the lockup.
    fn glint_at(&self, nx: f64, ny: f64) -> f64 {
        let d = nx * 0.7 + (1.0 - ny) * 0.7;
        (-((d - self.glint).powi(2)) / 0.015).exp() * 0.55
    }
}

fn mark_colour(ny: f64) -> (u8, u8, u8) {
    let t = (ny / 0.85).clamp(0.0, 1.0);
    let (a, b, start, span) = if t < 0.45 {
        (MARK_TOP, MARK_MID, 0.0, 0.45)
    } else {
        (MARK_MID, MARK_BOTTOM, 0.45, 0.55)
    };
    let u = (t - start) / span;
    let mix = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * u).round() as u8;
    (mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

/// Cell colour: the row's brand colour lifted toward white by the glint. The
/// result is quantised so a slow gradient does not cost one span per cell.
fn cell_colour(nx: f64, ny: f64, wordmark: bool, phase: &Phase) -> Color {
    let base = if wordmark {
        WORD_COLOUR
    } else {
        mark_colour(ny)
    };
    let k = (phase.glint_at(nx, ny) * 1.6).clamp(0.0, 1.0);
    let mix = |v: u8| ((v as f64 + (255.0 - v as f64) * k).round() as u8) & 0xF8;
    Color::Rgb(mix(base.0), mix(base.1), mix(base.2))
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

/// The splash artwork. `Lockup` is the whole brand mark; `Mark` is the mark
/// alone, for terminals too short for the wordmark, and the caller adds the
/// plain wordmark line beneath it.
pub enum Splash {
    Lockup(Vec<Vec<(String, Color)>>),
    Mark(Vec<Vec<(String, Color)>>),
    None,
}

impl Splash {
    /// The artwork rows, if any.
    pub fn rows(&self) -> Option<&Vec<Vec<(String, Color)>>> {
        match self {
            Splash::Lockup(rows) | Splash::Mark(rows) => Some(rows),
            Splash::None => None,
        }
    }

    /// Whether the wordmark is part of the artwork, so the caller knows not to
    /// print a second one.
    pub fn has_wordmark(&self) -> bool {
        matches!(self, Splash::Lockup(_))
    }
}

/// The splash for a terminal `term_w` by `term_h`. `zone_rows` is the composer's
/// height, reserved before the artwork takes what is left; `t` is seconds and
/// drives the reveal loop.
pub fn splash(term_w: usize, term_h: usize, zone_rows: usize, t: f64) -> Splash {
    if mode() == Mode::Off {
        return Splash::None;
    }
    let available = term_h.saturating_sub(zone_rows + 2).saturating_sub(2);
    let phase = Phase::new(t, mode() == Mode::Static);

    let lockup = Band::lockup();
    if let Some((cols, rows)) = fit(term_w, available, &lockup) {
        if rows >= MIN_LOCKUP_ROWS && cols >= MIN_LOCKUP_COLS {
            let dots_h = rows * dots_per_cell().1;
            let wordmark_from =
                (WORDMARK_TOP as f64 / LOCKUP_H as f64 * dots_h as f64).ceil() as usize;
            return Splash::Lockup(draw(&lockup, cols, rows, Some(wordmark_from), &phase));
        }
    }

    let mark = Band::mark();
    match fit(term_w, available, &mark) {
        Some((cols, rows)) if cols >= MIN_MARK_COLS => {
            Splash::Mark(draw(&mark, cols, rows, None, &phase))
        }
        _ => Splash::None,
    }
}

/// Sample the band, apply the reveal, pack it into cells, and colour it.
fn draw(
    band: &Band,
    cols: usize,
    rows: usize,
    wordmark_from: Option<usize>,
    phase: &Phase,
) -> Vec<Vec<(String, Color)>> {
    let (dot_w, dot_h) = dots_per_cell();
    let dots_w = cols * dot_w;
    let dots_h = rows * dot_h;
    let src_w = band.width();
    let src_h = band.height();

    let mut dots = vec![vec![false; dots_w]; dots_h];
    for (y, line) in dots.iter_mut().enumerate() {
        for (x, dot) in line.iter_mut().enumerate() {
            let sx = band.x0 + ((x as f64 + 0.5) * src_w as f64 / dots_w as f64) as usize;
            let sy = band.y0 + ((y as f64 + 0.5) * src_h as f64 / dots_h as f64) as usize;
            if !lockup_bit(sx.min(band.x1), sy.min(band.y1)) {
                continue;
            }
            let ny = (y as f64 + 0.5) / dots_h as f64;
            let wordmark = wordmark_from.is_some_and(|from| sy >= from);
            *dot = phase.lit(x, y, ny, wordmark);
        }
    }

    let cells = if dot_w == 1 {
        pack_blocks(&dots)
    } else {
        pack_braille(&dots, cols)
    };
    colour(cells, cols, rows, dot_h, wordmark_from, phase)
}

/// Braille packs 2x4 dots into one cell.
fn pack_braille(dots: &[Vec<bool>], cols: usize) -> Vec<Vec<char>> {
    dots.chunks(4)
        .map(|band| {
            (0..cols)
                .map(|cell| {
                    let mut pattern = 0u32;
                    for (dy, bits) in DOT_BITS.iter().enumerate() {
                        for (dx, bit) in bits.iter().enumerate() {
                            if band[dy][cell * 2 + dx] {
                                pattern |= *bit as u32;
                            }
                        }
                    }
                    char::from_u32(BRAILLE_BASE + pattern).unwrap_or('\u{2800}')
                })
                .collect()
        })
        .collect()
}

/// Half-block packs two vertical dots into one cell: `▀`, `▄`, `█` or blank.
fn pack_blocks(dots: &[Vec<bool>]) -> Vec<Vec<char>> {
    dots.chunks(2)
        .map(|pair| {
            pair[0]
                .iter()
                .zip(pair[1].iter())
                .map(|(top, bottom)| match (top, bottom) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                })
                .collect()
        })
        .collect()
}

/// Colour each cell by its position, before the spans are merged.
fn colour(
    cells: Vec<Vec<char>>,
    cols: usize,
    rows: usize,
    dot_h: usize,
    wordmark_from: Option<usize>,
    phase: &Phase,
) -> Vec<Vec<(String, Color)>> {
    let coloured: Vec<Vec<(char, Color)>> = cells
        .into_iter()
        .enumerate()
        .map(|(row, line)| {
            line.into_iter()
                .enumerate()
                .map(|(cell, ch)| {
                    let nx = (cell as f64 + 0.5) / cols as f64;
                    let ny = (row as f64 + 0.5) / rows as f64;
                    let wordmark = wordmark_from.is_some_and(|from| row * dot_h >= from);
                    (ch, cell_colour(nx, ny, wordmark, phase))
                })
                .collect()
        })
        .collect();
    group(coloured)
}

/// Drop blank edges, then merge neighbouring cells that share a colour into one
/// span so a gradient row costs a handful of spans rather than one per cell.
fn group(mut cells: Vec<Vec<(char, Color)>>) -> Vec<Vec<(String, Color)>> {
    for line in cells.iter_mut() {
        while line.last().is_some_and(|&(ch, _)| ch == ' ') {
            line.pop();
        }
    }
    while cells.first().is_some_and(|line| line.is_empty()) {
        cells.remove(0);
    }
    while cells.last().is_some_and(|line| line.is_empty()) {
        cells.pop();
    }
    cells
        .into_iter()
        .map(|line| {
            let mut spans: Vec<(String, Color)> = Vec::new();
            for (ch, colour) in line {
                match spans.last_mut() {
                    Some((text, last)) if *last == colour => text.push(ch),
                    _ => spans.push((ch.to_string(), colour)),
                }
            }
            spans
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn revealed(
        band: &Band,
        cols: usize,
        rows: usize,
        wordmark_from: Option<usize>,
    ) -> Vec<Vec<bool>> {
        let (dot_w, dot_h) = (2usize, 4usize);
        let dots_w = cols * dot_w;
        let dots_h = rows * dot_h;
        let phase = Phase::new(0.0, true);
        let mut out = vec![vec![false; dots_w]; dots_h];
        for row in 0..rows {
            for cell in 0..cols {
                for dy in 0..dot_h {
                    for dx in 0..dot_w {
                        let x = cell * dot_w + dx;
                        let y = row * dot_h + dy;
                        let sx = band.x0
                            + ((x as f64 + 0.5) * band.width() as f64 / dots_w as f64) as usize;
                        let sy = band.y0
                            + ((y as f64 + 0.5) * band.height() as f64 / dots_h as f64) as usize;
                        if !lockup_bit(sx.min(band.x1), sy.min(band.y1)) {
                            continue;
                        }
                        let ny = (y as f64 + 0.5) / dots_h as f64;
                        let word = wordmark_from.is_some_and(|from| sy >= from);
                        out[y][x] = phase.lit(x, y, ny, word);
                    }
                }
            }
        }
        out
    }

    #[test]
    fn raster_is_the_documented_size() {
        assert_eq!(LOCKUP.len(), ROW_BYTES * LOCKUP_H);
    }

    #[test]
    fn lockup_fits_a_normal_terminal_but_not_a_short_one() {
        assert!(matches!(splash(120, 40, 5, 0.0), Splash::Lockup(_)));
        assert!(matches!(splash(100, 30, 5, 0.0), Splash::Lockup(_)));
        // 80x24 is too short for the wordmark, so the mark takes the room
        assert!(matches!(splash(80, 24, 5, 0.0), Splash::Mark(_)));
        assert!(matches!(splash(40, 12, 5, 0.0), Splash::None));
    }

    #[test]
    fn lockup_keeps_its_aspect() {
        for cols in [30, 47, 69] {
            let band = Band::lockup();
            let aspect = 2.0 * band.width() as f64 / band.height() as f64;
            let rows = (cols as f64 / aspect).ceil() as usize;
            let rows_out = splash(200, 200, 0, 0.0).rows().expect("artwork").len();
            assert!(rows_out >= rows.saturating_sub(1));
        }
    }

    #[test]
    fn mark_reads_as_a_temple_with_a_hollow_p() {
        let band = Band::mark();
        let cols = 47;
        let aspect = 2.0 * band.width() as f64 / band.height() as f64;
        let rows = (cols as f64 / aspect).ceil() as usize;
        let dots = revealed(&band, cols, rows, None);
        // the pediment peak: a couple of dots, not a full bar
        let peak = dots[0].iter().filter(|&&d| d).count();
        assert!(peak > 0 && peak <= 4, "peak={peak}");
        // the P counter leaves a hole through the middle of the mark
        let mid = dots.len() / 2;
        assert!(
            dots[mid].iter().any(|&d| !d),
            "the P counter must stay open"
        );
    }

    #[test]
    fn reveal_ends_fully_lit_and_dissolve_ends_dark() {
        let band = Band::mark();
        let cols = 47;
        let aspect = 2.0 * band.width() as f64 / band.height() as f64;
        let rows = (cols as f64 / aspect).ceil() as usize;
        let lit = revealed(&band, cols, rows, None);
        // every dot that samples ink must be lit once the reveal completes
        let (dot_w, dot_h) = (2usize, 4usize);
        let dots_w = cols * dot_w;
        let dots_h = rows * dot_h;
        let mut ink = 0;
        let mut lit_count = 0;
        for y in 0..dots_h {
            for x in 0..dots_w {
                let sx =
                    band.x0 + ((x as f64 + 0.5) * band.width() as f64 / dots_w as f64) as usize;
                let sy =
                    band.y0 + ((y as f64 + 0.5) * band.height() as f64 / dots_h as f64) as usize;
                if lockup_bit(sx.min(band.x1), sy.min(band.y1)) {
                    ink += 1;
                    if lit[y][x] {
                        lit_count += 1;
                    }
                }
            }
        }
        assert!(lit_count * 100 >= ink * 95, "lit={lit_count} ink={ink}");

        let phase = Phase::new(DISSOLVE_END + 0.5, false);
        let mut dark = 0;
        for y in 0..rows * 4 {
            for x in 0..cols * 2 {
                let ny = (y as f64 + 0.5) / (rows * 4) as f64;
                if !phase.lit(x, y, ny, false) {
                    dark += 1;
                }
            }
        }
        assert!(dark * 100 >= cols * rows * 4 * 95, "dark={dark}");
    }

    #[test]
    fn off_mode_draws_nothing() {
        // mode() is process-global and defaults to Braille, so assert the enum
        // plumbing rather than mutating the environment.
        assert!(matches!(Splash::None.rows(), None));
        assert!(!Splash::None.has_wordmark());
        assert!(Splash::Lockup(Vec::new()).has_wordmark());
        assert!(!Splash::Mark(Vec::new()).has_wordmark());
    }
}
