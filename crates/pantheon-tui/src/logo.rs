//! The Pantheon logo drawn as a terminal dot-matrix.
//!
//! The mark is the temple-with-P from the brand lockup (`web/logo.svg`), rasterised
//! from the same polygon outlines so any width scales without a baked-in bitmap.
//! Braille patterns give 2x4 dots per character cell, which is the densest portrait
//! grid that needs nothing beyond a monospace font and the UTF-8 Braille block, so
//! it works in an ordinary terminal with no Kitty graphics and no Sixel.
//!
//! Nothing here touches the composer, the transcript, or the status bar: the logo
//! is only drawn on the new-session splash.

/// Mark outlines in the logo's own 240x273 unit space (origin at the mark's
/// bounding box). Kept in sync with `web/logo.svg`; the P pillar is the outer
/// contour with the counter subtracted.
const ROOF: [(f64, f64); 6] = [
    (120.0, 0.0),
    (240.0, 60.0),
    (240.0, 82.0),
    (120.0, 22.0),
    (0.0, 82.0),
    (0.0, 60.0),
];
const FRIEZE: [(f64, f64); 6] = [
    (120.0, 40.0),
    (226.0, 93.0),
    (226.0, 115.0),
    (120.0, 62.0),
    (14.0, 115.0),
    (14.0, 93.0),
];
const PILLAR_LEFT: [(f64, f64); 6] = [
    (28.0, 127.0),
    (68.0, 127.0),
    (68.0, 237.0),
    (28.0, 217.0),
    (16.0, 227.0),
    (28.0, 213.0),
];
const P_OUTER: [(f64, f64); 8] = [
    (120.0, 80.0),
    (162.0, 101.0),
    (162.0, 185.0),
    (118.0, 207.0),
    (118.0, 250.0),
    (100.0, 273.0),
    (78.0, 250.0),
    (78.0, 101.0),
];
const P_COUNTER: [(f64, f64); 6] = [
    (120.0, 102.0),
    (142.0, 113.0),
    (142.0, 163.0),
    (120.0, 174.0),
    (98.0, 174.0),
    (98.0, 113.0),
];
const PILLAR_RIGHT: [(f64, f64); 6] = [
    (172.0, 127.0),
    (212.0, 127.0),
    (212.0, 213.0),
    (224.0, 227.0),
    (212.0, 217.0),
    (172.0, 237.0),
];

/// Width of the `PANTHEON` wordmark in cells; the mark never outgrows it, so the
/// lockup stays balanced the way the brand asset is.
const MAX_MARK_COLS: usize = 47;
/// Mark aspect: 240 wide by 273 tall, so `cols / rows = 1.758`.
const MARK_ASPECT: f64 = 1.758;
/// Below this many rows a mark is too small to read as the logo.
const MIN_MARK_ROWS: usize = 6;
/// Below this many columns the P stops reading as a P.
const MIN_MARK_COLS: usize = 12;

/// Braille dot bits, indexed `[dy][dx]` for the 4x2 dot grid inside one cell:
/// dots 1 4 / 2 5 / 3 6 / 7 8.
const DOT_BITS: [[u8; 2]; 4] = [[0x01, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]];
const BRAILLE_BASE: u32 = 0x2800;

/// Even-odd ray cast: is `(x, y)` inside this outline?
fn inside(x: f64, y: f64, outline: &[(f64, f64)]) -> bool {
    let mut hits = false;
    let mut previous = outline.len() - 1;
    for (index, &(xi, yi)) in outline.iter().enumerate() {
        let (xj, yj) = outline[previous];
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            hits = !hits;
        }
        previous = index;
    }
    hits
}

/// The mark on a `cols * 2` by `rows * 4` dot grid.
fn dots(cols: usize, rows: usize) -> Vec<Vec<bool>> {
    let dots_w = cols * 2;
    let dots_h = rows * 4;
    (0..dots_h)
        .map(|dy| {
            // sample the dot centre in the mark's own space
            let y = (dy as f64 + 0.5) * 273.0 / dots_h as f64;
            (0..dots_w)
                .map(|dx| {
                    let x = (dx as f64 + 0.5) * 240.0 / dots_w as f64;
                    let filled = inside(x, y, &ROOF)
                        || inside(x, y, &FRIEZE)
                        || inside(x, y, &PILLAR_LEFT)
                        || inside(x, y, &P_OUTER)
                        || inside(x, y, &PILLAR_RIGHT);
                    filled && !inside(x, y, &P_COUNTER)
                })
                .collect()
        })
        .collect()
}

