// SPDX-License-Identifier: GPL-2.0-or-later
//! Where a string's ink falls, for placing numbers so they look centred.
//!
//! A bitmap font's cell has room for what the font might draw: the digits sit
//! in the middle of a line that also holds ascenders and descenders, and each
//! glyph's advance runs past its last lit column. Placing text by the cell or the
//! advance puts the digits high or low and a "1" off to the side. This measures
//! the lit pixels instead, with the drop shadow left out.

/// The box of lit pixels of a run of text, relative to where its pen starts (the
/// `x` and `y` handed to the font's draw call). `right` and `bottom` are one past
/// the last lit column and row.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Ink {
    pub left: i32,
    pub right: i32,
    pub top: i32,
    pub bottom: i32,
}

impl Ink {
    /// Twice the box's centre, so the half pixel survives: `(x, y)`.
    pub const fn centre2(&self) -> (i32, i32) {
        (self.left + self.right, self.top + self.bottom)
    }
}

/// The ink of `text` set in a font of `glyph_h`-row cells whose glyph `ch`
/// advances the pen by `advance(ch)` and whose row `r` is `row(ch, r)`, one bit
/// a column from bit 0. `None` if nothing is lit.
pub fn ink_of(
    text: &str,
    glyph_h: u8,
    advance: impl Fn(char) -> i32,
    row: impl Fn(char, u8) -> u32,
) -> Option<Ink> {
    let mut ink: Option<Ink> = None;
    let mut pen = 0;
    for ch in text.chars() {
        for r in 0..glyph_h {
            let bits = row(ch, r);
            if bits == 0 {
                continue;
            }
            let (lo, hi) = (
                bits.trailing_zeros() as i32,
                32 - bits.leading_zeros() as i32,
            );
            let b = ink.get_or_insert(Ink {
                left: pen + lo,
                right: pen + hi,
                top: r as i32,
                bottom: r as i32 + 1,
            });
            b.left = b.left.min(pen + lo);
            b.right = b.right.max(pen + hi);
            b.top = b.top.min(r as i32);
            b.bottom = b.bottom.max(r as i32 + 1);
        }
        pen += advance(ch);
    }
    ink
}

/// Where to draw `text` so the middle of its ink is `(cx2, cy2) / 2`: the pen
/// position `(x, y)`.
pub const fn place(ink: Ink, cx2: i32, cy2: i32) -> (i32, i32) {
    let (w2, h2) = ink.centre2();
    ((cx2 - w2).div_euclid(2), (cy2 - h2).div_euclid(2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use psx_font::fonts::{KENNEY_PIXEL, KENNEY_ROCKET};
    use psx_font::BitmapFont;

    fn ink_in(font: &BitmapFont, text: &str) -> Option<Ink> {
        ink_of(
            text,
            font.glyph_h,
            |c| i32::from(font.glyph_advance(c)),
            |c, r| {
                font.glyph_index(c)
                    .map_or(0, |i| font.glyph_row_packed(i, r))
            },
        )
    }

    /// Every string the HUD puts in a panel: the numbers 0 to 99 (scores, the
    /// boost count, the goal target), every clock the match can show, and an
    /// overtime clock.
    fn hud_strings() -> std::vec::Vec<std::string::String> {
        let mut all = std::vec::Vec::new();
        for n in 0..=99 {
            all.push(std::format!("{n}"));
            all.push(std::format!("TO {n}"));
        }
        for m in 0..=9 {
            for ss in 0..60 {
                all.push(std::format!("{m}:{ss:02}"));
                all.push(std::format!("+{m}:{ss:02}"));
            }
        }
        all
    }

    /// For each string, at 1x and 2x, in panels of every height the HUD has and
    /// some it does not, the ink's middle is within a pixel of the panel's both
    /// ways, and the gap above the digits and the gap below them (the drop
    /// shadow excluded) differ by at most one pixel.
    #[test]
    fn numbers_are_centred_in_their_panels() {
        for font in [&KENNEY_PIXEL, &KENNEY_ROCKET] {
            for scale in [1, 2] {
                for text in hud_strings() {
                    let one = ink_in(font, &text).unwrap();
                    let ink = Ink {
                        left: one.left * scale,
                        right: one.right * scale,
                        top: one.top * scale,
                        bottom: one.bottom * scale,
                    };
                    for h in (ink.bottom - ink.top + 2)..=40 {
                        for cx2 in [200, 201, 320, 321] {
                            let (x, y) = place(ink, cx2, h);
                            let (mid_x2, mid_y2) =
                                (2 * x + ink.left + ink.right, 2 * y + ink.top + ink.bottom);
                            assert!(
                                (mid_x2 - cx2).abs() <= 2,
                                "{text:?} x: ink middle {mid_x2} for {cx2}"
                            );
                            assert!(
                                (mid_y2 - h).abs() <= 2,
                                "{text:?} y: ink middle {mid_y2} for panel {h}"
                            );
                            let (top_gap, bottom_gap) = (y + ink.top, h - (y + ink.bottom));
                            assert!(
                                (top_gap - bottom_gap).abs() <= 1,
                                "{text:?} in a panel {h} high: gaps {top_gap} above, {bottom_gap} below"
                            );
                        }
                    }
                }
            }
        }
    }

    /// The old placement, by the line cell and the advance, is what this
    /// replaces: it leaves the digits high in the HUD font, which is the bug.
    #[test]
    fn the_cell_is_not_the_ink() {
        let ink = ink_in(&KENNEY_PIXEL, "12").unwrap();
        let h = 13;
        let cell_top = (h - i32::from(KENNEY_PIXEL.line_height)) / 2;
        let (top_gap, bottom_gap) = (cell_top + ink.top, h - (cell_top + ink.bottom));
        assert!(bottom_gap - top_gap >= 2, "gaps {top_gap} / {bottom_gap}");
    }
}
