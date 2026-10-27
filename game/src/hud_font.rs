// SPDX-License-Identifier: GPL-2.0-or-later
//! The HUD font: Kenney Pixel with its K put right. The shipped bitmap draws K
//! as an H with a short crossbar, which at HUD size reads "SKIP" as "SHIP".

use psx_font::fonts::kenney_pixel::{KENNEY_PIXEL, KENNEY_PIXEL_BITMAP};
use psx_font::BitmapFont;

/// A proper K in the font's own 6-wide cell: rows 1 to 7, bit `i` the column `i`.
const K_ROWS: [u8; 7] = [0x22, 0x12, 0x0a, 0x06, 0x0a, 0x12, 0x22];
/// Bytes a glyph takes: 11 rows of 2.
const STRIDE: usize = 22;
/// Glyph index of K from the space at U+0020.
const K_INDEX: usize = 0x4b - 0x20;

const fn patched() -> [u8; KENNEY_PIXEL_BITMAP.len()] {
    let mut bitmap = KENNEY_PIXEL_BITMAP;
    let mut row = 0;
    while row < K_ROWS.len() {
        bitmap[K_INDEX * STRIDE + 2 * (row + 1)] = K_ROWS[row];
        bitmap[K_INDEX * STRIDE + 2 * (row + 1) + 1] = 0;
        row += 1;
    }
    bitmap
}

static BITMAP: [u8; KENNEY_PIXEL_BITMAP.len()] = patched();

/// Kenney Pixel with the corrected K.
pub static HUD_FONT: BitmapFont = BitmapFont {
    bitmap: &BITMAP,
    ..KENNEY_PIXEL
};