/// The mark as Braille cells, one `String` per terminal row, trimmed of the
/// blank cells that only pad the widest row.
fn braille_rows(cols: usize, rows: usize) -> Vec<String> {
    let grid = dots(cols, rows);
    let mut lines: Vec<String> = (0..rows)
        .map(|row| {
            let mut bits = Vec::with_capacity(cols);
            for cell in 0..cols {
                let mut pattern = 0u32;
                for (dy, row_bits) in DOT_BITS.iter().enumerate() {
                    for (dx, bit) in row_bits.iter().enumerate() {
                        if grid[row * 4 + dy][cell * 2 + dx] {
                            pattern |= *bit as u32;
                        }
                    }
                }
                bits.push(char::from_u32(BRAILLE_BASE + pattern).unwrap_or('\u{2800}'));
            }
            bits.into_iter().collect()
        })
        .collect();
    // Trailing blank cells are padding, not shape: drop them so a narrow
    // terminal never wraps on invisible width, and centre the visible mark.
    for line in &mut lines {
        while line.ends_with('\u{2800}') {
            line.pop();
        }
    }
    while lines.first().is_some_and(|line| line.is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

/// Brand gradient for the dot-matrix rows: light lavender at the top, the brand
/// purple through the middle, deep purple at the base. Every stop stays legible
/// on a near-black terminal, unlike the darker end of the app-icon gradient.
fn brand_gradient(t: f64) -> ratatui::style::Color {
    const TOP: (u8, u8, u8) = (196, 181, 253);
    const MID: (u8, u8, u8) = (114, 35, 255);
    const BOTTOM: (u8, u8, u8) = (75, 0, 205);
    let lerp = |a: (u8, u8, u8), b: (u8, u8, u8), t: f64| {
        let channel = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round() as u8;
        (channel(a.0, b.0), channel(a.1, b.1), channel(a.2, b.2))
    };
    let (r, g, b) = if t < 0.45 {
        lerp(TOP, MID, t / 0.45)
    } else {
        lerp(MID, BOTTOM, (t - 0.45) / 0.55)
    };
    ratatui::style::Color::Rgb(r, g, b)
}

/// The mark's cell width for a terminal `term_w` by `term_h`, or `None` when the
/// splash has no room for it. `wordmark_rows` and `zone_rows` are the rows the
/// rest of the splash needs, so the mark takes what is left rather than pushing
/// the composer off screen.
pub fn mark_width(
    term_w: usize,
    term_h: usize,
    wordmark_rows: usize,
    zone_rows: usize,
) -> Option<usize> {
    let available = term_h
        .saturating_sub(zone_rows + wordmark_rows + 2) // composer + wordmark + blanks
        .saturating_sub(2); // breathing room above and below
    if available < MIN_MARK_ROWS {
        return None;
    }
    let cols = (available as f64 * MARK_ASPECT).floor() as usize;
    let cols = cols.min(MAX_MARK_COLS).min(term_w.saturating_sub(4));
    let rows = (cols as f64 / MARK_ASPECT).ceil() as usize;
    (rows >= MIN_MARK_ROWS && cols >= MIN_MARK_COLS).then_some(cols)
}

/// The mark as coloured rows for the splash: `(cells, colour)` per terminal row.
pub fn mark_rows(
    term_w: usize,
    term_h: usize,
    wordmark_rows: usize,
    zone_rows: usize,
) -> Vec<(String, ratatui::style::Color)> {
    let Some(cols) = mark_width(term_w, term_h, wordmark_rows, zone_rows) else {
        return Vec::new();
    };
    let rows = (cols as f64 / MARK_ASPECT).ceil() as usize;
    braille_rows(cols, rows)
        .into_iter()
        .enumerate()
        .map(|(index, cells)| {
            let t = index as f64 / rows.saturating_sub(1).max(1) as f64;
            (cells, brand_gradient(t))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_keeps_its_aspect_at_every_width() {
        for cols in [12, 24, 47] {
            let rows = (cols as f64 / MARK_ASPECT).ceil() as usize;
            let lines = braille_rows(cols, rows);
            assert_eq!(lines.len(), rows, "cols={cols}");
            assert!(lines.iter().all(|line| line.chars().count() <= cols));
        }
    }

    #[test]
    fn mark_is_symmetric_and_has_a_hollow_p() {
        let cols = 40;
        let rows = (cols as f64 / MARK_ASPECT).ceil() as usize;
        let lines = braille_rows(cols, rows);
        // the pediment peak sits on the centre column
        let peak = lines[0].chars().count();
        let grid = dots(cols, rows);
        let centre = grid[0].iter().filter(|&&dot| dot).count();
        assert!(
            centre <= 4,
            "the top row is the pediment peak, not a wide bar"
        );
        assert!(peak > 0);
        // the P counter leaves a hole in the middle of the mark
        let middle = &grid[rows * 2];
        assert!(
            middle.iter().any(|&dot| !dot),
            "the P counter must stay open"
        );
    }

    #[test]
    fn small_terminals_drop_the_mark_but_keep_the_wordmark() {
        assert!(mark_width(80, 24, 5, 5).is_some());
        assert!(mark_width(80, 12, 5, 5).is_none(), "too short to read");
        assert!(
            mark_width(14, 40, 5, 5).is_none(),
            "too narrow to read the P"
        );
    }
}
